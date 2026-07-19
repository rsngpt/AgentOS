//! JSON-RPC 2.0 server over the daemon's Unix socket.
//!
//! One request per line (LSP-style framing comes later if needed). Methods:
//! `sandbox.create`, `sandbox.list`, `sandbox.kill`. `sandbox.attach` and
//! `sandbox.events` (streaming) arrive with milestone M1/M2.

use agentos_core::SandboxSpec;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::registry::Registry;

#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: Value,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<Value>,
}

pub async fn serve_connection(
    stream: UnixStream,
    registry: Registry,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();

    while let Some(line) = lines.next_line().await? {
        let resp = match serde_json::from_str::<Request>(&line) {
            Ok(req) => dispatch(req, &registry).await,
            Err(e) => Response {
                jsonrpc: "2.0",
                id: Value::Null,
                result: None,
                error: Some(json!({ "code": -32700, "message": e.to_string() })),
            },
        };
        let mut out = serde_json::to_vec(&resp)?;
        out.push(b'\n');
        write.write_all(&out).await?;
    }
    Ok(())
}

async fn dispatch(req: Request, registry: &Registry) -> Response {
    let result: Result<Value, String> = match req.method.as_str() {
        "sandbox.create" => match serde_json::from_value::<SandboxSpec>(req.params) {
            Ok(spec) => registry
                .create(spec)
                .await
                .map(|id| json!({ "id": id }))
                .map_err(|e| e.to_string()),
            Err(e) => Err(format!("invalid SandboxSpec: {e}")),
        },
        "sandbox.list" => Ok(json!(registry
            .list()
            .await
            .into_iter()
            .map(|(id, name, state)| json!({ "id": id, "name": name, "state": state }))
            .collect::<Vec<_>>())),
        "sandbox.kill" => match serde_json::from_value(req.params) {
            Ok(id) => registry
                .kill(&id)
                .await
                .map(|_| json!({ "killed": true }))
                .map_err(|e| e.to_string()),
            Err(e) => Err(format!("invalid sandbox id: {e}")),
        },
        other => Err(format!("unknown method {other:?}")),
    };

    match result {
        Ok(v) => Response {
            jsonrpc: "2.0",
            id: req.id,
            result: Some(v),
            error: None,
        },
        Err(msg) => Response {
            jsonrpc: "2.0",
            id: req.id,
            result: None,
            error: Some(json!({ "code": -32000, "message": msg })),
        },
    }
}
