//! Hypervisor abstraction for Agent OS.
//!
//! The daemon talks only to [`VmmBackend`] and [`VmHandle`]; everything
//! platform-specific lives in [`backends`]. Adding an OS means adding one
//! backend module — nothing above this crate changes.
//!
//! Backends by platform (see ARCHITECTURE.md §4):
//! - macOS: Virtualization.framework (`objc2-virtualization`)
//! - Linux: Cloud Hypervisor child process driven over its REST API socket
//! - Windows: stub returning [`agentos_core::Error::Unsupported`]

pub mod backends;

use std::path::PathBuf;

use agentos_core::{ExitInfo, Result, SandboxSpec};
use tokio::io::{AsyncRead, AsyncWrite};

/// Host-side filesystem locations prepared by the daemon for one sandbox.
#[derive(Debug, Clone)]
pub struct SandboxPaths {
    /// Per-sandbox scratch dir, e.g. `~/.agentos/sandboxes/<id>/`.
    pub sandbox_dir: PathBuf,
    /// Shared read-only guest kernel image (uncompressed ARM64/x86 Image).
    pub kernel: PathBuf,
    /// Shared initramfs containing the guest agent as /init.
    pub initramfs: PathBuf,
    /// Shared read-only runtime rootfs (squashfs), attached as `/dev/vda`.
    /// `None` falls the guest back to the initramfs (busybox only).
    pub rootfs: Option<PathBuf>,
    /// This sandbox's writable overlay disk, attached as `/dev/vdb`.
    pub overlay: Option<PathBuf>,
    /// Unix socket of the daemon's egress proxy for this sandbox. `None`
    /// under `NetPolicy::Offline`: the guest then has no egress path at all.
    pub proxy_socket: Option<PathBuf>,
}

/// virtio-fs share tag for the i-th mount in a `SandboxSpec` (the daemon and
/// the backend must agree on this naming).
pub fn share_tag(index: usize) -> String {
    format!("share{index}")
}

/// Coarse VM run state as observed from the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    Starting,
    Running,
    Stopped,
}

/// Host-side resource measurements for a running VM (source of truth for
/// quotas; guest-reported numbers are advisory only).
#[derive(Debug, Clone, Copy, Default)]
pub struct VmStats {
    pub cpu_percent: u32,
    pub rss_mib: u32,
}

/// A bidirectional byte stream to a vsock port inside the guest.
pub type VsockStream = Box<dyn VsockIo>;

/// Object-safe alias for an async duplex stream.
pub trait VsockIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> VsockIo for T {}

/// A hypervisor capable of creating microVMs on this host.
#[async_trait::async_trait]
pub trait VmmBackend: Send + Sync {
    /// Short backend identifier for logs and `agentos ps` (e.g. `"vz"`, `"cloud-hypervisor"`).
    fn name(&self) -> &'static str;

    /// Where the daemon must bind this sandbox's egress-proxy Unix socket so
    /// that guest-initiated vsock connections to [`agentos_core::HOST_PROXY_PORT`]
    /// reach it. Backends with fixed naming conventions (Cloud Hypervisor's
    /// hybrid vsock `<socket>_<port>`) override this.
    fn proxy_socket_path(&self, sandbox_dir: &std::path::Path) -> PathBuf {
        sandbox_dir.join("proxy.sock")
    }

    /// Boot a microVM for `spec`. Returns once the VMM process is spawned;
    /// the caller completes the guest-agent handshake via [`VmHandle::connect_vsock`].
    async fn create(&self, spec: &SandboxSpec, paths: &SandboxPaths) -> Result<Box<dyn VmHandle>>;
}

/// A handle to one running microVM.
///
/// Dropping the handle must not leak the VM: implementations kill the VMM
/// process on drop if it is still alive (fail-closed).
#[async_trait::async_trait]
pub trait VmHandle: Send + Sync {
    fn state(&self) -> VmState;

    /// Sample host-side resource usage of the VMM process.
    fn stats(&self) -> Result<VmStats>;

    /// Connect to a vsock port inside the guest (control or proxy channel).
    async fn connect_vsock(&mut self, port: u32) -> Result<VsockStream>;

    /// The kill switch: destroy the VMM process immediately (SIGKILL-grade).
    /// Must be absolute — no graceful shutdown, nothing the guest can delay.
    async fn kill(&mut self) -> Result<()>;

    /// Wait for the VMM process to exit (either on its own or via `kill`).
    async fn wait(&mut self) -> Result<ExitInfo>;
}

/// The preferred backend for the current host OS.
pub fn default_backend() -> Result<Box<dyn VmmBackend>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Box::new(backends::macos::VzBackend::new()))
    }
    #[cfg(target_os = "linux")]
    {
        Ok(Box::new(backends::cloud_hypervisor::CloudHypervisorBackend::new()))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Err(agentos_core::Error::Unsupported(
            "Agent OS currently supports macOS and Linux hosts".into(),
        ))
    }
}
