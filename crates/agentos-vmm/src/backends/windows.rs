//! Windows backend stub. v1 does not support Windows hosts.
//!
//! Candidate future paths: a Windows Hypervisor Platform (WHP) VMM, or
//! reusing the Linux backend inside a WSL2 utility VM.

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
