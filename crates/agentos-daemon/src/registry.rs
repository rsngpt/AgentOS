//! In-memory registry of sandboxes: the daemon's single source of truth.

use std::collections::HashMap;
use std::sync::Arc;

use agentos_core::{Error, Result, SandboxId, SandboxSpec, SandboxState};
use tokio::sync::Mutex;

/// One tracked sandbox. Grows a `Box<dyn VmHandle>` once VM creation is wired
/// up (milestone M1).
pub struct Sandbox {
    pub spec: SandboxSpec,
    pub state: SandboxState,
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<SandboxId, Sandbox>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Register a sandbox in `Provisioning` state and return its id.
    /// M1 wires this to `agentos_vmm::default_backend()` to actually boot it.
    pub async fn create(&self, spec: SandboxSpec) -> Result<SandboxId> {
        let id = SandboxId::new();
        self.inner.lock().await.insert(
            id.clone(),
            Sandbox {
                spec,
                state: SandboxState::Provisioning,
            },
        );
        Ok(id)
    }

    pub async fn list(&self) -> Vec<(SandboxId, String, SandboxState)> {
        self.inner
            .lock()
            .await
            .iter()
            .map(|(id, sb)| (id.clone(), sb.spec.name.clone(), sb.state.clone()))
            .collect()
    }

    /// The kill switch entry point. M1 makes this SIGKILL the VMM child and
    /// apply the save/wipe disposition; for now it only validates the id.
    pub async fn kill(&self, id: &SandboxId) -> Result<()> {
        let guard = self.inner.lock().await;
        let sb = guard.get(id).ok_or_else(|| Error::UnknownSandbox(id.clone()))?;
        if sb.state.is_terminal() {
            return Err(Error::InvalidState {
                id: id.clone(),
                state: format!("{:?}", sb.state),
                reason: "already terminated".into(),
            });
        }
        Err(Error::Backend("kill not implemented yet (milestone M1)".into()))
    }
}
