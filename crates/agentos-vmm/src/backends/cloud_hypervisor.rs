//! Linux backend: Cloud Hypervisor spawned as a child process and driven
//! over its REST API socket (`ch-remote` protocol).
//!
//! Chosen over Firecracker because our mount model requires virtio-fs
//! (Firecracker has none). Each mount runs an unprivileged `virtiofsd`;
//! read-only shares are enforced by the virtiofsd invocation, not the guest.

use agentos_core::{Error, ExitInfo, Result, SandboxSpec};

use crate::{SandboxPaths, VmHandle, VmState, VmStats, VmmBackend, VsockStream};

pub struct CloudHypervisorBackend;

impl CloudHypervisorBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CloudHypervisorBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl VmmBackend for CloudHypervisorBackend {
    fn name(&self) -> &'static str {
        "cloud-hypervisor"
    }

    async fn create(
        &self,
        _spec: &SandboxSpec,
        _paths: &SandboxPaths,
    ) -> Result<Box<dyn VmHandle>> {
        // M3: spawn virtiofsd per mount, spawn cloud-hypervisor with
        // --kernel/--disk/--fs/--vsock and no --net, configure via API socket.
        Err(Error::Backend(
            "cloud-hypervisor backend not implemented yet (milestone M3)".into(),
        ))
    }
}
