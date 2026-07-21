//! Core domain types for Agent OS.
//!
//! This crate is the shared vocabulary of the system: sandbox specifications,
//! permission policies, lifecycle states, events, and the host⇄guest control
//! protocol. It performs no I/O and has no platform dependencies, so every
//! other crate — daemon, CLI, VMM backends, and the guest agent — can depend
//! on it, including cross-compiled guest builds.

pub mod error;
pub mod event;
pub mod policy;
pub mod protocol;
pub mod spec;
pub mod state;

pub use error::{Error, Result};
pub use policy::FleetPolicy;
pub use spec::{
    template_net, AutoKillRules, MountMode, MountSpec, NetPolicy, RepoSpec, ResourceLimits,
    SandboxId, SandboxSpec, TerminationDisposition, REPO_GUEST_PATH, TEMPLATES,
};
pub use state::{ExitInfo, SandboxState};

/// vsock port the guest agent listens on for the daemon's control connection.
pub const GUEST_CONTROL_PORT: u32 = 1024;

/// vsock port the guest agent connects to for proxied network egress.
pub const HOST_PROXY_PORT: u32 = 1025;
