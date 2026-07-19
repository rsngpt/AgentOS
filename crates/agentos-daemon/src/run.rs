//! Orchestration of one sandbox run: provision → boot → handshake → exec →
//! stream → reap → dispose. Emits JSON-line events to the connected client.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentos_core::protocol::{GuestMessage, HostMessage, PROTOCOL_VERSION};
use agentos_core::{
    Error, NetPolicy, Result, SandboxSpec, SandboxState, TerminationDisposition,
    GUEST_CONTROL_PORT,
};
use agentos_vmm::{share_tag, SandboxPaths};
use serde_json::json;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::info;

use crate::monitor;
use crate::proxy;
use crate::registry::Registry;
use crate::frames;

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);

async fn emit(
    w: &mut (impl AsyncWrite + Unpin),
    event: serde_json::Value,
) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(&event)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

/// Run one sandbox to completion, streaming events to `client`.
pub async fn run_sandbox(
    registry: Registry,
    spec: SandboxSpec,
    client: &mut (impl AsyncWrite + Unpin),
) -> std::io::Result<()> {
    let id = registry.create(spec.clone()).await;
    emit(client, json!({ "event": "created", "id": id })).await?;

    match drive(&registry, &id, spec, client).await {
        Ok(()) => Ok(()),
        Err(e) => {
            registry
                .set_state(
                    &id,
                    SandboxState::Killed {
                        reason: format!("error: {e}"),
                        // Keep the sandbox dir (console/helper logs) on errors.
                        disposition: TerminationDisposition::Save,
                    },
                )
                .await;
            emit(client, json!({ "event": "error", "message": e.to_string() })).await
        }
    }
}

fn agentos_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME not set")).join(".agentos")
}

async fn drive(
    registry: &Registry,
    id: &agentos_core::SandboxId,
    mut spec: SandboxSpec,
    client: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    // Provision: images present, mount sources valid.
    let home = agentos_home();
    let images = home.join("images");
    let kernel = images.join("kernel");
    let initramfs = images.join("initramfs.cpio.gz");
    for p in [&kernel, &initramfs] {
        if !p.exists() {
            return Err(Error::Backend(format!(
                "guest image missing: {} (run scripts/build-guest-image.sh)",
                p.display()
            )));
        }
    }
    for m in &mut spec.mounts {
        let canon = m.host_path.canonicalize().map_err(|e| {
            Error::InvalidSpec(format!("mount {}: {e}", m.host_path.display()))
        })?;
        if !canon.is_dir() {
            return Err(Error::InvalidSpec(format!(
                "mount {} is not a directory",
                canon.display()
            )));
        }
        m.host_path = canon;
    }

    let sandbox_dir = home.join("sandboxes").join(id.to_string());
    std::fs::create_dir_all(&sandbox_dir)?;

    let backend = agentos_vmm::default_backend()?;

    // Egress proxy (unless offline: then there is no egress path at all).
    // The socket location is the backend's call: guest-initiated vsock
    // connections must land on it (CH's hybrid vsock names it by convention).
    let egress_bytes = Arc::new(AtomicU64::new(0));
    let (proxy_socket, proxy_task) = if matches!(spec.net, NetPolicy::Offline) {
        (None, None)
    } else {
        let path = backend.proxy_socket_path(&sandbox_dir);
        let policy = spec.net.clone();
        let bytes = egress_bytes.clone();
        let sock = path.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = proxy::serve(&sock, policy, bytes).await {
                tracing::warn!(error = %e, "egress proxy stopped");
            }
        });
        (Some(path), Some(task))
    };

    let paths = SandboxPaths {
        sandbox_dir: sandbox_dir.clone(),
        kernel,
        initramfs,
        overlay: None,
        proxy_socket,
    };

    // Boot.
    info!(%id, backend = backend.name(), "booting microVM");
    let mut handle = backend.create(&spec, &paths).await?;
    registry.set_state(id, SandboxState::Booting).await;

    let mut stream = handle.connect_vsock(GUEST_CONTROL_PORT).await?;
    let shared = Arc::new(Mutex::new(handle));
    registry.set_handle(id, shared.clone()).await;

    // Handshake (bounded: a wedged boot must not hang the client forever).
    let hello = async {
        frames::write_host_frame(&mut stream, &HostMessage::Hello { version: PROTOCOL_VERSION })
            .await?;
        frames::read_guest_frame(&mut stream).await
    };
    match tokio::time::timeout(HANDSHAKE_TIMEOUT, hello).await {
        Ok(Ok(Some(GuestMessage::Hello { version }))) if version == PROTOCOL_VERSION => {}
        Ok(Ok(other)) => {
            return Err(Error::Protocol(format!("bad handshake reply: {other:?}")));
        }
        Ok(Err(e)) => return Err(Error::Io(e)),
        Err(_) => {
            shared.lock().await.kill().await.ok();
            return Err(Error::Protocol(format!(
                "guest agent did not answer within {HANDSHAKE_TIMEOUT:?} (see {}/console.log)",
                sandbox_dir.display()
            )));
        }
    }

    // Exec.
    let exec_mounts = spec
        .mounts
        .iter()
        .enumerate()
        .map(|(i, m)| {
            (
                share_tag(i),
                m.guest_path.to_string_lossy().into_owned(),
                m.mode == agentos_core::MountMode::ReadOnly,
            )
        })
        .collect();
    frames::write_host_frame(
        &mut stream,
        &HostMessage::Exec {
            command: spec.command.clone(),
            env: spec.env.clone(),
            mounts: exec_mounts,
            net: spec.net.clone(),
        },
    )
    .await?;
    registry.set_state(id, SandboxState::Running).await;
    emit(client, json!({ "event": "running" }))
        .await
        .map_err(Error::Io)?;

    // Auto-kill monitor: samples guest memory (advisory, updated from
    // Metrics frames below), proxy egress bytes, and wall-clock runtime.
    let guest_mem = Arc::new(AtomicU32::new(0));
    let monitor_task = tokio::spawn(monitor::watch(
        registry.clone(),
        id.clone(),
        spec.auto_kill,
        guest_mem.clone(),
        egress_bytes.clone(),
    ));

    // Stream until Exited or EOF.
    let mut exit_info = None;
    while let Some(msg) = frames::read_guest_frame(&mut stream).await? {
        match msg {
            GuestMessage::Stdout { data } => {
                emit(client, json!({ "event": "stdout", "data": data }))
                    .await
                    .map_err(Error::Io)?;
            }
            GuestMessage::Stderr { data } => {
                emit(client, json!({ "event": "stderr", "data": data }))
                    .await
                    .map_err(Error::Io)?;
            }
            GuestMessage::Metrics { mem_mib, .. } => {
                guest_mem.store(mem_mib, Ordering::Relaxed);
            }
            GuestMessage::Exited { info } => {
                exit_info = Some(info);
                break;
            }
            GuestMessage::Hello { .. } => {}
        }
    }
    monitor_task.abort();

    // Reap the VMM child (guest powers off after Exited; EOF means it died
    // or was killed).
    let vmm_exit = shared.lock().await.wait().await;
    if let Some(t) = proxy_task {
        t.abort();
    }
    info!(%id, ?exit_info, ?vmm_exit, "sandbox finished");

    match exit_info {
        Some(info) => {
            registry.set_state(id, SandboxState::Exited { info }).await;
            std::fs::remove_dir_all(&sandbox_dir).ok(); // normal exit: wipe
            emit(
                client,
                json!({ "event": "exited", "code": info.code, "signal": info.signal }),
            )
            .await
            .map_err(Error::Io)?;
        }
        None => {
            // EOF without Exited: the kill switch fired (manual or auto).
            let (reason, disposition) = match registry.get_state(id).await {
                Some(SandboxState::Killed { reason, disposition }) => (reason, disposition),
                _ => ("vmm died".to_string(), TerminationDisposition::Save),
            };
            match disposition {
                TerminationDisposition::Wipe => {
                    std::fs::remove_dir_all(&sandbox_dir).ok();
                }
                TerminationDisposition::Save => {
                    info!(%id, dir = %sandbox_dir.display(), "sandbox state saved");
                }
            }
            emit(
                client,
                json!({
                    "event": "terminated",
                    "reason": reason,
                    "saved_dir": matches!(disposition, TerminationDisposition::Save)
                        .then(|| sandbox_dir.display().to_string()),
                }),
            )
            .await
            .map_err(Error::Io)?;
        }
    }
    Ok(())
}
