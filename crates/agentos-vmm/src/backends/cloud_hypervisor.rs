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

use crate::{SandboxPaths, VmHandle, VmState, VmStats, VmmBackend, VsockStream};

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
        // vhost-user needs the socket to exist before CH starts.
        for (i, _) in spec.mounts.iter().enumerate() {
            wait_for_path(&dir.join(format!("fs{i}.sock")), Duration::from_secs(3)).await?;
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

        let ch_log = std::fs::File::create(dir.join("helper.log"))?;
        let mut cmd = Command::new(&self.ch_bin);
        cmd.arg("--kernel")
            .arg(&paths.kernel)
            .arg("--initramfs")
            .arg(&paths.initramfs)
            .arg("--cmdline")
            .arg(cmdline)
            .arg("--cpus")
            .arg(format!("boot={}", spec.limits.vcpus))
            .arg("--memory")
            .arg(&memory)
            .arg("--vsock")
            .arg(format!("cid=3,socket={}", vsock.display()))
            .arg("--serial")
            .arg(format!("file={}", dir.join("console.log").display()))
            .arg("--console")
            .arg("off")
            .stdin(Stdio::null())
            .stdout(Stdio::from(ch_log.try_clone()?))
            .stderr(Stdio::from(ch_log))
            .kill_on_drop(true);
        for fs in &fs_args {
            cmd.arg("--fs").arg(fs);
        }
        let child = cmd
            .spawn()
            .map_err(|e| Error::Backend(format!("spawn {}: {e}", self.ch_bin.display())))?;

        Ok(Box::new(ChVmHandle {
            child,
            virtiofsds,
            vsock,
        }))
    }
}

pub struct ChVmHandle {
    child: Child,
    virtiofsds: Vec<Child>,
    vsock: PathBuf,
}

#[async_trait::async_trait]
impl VmHandle for ChVmHandle {
    fn state(&self) -> VmState {
        VmState::Running
    }

    fn stats(&self) -> Result<VmStats> {
        Ok(VmStats::default())
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
