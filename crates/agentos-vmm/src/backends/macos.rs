//! macOS backend: Apple Virtualization.framework via `objc2-virtualization`.
//!
//! Planned device set (ARCHITECTURE.md §4–§5): direct kernel boot
//! (`VZLinuxBootLoader`), virtio-blk for rootfs + overlay, one
//! `VZVirtioFileSystemDeviceConfiguration` per mount (RO enforced by share
//! config), `VZVirtioSocketDeviceConfiguration` for vsock. No network device.

use agentos_core::{Error, ExitInfo, Result, SandboxSpec};

use crate::{SandboxPaths, VmHandle, VmState, VmStats, VmmBackend, VsockStream};

pub struct VzBackend;

impl VzBackend {
    pub fn new() -> Self {
        Self
    }
}

impl Default for VzBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl VmmBackend for VzBackend {
    fn name(&self) -> &'static str {
        "vz"
    }

    async fn create(
        &self,
        _spec: &SandboxSpec,
        _paths: &SandboxPaths,
    ) -> Result<Box<dyn VmHandle>> {
        // M1: build VZVirtualMachineConfiguration from spec/paths, spawn the
        // VM in a dedicated child helper process (so kill() is a SIGKILL),
        // and return a handle wrapping that child.
        Err(Error::Backend(
            "vz backend not implemented yet (milestone M1)".into(),
        ))
    }
}

/// Handle to a VM hosted by a child helper process running Virtualization.framework.
pub struct VzVmHandle {
    _child_pid: u32,
}

#[async_trait::async_trait]
impl VmHandle for VzVmHandle {
    fn state(&self) -> VmState {
        VmState::Stopped
    }

    fn stats(&self) -> Result<VmStats> {
        Err(Error::Backend("not implemented".into()))
    }

    async fn connect_vsock(&self, _port: u32) -> Result<VsockStream> {
        Err(Error::Backend("not implemented".into()))
    }

    async fn kill(&mut self) -> Result<()> {
        Err(Error::Backend("not implemented".into()))
    }

    async fn wait(&mut self) -> Result<ExitInfo> {
        Err(Error::Backend("not implemented".into()))
    }
}
