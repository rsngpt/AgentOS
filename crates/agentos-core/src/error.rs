use crate::spec::SandboxId;

/// Unified error type for Agent OS operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A user-supplied spec fragment (mount string, net policy, limits) was invalid.
    #[error("invalid specification: {0}")]
    InvalidSpec(String),

    /// The requested sandbox does not exist in the daemon registry.
    #[error("unknown sandbox: {0}")]
    UnknownSandbox(SandboxId),

    /// The operation is not valid in the sandbox's current lifecycle state.
    #[error("sandbox {id} is {state}: {reason}")]
    InvalidState {
        id: SandboxId,
        state: String,
        reason: String,
    },

    /// No hypervisor backend is available on this host OS.
    #[error("no supported hypervisor backend on this platform: {0}")]
    Unsupported(String),

    /// A hypervisor backend failed to create or operate a VM.
    #[error("vmm backend error: {0}")]
    Backend(String),

    /// A control-protocol message could not be encoded/decoded.
    #[error("protocol error: {0}")]
    Protocol(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
