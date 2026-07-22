//! In-memory registry of sandboxes: the daemon's single source of truth.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use agentos_core::event::{Event, EventKind};
use agentos_core::{
    AutoKillRules, Error, NetPolicy, Result, SandboxId, SandboxSpec, SandboxState,
    TerminationDisposition,
};
use agentos_vmm::VmHandle;

use crate::live::LivePermissions;
use tokio::sync::{broadcast, Mutex};

/// Shared, lockable VM handle. The run task holds the lock only briefly
/// (reap after EOF); the stream pumping happens on a separate vsock stream,
/// so `kill` can always acquire it.
pub type SharedHandle = Arc<Mutex<Box<dyn VmHandle>>>;

pub struct Sandbox {
    pub spec: SandboxSpec,
    pub state: SandboxState,
    pub handle: Option<SharedHandle>,
    /// The grants the proxy and monitor actually consult. `spec` is kept in
    /// step so a snapshot restores with the permissions in force at the time.
    pub permissions: Option<Arc<LivePermissions>>,
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<SandboxId, Sandbox>>>,
    events: broadcast::Sender<Event>,
}

impl Registry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            events: broadcast::channel(1024).0,
        }
    }

    /// Publish an event to every live `events.subscribe` client (dropped
    /// silently when nobody listens).
    pub fn emit_event(&self, sandbox: SandboxId, kind: EventKind) {
        let _ = self.events.send(Event {
            sandbox,
            timestamp_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            kind,
        });
    }

    pub fn subscribe_events(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Register a sandbox in `Provisioning` state and return its id.
    pub async fn create(&self, spec: SandboxSpec) -> SandboxId {
        let id = SandboxId::new();
        self.inner.lock().await.insert(
            id.clone(),
            Sandbox {
                spec,
                state: SandboxState::Provisioning,
                handle: None,
                permissions: None,
            },
        );
        id
    }

    /// Re-register a sandbox that was snapshotted before this daemon started.
    /// It has no VM until `restore` boots one.
    pub async fn insert_snapshotted(&self, id: SandboxId, spec: SandboxSpec) {
        self.inner.lock().await.entry(id).or_insert(Sandbox {
            spec,
            state: SandboxState::Snapshotted,
            handle: None,
            permissions: None,
        });
    }

    pub async fn set_handle(&self, id: &SandboxId, handle: SharedHandle) {
        if let Some(sb) = self.inner.lock().await.get_mut(id) {
            sb.handle = Some(handle);
        }
    }

    /// Update state; refuses to leave a terminal state (a kill that raced
    /// a normal exit keeps whichever verdict landed first).
    pub async fn set_state(&self, id: &SandboxId, state: SandboxState) {
        if let Some(sb) = self.inner.lock().await.get_mut(id) {
            if !sb.state.is_terminal() {
                sb.state = state.clone();
                self.emit_event(id.clone(), EventKind::StateChanged { new_state: state });
            }
        }
    }

    pub async fn get_state(&self, id: &SandboxId) -> Option<SandboxState> {
        self.inner.lock().await.get(id).map(|sb| sb.state.clone())
    }

    pub async fn list(&self) -> Vec<(SandboxId, String, SandboxState)> {
        self.inner
            .lock()
            .await
            .iter()
            .map(|(id, sb)| (id.clone(), sb.spec.name.clone(), sb.state.clone()))
            .collect()
    }

    /// Freeze or unfreeze a sandbox's vCPUs (PRD §7 pause mid-task).
    /// Only legal from the matching live state; terminal sandboxes refuse.
    pub async fn set_paused(&self, id: &SandboxId, paused: bool) -> Result<()> {
        let handle = {
            let guard = self.inner.lock().await;
            let sb = guard
                .get(id)
                .ok_or_else(|| Error::UnknownSandbox(id.clone()))?;
            let ok = if paused {
                matches!(sb.state, SandboxState::Running)
            } else {
                matches!(sb.state, SandboxState::Paused)
            };
            if !ok {
                return Err(Error::InvalidState {
                    id: id.clone(),
                    state: format!("{:?}", sb.state),
                    reason: if paused {
                        "only a running sandbox can be paused".into()
                    } else {
                        "only a paused sandbox can be resumed".into()
                    },
                });
            }
            sb.handle.clone()
        };
        let handle = handle.ok_or_else(|| Error::InvalidState {
            id: id.clone(),
            state: "no vm".into(),
            reason: "sandbox has no running VM".into(),
        })?;

        // Drive the hypervisor first: only record the new state if it worked.
        {
            let mut h = handle.lock().await;
            if paused {
                h.pause().await?;
            } else {
                h.resume().await?;
            }
        }
        self.set_state(
            id,
            if paused {
                SandboxState::Paused
            } else {
                SandboxState::Running
            },
        )
        .await;
        Ok(())
    }

    /// Write the VM's state to `path` and tear the VM down. The sandbox dir
    /// survives so `restore` can bring it back mid-execution.
    pub async fn snapshot(&self, id: &SandboxId, path: &std::path::Path) -> Result<()> {
        let handle = {
            let guard = self.inner.lock().await;
            let sb = guard
                .get(id)
                .ok_or_else(|| Error::UnknownSandbox(id.clone()))?;
            if !matches!(sb.state, SandboxState::Running | SandboxState::Paused) {
                return Err(Error::InvalidState {
                    id: id.clone(),
                    state: format!("{:?}", sb.state),
                    reason: "only a running or paused sandbox can be snapshotted".into(),
                });
            }
            sb.handle.clone()
        };
        let handle = handle.ok_or_else(|| Error::InvalidState {
            id: id.clone(),
            state: "no vm".into(),
            reason: "sandbox has no running VM".into(),
        })?;
        // Mark it first so the run task, which sees the VM disappear, knows
        // this was a snapshot and keeps the sandbox dir.
        self.set_state(id, SandboxState::Snapshotted).await;
        // If this fails the VM may be gone anyway, so surface the error rather
        // than pretending the sandbox is still healthy.
        let mut vm = handle.lock().await;
        vm.snapshot(path).await
    }

    /// Spec of a sandbox, for restoring it later.
    pub async fn spec(&self, id: &SandboxId) -> Option<SandboxSpec> {
        self.inner.lock().await.get(id).map(|sb| sb.spec.clone())
    }

    /// Attach the live grants a booting sandbox's proxy and monitor read from.
    pub async fn set_permissions_handle(&self, id: &SandboxId, perms: Arc<LivePermissions>) {
        if let Some(sb) = self.inner.lock().await.get_mut(id) {
            sb.permissions = Some(perms);
        }
    }

    /// Change a live sandbox's grants (PRD §4.5: the dashboard's permission
    /// controls, which are only meaningful if they work on a running agent).
    ///
    /// Two constraints that are not negotiable:
    ///
    /// - **Fleet policy is re-applied here.** Editing permissions is a second
    ///   door into the same decision `run_sandbox` guards; letting it through
    ///   unchecked would let any user widen a sandbox past what IT allows by
    ///   starting compliant and then editing. The new grants go through
    ///   `FleetPolicy::apply` exactly as the original spec did.
    /// - **Only network and auto-kill can change.** Mounts and CPU/RAM/disk are
    ///   fixed in the VM's device set at creation; see `live.rs`.
    pub async fn set_permissions(
        &self,
        id: &SandboxId,
        net: Option<NetPolicy>,
        auto_kill: Option<AutoKillRules>,
    ) -> Result<SandboxSpec> {
        let mut guard = self.inner.lock().await;
        let sb = guard
            .get_mut(id)
            .ok_or_else(|| Error::UnknownSandbox(id.clone()))?;
        if sb.state.is_terminal() {
            return Err(Error::InvalidState {
                id: id.clone(),
                state: format!("{:?}", sb.state),
                reason: "cannot change the permissions of a terminated sandbox".into(),
            });
        }

        // Build the spec the sandbox *would* have, and hold it to the same
        // machine-wide policy a fresh run would face.
        let mut proposed = sb.spec.clone();
        if let Some(net) = net {
            proposed.net = net;
        }
        if let Some(rules) = auto_kill {
            proposed.auto_kill = rules;
        }
        let policy = agentos_core::FleetPolicy::load()?;
        let proposed = if policy.is_empty() {
            proposed
        } else {
            policy.apply(proposed)?
        };

        // Apply to the values the proxy and monitor actually read. Without a
        // live handle (a snapshotted sandbox) the spec change still lands and
        // takes effect when it is restored.
        if let Some(perms) = &sb.permissions {
            perms.set_net(proposed.net.clone());
            perms.set_auto_kill(proposed.auto_kill);
        }
        sb.spec = proposed.clone();
        drop(guard);

        self.emit_event(
            id.clone(),
            EventKind::PermissionsChanged {
                net: proposed.net.describe(),
            },
        );
        Ok(proposed)
    }

    /// The kill switch: SIGKILL the VMM child. Absolute and immediate.
    pub async fn kill(
        &self,
        id: &SandboxId,
        reason: &str,
        disposition: TerminationDisposition,
    ) -> Result<()> {
        let handle = {
            let mut guard = self.inner.lock().await;
            let sb = guard
                .get_mut(id)
                .ok_or_else(|| Error::UnknownSandbox(id.clone()))?;
            if sb.state.is_terminal() {
                return Err(Error::InvalidState {
                    id: id.clone(),
                    state: format!("{:?}", sb.state),
                    reason: "already terminated".into(),
                });
            }
            sb.state = SandboxState::Killed {
                reason: reason.to_string(),
                disposition,
            };
            sb.handle.clone()
        };
        match handle {
            Some(h) => h.lock().await.kill().await,
            None => Ok(()), // killed before the VM ever spawned
        }
    }
}
