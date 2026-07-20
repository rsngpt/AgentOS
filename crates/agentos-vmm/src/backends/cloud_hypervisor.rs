//! Linux backend: Cloud Hypervisor spawned as a child process.
//!
//! Chosen over Firecracker because our mount model requires virtio-fs
//! (Firecracker has none). Design mirrors the macOS helper model:
//!
//! - the `cloud-hypervisor` process *is* the VM → kill = SIGKILL the child;
//! - **no network device is ever configured** — the guest's only channel is
//!   virtio-vsock (`--vsock`, hybrid Unix-socket model);
//! - one unprivileged `virtiofsd` per mount, with `--readonly` enforced
//!   host-side for RO shares (vhost-user-fs requires `--memory shared=on`);
//! - host-initiated control connections speak the hybrid handshake:
//!   connect to the vsock UDS, send `CONNECT <port>\n`, expect `OK <n>\n`;
//! - guest-initiated egress connections to port P surface as connections to
//!   `<vsock_socket>_P`, which is where the daemon binds its policy proxy
//!   (see `proxy_socket_path`).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use agentos_core::{Error, ExitInfo, Result, SandboxSpec, HOST_PROXY_PORT};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::process::{Child, Command};

use crate::{SandboxPaths, VmHandle, VmState, VmmBackend, VsockStream};

const CONNECT_RETRIES: u32 = 150; // x 100ms = 15s boot budget

pub struct CloudHypervisorBackend {
    ch_bin: PathBuf,
    virtiofsd_bin: PathBuf,
}

impl CloudHypervisorBackend {
    pub fn new() -> Self {
        Self {
            ch_bin: bin_from_env("CLOUD_HYPERVISOR", "cloud-hypervisor"),
            virtiofsd_bin: bin_from_env("VIRTIOFSD", "virtiofsd"),
        }
    }
}

fn bin_from_env(var: &str, default: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(default))
}

impl Default for CloudHypervisorBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn vsock_socket(sandbox_dir: &Path) -> PathBuf {
    sandbox_dir.join("vsock.sock")
}

#[async_trait::async_trait]
impl VmmBackend for CloudHypervisorBackend {
    fn name(&self) -> &'static str {
        "cloud-hypervisor"
    }

    fn proxy_socket_path(&self, sandbox_dir: &Path) -> PathBuf {
        // CH's hybrid vsock convention: guest connections to port P arrive on
        // "<socket>_P". The daemon must bind its proxy exactly there.
        let mut name = vsock_socket(sandbox_dir).into_os_string();
        name.push(format!("_{HOST_PROXY_PORT}"));
        PathBuf::from(name)
    }

    async fn create(&self, spec: &SandboxSpec, paths: &SandboxPaths) -> Result<Box<dyn VmHandle>> {
        let dir = &paths.sandbox_dir;
        let vsock = vsock_socket(dir);

        // One virtiofsd per share; RO enforced by virtiofsd, not the guest.
        let mut virtiofsds = Vec::new();
        let mut fs_args: Vec<String> = Vec::new();
        for (i, m) in spec.mounts.iter().enumerate() {
            let sock = dir.join(format!("fs{i}.sock"));
            let log = std::fs::File::create(dir.join(format!("fs{i}.log")))?;
            let mut cmd = Command::new(&self.virtiofsd_bin);
            // --sandbox none: the default (namespace) needs unprivileged user
            // namespaces, which Ubuntu 24.04+ blocks via AppArmor. virtiofsd
            // still runs as the invoking user and can expose nothing beyond
            // the directory the user explicitly granted (same trust model as
            // the macOS in-process share). TODO: auto-upgrade to namespace
            // sandboxing where the host allows it.
            cmd.arg("--socket-path")
                .arg(&sock)
                .arg("--shared-dir")
                .arg(&m.host_path)
                .arg("--sandbox")
                .arg("none")
                .stdin(Stdio::null())
                .stdout(Stdio::from(log.try_clone()?))
                .stderr(Stdio::from(log))
                .kill_on_drop(true);
            if m.mode == agentos_core::MountMode::ReadOnly {
                cmd.arg("--readonly");
            }
            let child = cmd.spawn().map_err(|e| {
                Error::Backend(format!("spawn {}: {e}", self.virtiofsd_bin.display()))
            })?;
            virtiofsds.push(child);
            fs_args.push(format!(
                "tag={},socket={}",
                crate::share_tag(i),
                sock.display()
            ));
        }
        // vhost-user needs the socket to exist before CH starts. If a
        // virtiofsd died instead, surface its own words.
        for (i, _) in spec.mounts.iter().enumerate() {
            if let Err(e) =
                wait_for_path(&dir.join(format!("fs{i}.sock")), Duration::from_secs(3)).await
            {
                let log = std::fs::read_to_string(dir.join(format!("fs{i}.log")))
                    .unwrap_or_default();
                let log = log.trim();
                return Err(Error::Backend(format!(
                    "{e}{}{}",
                    if log.is_empty() { "" } else { "; virtiofsd said: " },
                    log.chars().take(300).collect::<String>()
                )));
            }
        }

        let cmdline = if cfg!(target_arch = "aarch64") {
            "console=ttyAMA0"
        } else {
            "console=ttyS0"
        };
        let memory = if spec.mounts.is_empty() {
            format!("size={}M", spec.limits.mem_mib)
        } else {
            // vhost-user-fs requires guest memory shared with virtiofsd.
            format!("size={}M,shared=on", spec.limits.mem_mib)
        };

        let api_socket = dir.join("api.sock");
        let mut args: Vec<String> = vec![
            // Control channel for pause/resume (ch-remote talks to this).
            "--api-socket".into(),
            api_socket.display().to_string(),
            "--kernel".into(),
            paths.kernel.display().to_string(),
            "--initramfs".into(),
            paths.initramfs.display().to_string(),
            "--cmdline".into(),
            cmdline.into(),
            "--cpus".into(),
            format!("boot={}", spec.limits.vcpus),
            "--memory".into(),
            memory,
            "--vsock".into(),
            format!("cid=3,socket={}", vsock.display()),
            "--serial".into(),
            format!("file={}", dir.join("console.log").display()),
            "--console".into(),
            "off".into(),
        ];
        for fs in &fs_args {
            args.push("--fs".into());
            args.push(fs.clone());
        }
        // Disks, in order: vda = read-only runtime rootfs, vdb = writable
        // overlay. Each value after --disk is one disk (CH parses them
        // independently); mark read/write explicitly. Keep vda/vdb order in
        // sync with the guest agent.
        // Disks: vda = read-only rootfs, vdb = writable overlay.
        // image_type=raw is REQUIRED: without it CH auto-detects raw but then
        // "disables sector 0 writes" as a qcow2-misdetection safeguard, which
        // makes mkfs.ext4's superblock write to the overlay fail ReadOnly.
        if let Some(rootfs) = &paths.rootfs {
            args.push("--disk".into());
            args.push(format!("path={},image_type=raw,readonly=on", rootfs.display()));
            if let Some(overlay) = &paths.overlay {
                args.push("--disk".into());
                args.push(format!("path={},image_type=raw", overlay.display()));
            }
        }
        tracing::info!(bin = %self.ch_bin.display(), ?args, "spawning cloud-hypervisor");

        let ch_log = std::fs::File::create(dir.join("helper.log"))?;
        let child = Command::new(&self.ch_bin)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::from(ch_log.try_clone()?))
            .stderr(Stdio::from(ch_log))
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| Error::Backend(format!("spawn {}: {e}", self.ch_bin.display())))?;

        Ok(Box::new(ChVmHandle {
            child,
            virtiofsds,
            vsock,
            api_socket,
            ch_remote: bin_from_env("CH_REMOTE", "ch-remote"),
        }))
    }
}

pub struct ChVmHandle {
    child: Child,
    virtiofsds: Vec<Child>,
    vsock: PathBuf,
    api_socket: PathBuf,
    ch_remote: PathBuf,
}

impl ChVmHandle {
    /// Drive the VM through `ch-remote` against this VM's API socket.
    async fn ch_remote(&self, verb: &str) -> Result<()> {
        let out = Command::new(&self.ch_remote)
            .arg("--api-socket")
            .arg(&self.api_socket)
            .arg(verb)
            .output()
            .await
            .map_err(|e| Error::Backend(format!("running {} {verb}: {e}", self.ch_remote.display())))?;
        if !out.status.success() {
            return Err(Error::Backend(format!(
                "ch-remote {verb} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl VmHandle for ChVmHandle {
    fn state(&self) -> VmState {
        VmState::Running
    }

    fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    async fn connect_vsock(&mut self, port: u32) -> Result<VsockStream> {
        // Hybrid vsock handshake, retried while the guest boots and the
        // guest agent opens its listener.
        for _ in 0..CONNECT_RETRIES {
            match try_connect(&self.vsock, port).await {
                Ok(stream) => return Ok(Box::new(stream)),
                Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
            }
        }
        Err(Error::Backend(format!(
            "guest never accepted vsock port {port} via {}",
            self.vsock.display()
        )))
    }

    async fn pause(&mut self) -> Result<()> {
        self.ch_remote("pause").await
    }

    async fn resume(&mut self) -> Result<()> {
        self.ch_remote("resume").await
    }

    async fn snapshot(&mut self, path: &std::path::Path) -> Result<()> {
        // CH requires the VM be paused before snapshotting, and writes a
        // *directory* of state files addressed by URL.
        self.ch_remote("pause").await?;
        std::fs::create_dir_all(path)?;
        let out = Command::new(&self.ch_remote)
            .arg("--api-socket")
            .arg(&self.api_socket)
            .arg("snapshot")
            .arg(format!("file://{}", path.display()))
            .output()
            .await
            .map_err(|e| Error::Backend(format!("running ch-remote snapshot: {e}")))?;
        if !out.status.success() {
            return Err(Error::Backend(format!(
                "ch-remote snapshot failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
        self.child.kill().await.ok();
        for fsd in &mut self.virtiofsds {
            fsd.kill().await.ok();
        }
        Ok(())
    }

    async fn kill(&mut self) -> Result<()> {
        for fsd in &mut self.virtiofsds {
            fsd.kill().await.ok();
        }
        self.child.kill().await.map_err(Error::Io)
    }

    async fn wait(&mut self) -> Result<ExitInfo> {
        let status = self.child.wait().await?;
        // The VM is gone; the virtiofsds have no reason to outlive it.
        for fsd in &mut self.virtiofsds {
            fsd.kill().await.ok();
        }
        use std::os::unix::process::ExitStatusExt;
        Ok(ExitInfo {
            code: status.code(),
            signal: status.signal(),
        })
    }
}

async fn try_connect(vsock: &Path, port: u32) -> std::io::Result<BufReader<UnixStream>> {
    let stream = UnixStream::connect(vsock).await?;
    let mut stream = BufReader::new(stream);
    stream
        .get_mut()
        .write_all(format!("CONNECT {port}\n").as_bytes())
        .await?;
    let mut line = String::new();
    let n = tokio::time::timeout(Duration::from_secs(2), stream.read_line(&mut line))
        .await
        .map_err(|_| std::io::Error::other("hybrid vsock handshake timeout"))??;
    if n == 0 || !line.starts_with("OK") {
        return Err(std::io::Error::other(format!(
            "hybrid vsock refused port {port}: {line:?}"
        )));
    }
    Ok(stream)
}

async fn wait_for_path(path: &Path, budget: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    while !path.exists() {
        if tokio::time::Instant::now() > deadline {
            return Err(Error::Backend(format!(
                "virtiofsd socket {} never appeared",
                path.display()
            )));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
