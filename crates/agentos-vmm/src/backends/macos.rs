//! macOS backend: Apple Virtualization.framework, hosted in the
//! `agentos-vmhelper` child process (see `vmhelper/main.swift`).
//!
//! The helper *is* the VM: it boots the microVM (direct kernel boot,
//! virtio-vsock, **no network device**) and relays the guest agent's vsock
//! control connection over its own stdin/stdout. That makes the kill switch
//! a plain SIGKILL of the child, with nothing the guest can do to delay it.

use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::task::{Context, Poll};

use agentos_core::{Error, ExitInfo, Result, SandboxSpec, GUEST_CONTROL_PORT};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub struct VzBackend {
    helper: PathBuf,
}

impl VzBackend {
    pub fn new() -> Self {
        Self {
            helper: locate_helper(),
        }
    }
}

/// Find the signed vmhelper binary: $AGENTOS_VMHELPER, next to the current
/// executable, the dev build location, or ~/.agentos/bin.
fn locate_helper() -> PathBuf {
    if let Some(p) = std::env::var_os("AGENTOS_VMHELPER") {
        return PathBuf::from(p);
    }
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("agentos-vmhelper"));
            // target/{debug,release}/agentos -> target/vmhelper/agentos-vmhelper
            candidates.push(dir.join("../vmhelper/agentos-vmhelper"));
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".agentos/bin/agentos-vmhelper"));
    }
    candidates
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("agentos-vmhelper"))
}

impl Default for VzBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl super::super::VmmBackend for VzBackend {
    fn name(&self) -> &'static str {
        "vz"
    }

    async fn create(
        &self,
        spec: &SandboxSpec,
        paths: &super::super::SandboxPaths,
    ) -> Result<Box<dyn super::super::VmHandle>> {
        if !self.helper.exists() {
            return Err(Error::Backend(format!(
                "vmhelper not found at {} (build it with scripts/build-vmhelper.sh)",
                self.helper.display()
            )));
        }

        let mounts: Vec<serde_json::Value> = spec
            .mounts
            .iter()
            .enumerate()
            .map(|(i, m)| {
                serde_json::json!({
                    "tag": super::super::share_tag(i),
                    "host_path": m.host_path,
                    "read_only": m.mode == agentos_core::MountMode::ReadOnly,
                })
            })
            .collect();

        let config = serde_json::json!({
            "kernel": paths.kernel,
            "initramfs": paths.initramfs,
            "cmdline": "console=hvc0",
            "vcpus": spec.limits.vcpus,
            "mem_mib": spec.limits.mem_mib,
            "vsock_port": GUEST_CONTROL_PORT,
            "console_log": paths.sandbox_dir.join("console.log"),
            "mounts": mounts,
            "proxy_socket": paths.proxy_socket,
            "proxy_port": paths.proxy_socket.as_ref().map(|_| agentos_core::HOST_PROXY_PORT),
        });
        let config_path = paths.sandbox_dir.join("vmconfig.json");
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config)?)?;

        let helper_log = std::fs::File::create(paths.sandbox_dir.join("helper.log"))?;
        let mut child = Command::new(&self.helper)
            .arg(&config_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(helper_log))
            .kill_on_drop(true) // fail-closed: dropping the handle reaps the VM
            .spawn()
            .map_err(|e| Error::Backend(format!("spawn {}: {e}", self.helper.display())))?;

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        Ok(Box::new(VzVmHandle {
            child,
            stdio: Some((stdin, stdout)),
        }))
    }
}

pub struct VzVmHandle {
    child: Child,
    /// The helper's stdio, which *is* the guest's control-port vsock stream.
    /// Taken once by `connect_vsock`.
    stdio: Option<(ChildStdin, ChildStdout)>,
}

#[async_trait::async_trait]
impl super::super::VmHandle for VzVmHandle {
    fn state(&self) -> super::super::VmState {
        super::super::VmState::Running
    }

    fn stats(&self) -> Result<super::super::VmStats> {
        // M2: sample the helper process's CPU/RSS via libproc.
        Ok(super::super::VmStats::default())
    }

    async fn connect_vsock(&mut self, port: u32) -> Result<super::super::VsockStream> {
        if port != GUEST_CONTROL_PORT {
            return Err(Error::Backend(format!(
                "vz helper only relays the control port ({GUEST_CONTROL_PORT}), got {port}"
            )));
        }
        let (stdin, stdout) = self
            .stdio
            .take()
            .ok_or_else(|| Error::Backend("control stream already taken".into()))?;
        Ok(Box::new(HelperPipe { stdin, stdout }))
    }

    async fn kill(&mut self) -> Result<()> {
        // SIGKILL-grade: Child::kill sends SIGKILL and reaps.
        self.child.kill().await.map_err(Error::Io)
    }

    async fn wait(&mut self) -> Result<ExitInfo> {
        let status = self.child.wait().await?;
        use std::os::unix::process::ExitStatusExt;
        Ok(ExitInfo {
            code: status.code(),
            signal: status.signal(),
        })
    }
}

/// Duplex stream over the helper child's stdin/stdout.
struct HelperPipe {
    stdin: ChildStdin,
    stdout: ChildStdout,
}

impl AsyncRead for HelperPipe {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

impl AsyncWrite for HelperPipe {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.stdin).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stdin).poll_shutdown(cx)
    }
}
