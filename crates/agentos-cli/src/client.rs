//! Client for the agentosd Unix socket, including auto-starting the daemon.

use std::io::Write as _;
use std::path::PathBuf;
use std::time::Duration;

use agentos_core::SandboxSpec;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

fn socket_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME not set")).join(".agentos/agentosd.sock")
}

/// Connect to the daemon, spawning `agentosd` (sibling binary) if it is not
/// running yet.
pub async fn connect() -> Result<UnixStream, String> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path).await {
        return Ok(s);
    }

    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let daemon = exe.with_file_name("agentosd");
    if !daemon.exists() {
        return Err(format!(
            "daemon not running and {} not found; start agentosd manually",
            daemon.display()
        ));
    }
    let log = std::fs::File::create(
        path.parent()
            .map(|d| {
                std::fs::create_dir_all(d).ok();
                d.join("agentosd.log")
            })
            .unwrap(),
    )
    .map_err(|e| e.to_string())?;
    std::process::Command::new(&daemon)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone().map_err(|e| e.to_string())?))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", daemon.display()))?;

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(s) = UnixStream::connect(&path).await {
            return Ok(s);
        }
    }
    Err("daemon did not come up within 5s (see ~/.agentos/agentosd.log)".into())
}

async fn send_line(stream: &mut UnixStream, v: &Value) -> Result<(), String> {
    let mut line = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    line.push(b'\n');
    stream.write_all(&line).await.map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())
}

/// Run a sandbox, streaming its output to our stdio.
/// Returns the process exit code to use.
pub async fn run(spec: SandboxSpec) -> Result<i32, String> {
    let mut stream = connect().await?;
    send_line(
        &mut stream,
        &json!({ "id": 1, "method": "sandbox.run", "params": spec }),
    )
    .await?;

    let (read, _write) = stream.split();
    let mut lines = BufReader::new(read).lines();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();

    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
        if let Some(err) = v.get("error") {
            return Err(err["message"].as_str().unwrap_or("unknown error").to_string());
        }
        match v["event"].as_str() {
            Some("created") => {
                eprintln!("sandbox {} created", v["id"].as_str().unwrap_or("?"));
            }
            Some("running") => {}
            Some("stdout") => {
                stdout.write_all(&bytes(&v["data"])).map_err(|e| e.to_string())?;
                stdout.flush().ok();
            }
            Some("stderr") => {
                stderr.write_all(&bytes(&v["data"])).map_err(|e| e.to_string())?;
                stderr.flush().ok();
            }
            Some("exited") => {
                return Ok(v["code"].as_i64().unwrap_or(1) as i32);
            }
            Some("terminated") => {
                let reason = v["reason"].as_str().unwrap_or("kill switch");
                eprintln!("sandbox terminated ({reason})");
                if let Some(dir) = v["saved_dir"].as_str() {
                    eprintln!("sandbox state saved at {dir}");
                }
                return Ok(137);
            }
            Some("error") => {
                return Err(v["message"].as_str().unwrap_or("unknown error").to_string());
            }
            _ => {}
        }
    }
    Err("daemon connection closed unexpectedly".into())
}

fn bytes(v: &Value) -> Vec<u8> {
    v.as_array()
        .map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect())
        .unwrap_or_default()
}

/// Unary request/response helper for ps/kill.
pub async fn unary(method: &str, params: Value) -> Result<Value, String> {
    let mut stream = connect().await?;
    send_line(&mut stream, &json!({ "id": 1, "method": method, "params": params })).await?;
    let (read, _w) = stream.split();
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("daemon closed connection")?;
    let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if let Some(err) = v.get("error") {
        return Err(err["message"].as_str().unwrap_or("unknown error").to_string());
    }
    Ok(v["result"].clone())
}
