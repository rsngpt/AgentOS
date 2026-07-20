//! Sandbox lifecycle states.

use serde::{Deserialize, Serialize};

use crate::spec::TerminationDisposition;

/// Lifecycle of a sandbox.
///
/// ```text
/// Provisioning → Booting → Running → Exited
///                    │         │
///                    └────► Killed
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum SandboxState {
    /// Overlay disk and share configuration being prepared on the host.
    Provisioning,
    /// VMM process spawned; waiting for the guest agent's `Hello`.
    Booting,
    /// Guest agent handshake complete; agent command executing.
    Running,
    /// vCPUs frozen via `agentos pause`. The sandbox still holds its VM and
    /// grants; `resume` continues exactly where it left off.
    Paused,
    /// VM state written to disk via `agentos snapshot` and the VM torn down.
    /// The sandbox dir (state file + overlay) survives for `agentos restore`.
    Snapshotted,
    /// The guest command exited on its own.
    Exited { info: ExitInfo },
    /// Terminated via the kill switch (manual or auto-kill rule).
    Killed {
        /// Human-readable trigger: "user", or the auto-kill rule that fired.
        reason: String,
        disposition: TerminationDisposition,
    },
}

impl SandboxState {
    /// True once the sandbox can no longer transition to another state.
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Killed { .. })
    }
}

/// How a guest command finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    /// Exit code of the agent command, if it exited normally.
    pub code: Option<i32>,
    /// Signal that terminated it inside the guest, if any.
    pub signal: Option<i32>,
}
