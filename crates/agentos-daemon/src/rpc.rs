//! JSON-RPC-style server over the daemon's Unix socket.
//!
//! One JSON object per line. Two shapes of method:
//! - Unary (`sandbox.list`, `sandbox.kill`): one request line → one response
//!   line, connection handles many in sequence.
//! - Streaming (`sandbox.run`): the connection is dedicated to the run; the
//!   daemon streams `{event: ...}` lines until the sandbox terminates.

use agentos_core::{SandboxId, SandboxSpec, TerminationDisposition};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::registry::Registry;
use crate::run;

#[derive(Debug, Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

pub async fn serve_connection(
    stream: UnixStream,
    registry: Registry,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    while let Some(line) = lines.next_line().await? {
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                respond(&mut write, Value::Null, Err(format!("bad request: {e}"))).await?;
                continue;
            }
        };

        if req.method == "sandbox.run" {
            match serde_json::from_value::<SandboxSpec>(req.params) {
                Ok(spec) => {
                    // The rest of this connection carries the command's stdin.
                    run::run_sandbox(registry.clone(), spec, lines, &mut write).await?;
                }
                Err(e) => {
                    respond(&mut write, req.id, Err(format!("invalid SandboxSpec: {e}"))).await?;
                }
            }
            return Ok(()); // connection was dedicated to this run
        }

        if req.method == "sandbox.restore" {
            #[derive(Deserialize)]
            struct IdParam {
                id: SandboxId,
            }
            match serde_json::from_value::<IdParam>(req.params) {
                Ok(p) => run::restore_sandbox(registry.clone(), p.id, lines, &mut write).await?,
                Err(e) => {
                    respond(&mut write, req.id, Err(format!("invalid params: {e}"))).await?;
                }
            }
            return Ok(()); // connection was dedicated to this restore
        }

        if req.method == "events.subscribe" {
            // Dedicated connection: stream every registry event as a JSON
            // line until the client goes away (write failure ends us).
            let mut rx = registry.subscribe_events();
            loop {
                match rx.recv().await {
                    Ok(event) => {
                        let mut line = serde_json::to_vec(&event)?;
                        line.push(b'\n');
                        if write.write_all(&line).await.is_err() {
                            return Ok(());
                        }
                        write.flush().await.ok();
                    }
                    // Fell behind the broadcast buffer: skip and continue.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }

        let result = unary(&req, &registry).await;
        respond(&mut write, req.id, result).await?;
    }
    Ok(())
}

async fn unary(req: &Request, registry: &Registry) -> Result<Value, String> {
    match req.method.as_str() {
        "sandbox.list" => Ok(json!(registry
            .list()
            .await
            .into_iter()
            .map(|(id, name, state)| json!({ "id": id, "name": name, "state": state }))
            .collect::<Vec<_>>())),
        "sandbox.snapshot" => {
            #[derive(Deserialize)]
            struct IdParam {
                id: SandboxId,
            }
            let p: IdParam = serde_json::from_value(req.params.clone())
                .map_err(|e| format!("invalid params: {e}"))?;
            let dir = crate::run::sandbox_dir(&p.id);
            registry
                .snapshot(&p.id, &crate::run::state_path(&dir))
                .await
                .map(|_| json!({ "snapshotted": true, "dir": dir.display().to_string() }))
                .map_err(|e| e.to_string())
        }
        "sandbox.pause" | "sandbox.resume" => {
            #[derive(Deserialize)]
            struct IdParam {
                id: SandboxId,
            }
            let p: IdParam = serde_json::from_value(req.params.clone())
                .map_err(|e| format!("invalid params: {e}"))?;
            let pause = req.method == "sandbox.pause";
            registry
                .set_paused(&p.id, pause)
                .await
                .map(|_| json!({ "paused": pause }))
                .map_err(|e| e.to_string())
        }
        "sandbox.kill" => {
            #[derive(Deserialize)]
            struct KillParams {
                id: SandboxId,
                #[serde(default)]
                save: bool,
            }
            let p: KillParams = serde_json::from_value(req.params.clone())
                .map_err(|e| format!("invalid params: {e}"))?;
            let disposition = if p.save {
                TerminationDisposition::Save
            } else {
                TerminationDisposition::Wipe
            };
            registry
                .kill(&p.id, "user", disposition)
                .await
                .map(|_| json!({ "killed": true }))
                .map_err(|e| e.to_string())
        }
        other => Err(format!("unknown method {other:?}")),
    }
}

async fn respond(
    write: &mut (impl tokio::io::AsyncWrite + Unpin),
    id: Value,
    result: Result<Value, String>,
) -> std::io::Result<()> {
    let body = match result {
        Ok(v) => json!({ "jsonrpc": "2.0", "id": id, "result": v }),
        Err(msg) => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32000, "message": msg } }),
    };
    let mut line = serde_json::to_vec(&body)?;
    line.push(b'\n');
    write.write_all(&line).await?;
    write.flush().await
}
