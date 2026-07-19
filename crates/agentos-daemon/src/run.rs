//! Orchestration of one sandbox run: provision → boot → handshake → exec →
//! stream → reap → wipe. Emits JSON-line events to the connected client.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agentos_core::protocol::{GuestMessage, HostMessage, PROTOCOL_VERSION};
use agentos_core::{Error, Result, SandboxSpec, SandboxState, GUEST_CONTROL_PORT};
use agentos_vmm::SandboxPaths;
use serde_json::json;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::Mutex;
use tracing::info;

use crate::frames;
use crate::registry::Registry;

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
                        disposition: Default::default(),
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
    spec: SandboxSpec,
    client: &mut (impl AsyncWrite + Unpin),
) -> Result<()> {
    // Provision.
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
    let sandbox_dir = home.join("sandboxes").join(id.to_string());
    std::fs::create_dir_all(&sandbox_dir)?;
    let paths = SandboxPaths {
        sandbox_dir: sandbox_dir.clone(),
        kernel,
        initramfs,
        overlay: None,
    };

    // Boot.
    let backend = agentos_vmm::default_backend()?;
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
    frames::write_host_frame(
        &mut stream,
        &HostMessage::Exec {
            command: spec.command.clone(),
            env: spec.env.clone(),
            mounts: vec![], // M2: virtio-fs shares
            net: spec.net.clone(),
        },
    )
    .await?;
    registry.set_state(id, SandboxState::Running).await;
    emit(client, json!({ "event": "running" }))
        .await
        .map_err(Error::Io)?;

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
            GuestMessage::Exited { info } => {
                exit_info = Some(info);
                break;
            }
            GuestMessage::Hello { .. } | GuestMessage::Metrics { .. } => {}
        }
    }

    // Reap the VMM child (guest powers off after Exited; EOF means it died
    // or was killed).
    let vmm_exit = shared.lock().await.wait().await;
    info!(%id, ?exit_info, ?vmm_exit, "sandbox finished");

    match exit_info {
        Some(info) => {
            registry.set_state(id, SandboxState::Exited { info }).await;
            // Default disposition: wipe. Nothing of the sandbox survives.
            std::fs::remove_dir_all(&sandbox_dir).ok();
            emit(
                client,
                json!({ "event": "exited", "code": info.code, "signal": info.signal }),
            )
            .await
            .map_err(Error::Io)?;
        }
        None => {
            // EOF without Exited: killed (state already set by kill()) or crashed.
            emit(client, json!({ "event": "terminated" }))
                .await
                .map_err(Error::Io)?;
        }
    }
    Ok(())
}
