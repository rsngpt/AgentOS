//! CLI-side presentation over the shared `agentos-client` transport.

use std::io::Write as _;

use agentos_core::SandboxSpec;
use serde_json::{json, Value};

pub use agentos_client::unary;

/// Run a sandbox, streaming its output to our stdio.
/// Returns the process exit code to use.
pub async fn run(spec: SandboxSpec) -> Result<i32, String> {
    let mut stream = agentos_client::open_stream(
        "sandbox.run",
        serde_json::to_value(&spec).map_err(|e| e.to_string())?,
    )
    .await?;

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    while let Some(v) = stream.next().await? {
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

/// Stream daemon events to stdout as JSON lines until interrupted.
pub async fn events() -> Result<i32, String> {
    let mut stream = agentos_client::open_stream("events.subscribe", json!(null)).await?;
    while let Some(v) = stream.next().await? {
        println!("{v}");
    }
    Ok(0)
}
