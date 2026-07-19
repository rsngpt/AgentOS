//! Client for the `agentosd` Unix socket, shared by the CLI and the GUI.
//!
//! Wire format: one JSON object per line. Unary methods get one response
//! line; streaming methods (`sandbox.run`, `events.subscribe`) dedicate the
//! connection and yield a line per event.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;

fn socket_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME not set")).join(".agentos/agentosd.sock")
}

/// Connect to the daemon, spawning `agentosd` (a sibling binary of the
/// current executable) if it is not running yet.
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
    let log_dir = path.parent().expect("socket has parent");
    std::fs::create_dir_all(log_dir).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(log_dir.join("agentosd.log")).map_err(|e| e.to_string())?;
    std::process::Command::new(&daemon)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            log.try_clone().map_err(|e| e.to_string())?,
        ))
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

async fn send_request(stream: &mut UnixStream, method: &str, params: &Value) -> Result<(), String> {
    let mut line =
        serde_json::to_vec(&json!({ "id": 1, "method": method, "params": params }))
            .map_err(|e| e.to_string())?;
    line.push(b'\n');
    stream.write_all(&line).await.map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())
}

/// One-shot request/response (e.g. `sandbox.list`, `sandbox.kill`).
pub async fn unary(method: &str, params: Value) -> Result<Value, String> {
    let mut stream = connect().await?;
    send_request(&mut stream, method, &params).await?;
    let (read, _keep_write) = stream.into_split();
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

/// A dedicated streaming connection; `next()` yields one JSON value per line.
pub struct EventStream {
    lines: Lines<BufReader<OwnedReadHalf>>,
    // Keep the write half open: dropping it would half-close the socket and
    // some daemons treat that as client-gone.
    _write: OwnedWriteHalf,
}

impl EventStream {
    pub async fn next(&mut self) -> Result<Option<Value>, String> {
        match self.lines.next_line().await.map_err(|e| e.to_string())? {
            None => Ok(None),
            Some(line) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| e.to_string()),
        }
    }
}

/// Open a streaming method (`sandbox.run` with a spec, or `events.subscribe`).
pub async fn open_stream(method: &str, params: Value) -> Result<EventStream, String> {
    let mut stream = connect().await?;
    send_request(&mut stream, method, &params).await?;
    let (read, write) = stream.into_split();
    Ok(EventStream {
        lines: BufReader::new(read).lines(),
        _write: write,
    })
}
