//! Events emitted by the daemon: the audit trail behind the GUI's live
//! monitor, `agentos events`, and auto-kill decisions.

use serde::{Deserialize, Serialize};

use crate::spec::SandboxId;
use crate::state::SandboxState;

/// One entry in a sandbox's event log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub sandbox: SandboxId,
    /// Milliseconds since the Unix epoch, host clock.
    pub timestamp_ms: u64,
    pub kind: EventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EventKind {
    StateChanged { new_state: SandboxState },
    /// The egress proxy allowed or denied a connection attempt.
    NetVerdict {
        dest_host: String,
        dest_port: u16,
        allowed: bool,
    },
    /// Periodic resource sample (also the input to auto-kill rules).
    ResourceSample {
        cpu_percent: u32,
        mem_mib: u32,
        egress_total_bytes: u64,
        /// Overlay space in use, i.e. what the agent has written.
        #[serde(default)]
        disk_used_mib: u32,
    },
    /// An auto-kill rule fired; a `StateChanged` to `Killed` follows.
    AutoKillTriggered { rule: String },
}
