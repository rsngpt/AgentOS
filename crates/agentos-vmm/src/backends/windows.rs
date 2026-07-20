//! Windows backend stub. v1 does not support Windows hosts.
//!
//! **Recommended path (see ARCHITECTURE.md §4.1):** run the existing Linux
//! `CloudHypervisorBackend` unchanged inside a WSL2 distro. WSL2 is a real
//! Hyper-V-backed Linux VM with nested virtualization and `/dev/kvm`, so
//! Cloud Hypervisor, virtiofsd, vsock, and the whole guest stack work as-is —
//! this reuses a CI-proven backend rather than writing a from-scratch WHP VMM.
//! `agentosd` still runs natively on Windows; only the VMM child is launched
//! via `wsl.exe -d <distro> -- cloud-hypervisor …`, and the per-sandbox Unix
//! sockets live on the WSL filesystem.
//!
//! Alternative (larger effort, no WSL dependency): a native VMM against the
//! Windows Hypervisor Platform (WHP) API — a new `VmHandle` implementation
//! parallel to the macOS/Linux ones. Deferred until there's demand.
//!
//! This backend cannot be built or exercised from the macOS + Linux CI, so it
//! remains a compile stub; wiring it up is future work behind `#[cfg(windows)]`.

use agentos_core::{Error, Result, SandboxSpec};

use crate::{SandboxPaths, VmHandle, VmmBackend};

pub struct WindowsBackend;

#[async_trait::async_trait]
impl VmmBackend for WindowsBackend {
    fn name(&self) -> &'static str {
        "windows-unsupported"
    }

    async fn create(
        &self,
        _spec: &SandboxSpec,
        _paths: &SandboxPaths,
    ) -> Result<Box<dyn VmHandle>> {
        Err(Error::Unsupported(
            "Windows hosts are not supported yet; see ARCHITECTURE.md §4".into(),
        ))
    }
}
