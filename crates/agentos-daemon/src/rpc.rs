//! JSON-RPC-style server over the daemon's Unix socket.
//!
//! One JSON object per line. Two shapes of method:
//! - Unary (`sandbox.list`, `sandbox.kill`): one request line → one response
//!   line, connection handles many in sequence.
//! - Streaming (`sandbox.run`): the connection is dedicated to the run; the
//!   daemon streams `{event: ...}` lines until the sandbox terminates.

use agentos_core::{NetPolicy, SandboxId, SandboxSpec, TerminationDisposition};
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
        // PRD §4.5: the dashboard's permission controls, applied to an agent
        // that is already running. Fleet policy is re-checked in the registry.
        "sandbox.set_permissions" => {
            #[derive(Deserialize)]
            struct PermParams {
                id: SandboxId,
                /// CLI form: `offline` | `full` | `allowlist:a,b`.
                #[serde(default)]
                net: Option<String>,
                #[serde(default)]
                max_mem_mib: Option<u32>,
                #[serde(default)]
                max_egress_mib: Option<u32>,
                #[serde(default)]
                max_runtime_secs: Option<u64>,
                /// Present only so a client that sends it gets a straight
                /// answer instead of a silently ignored field.
                #[serde(default)]
                mounts: Option<Vec<String>>,
            }
            let p: PermParams = serde_json::from_value(req.params.clone())
                .map_err(|e| format!("invalid params: {e}"))?;
            if p.mounts.is_some() {
                return Err("mounts cannot be changed while a sandbox exists: the \
                            virtio-fs device set is fixed when the VM is created. \
                            Snapshot and restore with different mounts, or start a \
                            new sandbox."
                    .into());
            }
            let net = match p.net.as_deref() {
                Some(s) => Some(NetPolicy::parse(s).map_err(|e| e.to_string())?),
                None => None,
            };
            // Any auto-kill field present replaces the rule set, so clearing a
            // limit is expressible (send the others, omit that one).
            let auto_kill = if p.max_mem_mib.is_some()
                || p.max_egress_mib.is_some()
                || p.max_runtime_secs.is_some()
            {
                Some(agentos_core::AutoKillRules {
                    max_mem_mib: p.max_mem_mib,
                    max_egress_mib: p.max_egress_mib,
                    max_runtime_secs: p.max_runtime_secs,
                })
            } else {
                None
            };
            if net.is_none() && auto_kill.is_none() {
                return Err("nothing to change: pass net and/or an auto-kill limit".into());
            }
            let spec = registry
                .set_permissions(&p.id, net, auto_kill)
                .await
                .map_err(|e| e.to_string())?;
            // Keep the on-disk spec in step, so a later restore comes back with
            // the permissions actually in force rather than the original ones.
            crate::run::persist_spec(&p.id, &spec);
            Ok(json!({
                "net": spec.net.describe(),
                "auto_kill": spec.auto_kill,
            }))
        }
        // PRD §7: "share it with a colleague".
        "sandbox.export" => {
            #[derive(Deserialize)]
            struct ExportParams {
                id: SandboxId,
                dest: String,
            }
            let p: ExportParams = serde_json::from_value(req.params.clone())
                .map_err(|e| format!("invalid params: {e}"))?;
            let spec = registry
                .spec(&p.id)
                .await
                .ok_or_else(|| format!("unknown sandbox {}", p.id))?;
            let backend = agentos_vmm::default_backend().map_err(|e| e.to_string())?;
            let bytes = crate::bundle::export(
                &crate::run::sandbox_dir(&p.id),
                &spec,
                &p.id,
                backend.name(),
                std::path::Path::new(&p.dest),
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({ "path": p.dest, "bytes": bytes }))
        }
        "sandbox.import" => {
            #[derive(Deserialize)]
            struct ImportParams {
                path: String,
                /// `original=local` pairs for mounts that live elsewhere here.
                #[serde(default)]
                remap: Vec<String>,
            }
            let p: ImportParams = serde_json::from_value(req.params.clone())
                .map_err(|e| format!("invalid params: {e}"))?;
            let src = std::path::Path::new(&p.path);
            let manifest = crate::bundle::peek(src).map_err(|e| e.to_string())?;
            let backend = agentos_vmm::default_backend().map_err(|e| e.to_string())?;
            crate::bundle::check_compatible(&manifest, backend.name())
                .map_err(|e| e.to_string())?;

            let remap: Vec<(String, String)> = p
                .remap
                .iter()
                .filter_map(|kv| kv.split_once('=').map(|(a, b)| (a.into(), b.into())))
                .collect();
            let mut spec = manifest.spec.clone();
            crate::bundle::remap_mounts(&mut spec, &remap).map_err(|e| e.to_string())?;

            // An imported sandbox is subject to *this* machine's policy, not
            // the exporter's: a bundle must not be a way to carry permissions
            // past the receiving fleet's rules.
            let policy = agentos_core::FleetPolicy::load().map_err(|e| e.to_string())?;
            if !policy.is_empty() {
                let before = spec.mounts.clone();
                spec = policy.apply(spec).map_err(|e| e.to_string())?;
                if spec.mounts != before {
                    return Err("this machine's fleet policy would change the bundle's \
                                mounts, which a restored VM cannot survive — the saved \
                                guest expects the exact device set it was saved with"
                        .into());
                }
            }

            let id = registry.create(spec.clone()).await;
            let dir = crate::run::sandbox_dir(&id);
            if let Err(e) = crate::bundle::unpack(src, &dir) {
                return Err(e.to_string());
            }
            crate::run::persist_spec(&id, &spec);
            registry
                .set_state(&id, agentos_core::SandboxState::Snapshotted)
                .await;
            Ok(json!({
                "id": id,
                "from": manifest.source_id,
                "net": spec.net.describe(),
                "restore_with": format!("agentos restore {id}"),
            }))
        }
        // PRD §8, computed from the local log. Nothing is transmitted.
        "metrics.summary" => {
            let summary = crate::metrics::summarize(&crate::run::agentos_home());
            serde_json::to_value(summary).map_err(|e| e.to_string())
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
