//! CLI presentation on top of the `agentos-client` SDK.
//!
//! This file is deliberately thin: if driving Agent OS from the CLI needs
//! anything the SDK can't express, that's a gap an embedder would hit too.

use std::io::Write as _;

use agentos_client::{Client, RunEvent};
use agentos_core::{SandboxId, SandboxSpec};

fn client() -> Client {
    Client::new()
}

/// Parse a user-supplied sandbox id.
pub fn parse_id(id: &str) -> Result<SandboxId, String> {
    serde_json::from_value(serde_json::Value::String(id.to_string()))
        .map_err(|_| format!("not a valid sandbox id: {id}"))
}

/// Run a sandbox, streaming its output to our stdio; returns the exit code.
pub async fn run(spec: SandboxSpec) -> Result<i32, String> {
    let stream = client().run(&spec).await.map_err(|e| e.to_string())?;
    stream_run(stream).await
}

/// Restore a snapshotted sandbox and stream it exactly like `run`.
pub async fn restore(id: &str) -> Result<i32, String> {
    let id = parse_id(id)?;
    let stream = client().restore(&id).await.map_err(|e| e.to_string())?;
    stream_run(stream).await
}

async fn stream_run(mut stream: agentos_client::RunStream) -> Result<i32, String> {
    // Forward our stdin to the sandbox so interactive agents work. Runs
    // concurrently with the output loop, and is aborted when the run ends so
    // a blocked read on a terminal doesn't keep the process alive.
    let stdin_task = stream.stdin().map(|mut sender| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut input = tokio::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                match input.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sender.send(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                }
            }
            // Our stdin hit EOF: close the guest's too, so `cat`-style
            // commands finish instead of waiting forever.
            sender.close().await.ok();
        })
    });

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    loop {
        let event = stream.next().await.map_err(|e| e.to_string())?;
        let Some(event) = event else {
            return Err("daemon connection closed unexpectedly".into());
        };
        match event {
            RunEvent::Created(id) => eprintln!("sandbox {id} created"),
            RunEvent::Restoring(id) => eprintln!("restoring sandbox {id}"),
            RunEvent::Cloning { url } => eprintln!("cloning {url}"),
            RunEvent::Running | RunEvent::Unknown(_) => {}
            RunEvent::Stdout(data) => {
                stdout.write_all(&data).map_err(|e| e.to_string())?;
                stdout.flush().ok();
            }
            RunEvent::Stderr(data) => {
                stderr.write_all(&data).map_err(|e| e.to_string())?;
                stderr.flush().ok();
            }
            RunEvent::Exited { code, .. } => {
                if let Some(t) = &stdin_task {
                    t.abort();
                }
                return Ok(code.unwrap_or(1));
            }
            RunEvent::Terminated { reason, saved_dir } => {
                eprintln!("sandbox terminated ({reason})");
                if let Some(dir) = saved_dir {
                    eprintln!("sandbox state saved at {dir}");
                }
                if let Some(t) = &stdin_task {
                    t.abort();
                }
                return Ok(137);
            }
            RunEvent::Snapshotted { dir } => {
                eprintln!("sandbox snapshotted; state in {dir}");
                if let Some(t) = &stdin_task {
                    t.abort();
                }
                return Ok(0);
            }
        }
    }
}

pub async fn list() -> Result<i32, String> {
    let rows = client().list().await.map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("no sandboxes");
        return Ok(0);
    }
    println!("{:<38} {:<16} STATE", "ID", "NAME");
    for sb in rows {
        // The state enum is tagged, so its JSON tag is the display name.
        let state = serde_json::to_value(&sb.state)
            .ok()
            .and_then(|v| v["state"].as_str().map(String::from))
            .unwrap_or_else(|| "?".into());
        println!("{:<38} {:<16} {}", sb.id.to_string(), sb.name, state);
    }
    Ok(0)
}

pub async fn kill(id: &str, save: bool) -> Result<i32, String> {
    client()
        .kill(&parse_id(id)?, save)
        .await
        .map_err(|e| e.to_string())?;
    println!("killed");
    Ok(0)
}

/// Panic kill: the newest live sandbox, wiped. Shares `kill_newest_live` with
/// the GUI's global hotkey, so exercising this exercises that path too.
pub async fn kill_newest() -> Result<i32, String> {
    match client().kill_newest_live().await.map_err(|e| e.to_string())? {
        Some(id) => {
            println!("killed {id}");
            Ok(0)
        }
        None => {
            eprintln!("no running sandbox to kill");
            Ok(1)
        }
    }
}

pub async fn pause(id: &str) -> Result<i32, String> {
    client().pause(&parse_id(id)?).await.map_err(|e| e.to_string())?;
    println!("paused");
    Ok(0)
}

pub async fn resume(id: &str) -> Result<i32, String> {
    client().resume(&parse_id(id)?).await.map_err(|e| e.to_string())?;
    println!("resumed");
    Ok(0)
}

pub async fn snapshot(id: &str) -> Result<i32, String> {
    let dir = client()
        .snapshot(&parse_id(id)?)
        .await
        .map_err(|e| e.to_string())?;
    println!("snapshotted; state in {dir}");
    Ok(0)
}

/// Stream the daemon's event bus as JSON lines.
pub async fn events() -> Result<i32, String> {
    let mut sub = client().events().await.map_err(|e| e.to_string())?;
    while let Some(event) = sub.next().await.map_err(|e| e.to_string())? {
        println!("{}", serde_json::to_string(&event).unwrap_or_default());
    }
    Ok(0)
}
