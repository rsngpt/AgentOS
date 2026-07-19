//! The guest agent: PID 1 inside every Agent OS microVM.
//!
//! Responsibilities (milestone M1, guest side):
//! 1. Mount /proc, /sys, /dev and the overlay upper layer.
//! 2. Listen on vsock [`agentos_core::GUEST_CONTROL_PORT`] and complete the
//!    `Hello` handshake with the daemon.
//! 3. On `Exec`: mount the announced virtio-fs shares (honoring read-only),
//!    set env + proxy variables per `NetPolicy`, and spawn the agent command.
//! 4. Pump stdio both ways as `Stdin`/`Stdout`/`Stderr` frames; report
//!    `Metrics` periodically; send `Exited` and power off.
//!
//! Built as a static musl binary (`x86_64`/`aarch64-unknown-linux-musl`) and
//! embedded in the rootfs image as `/init`. The vsock/mount implementations
//! are Linux-only and gated below; on other targets this compiles to a stub
//! so `cargo build --workspace` works on any host.

use agentos_core::protocol::PROTOCOL_VERSION;

fn main() {
    #[cfg(target_os = "linux")]
    {
        linux::run();
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "agentos-guest-agent (protocol v{PROTOCOL_VERSION}) only runs inside a Linux guest; \
             this is a host-build stub"
        );
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::PROTOCOL_VERSION;

    pub fn run() {
        // M1: AF_VSOCK listener on GUEST_CONTROL_PORT, length-prefixed JSON
        // framing of agentos_core::protocol messages, mount(2) of virtio-fs
        // tags, fork/exec of the agent command, reboot(RB_POWER_OFF) on exit.
        eprintln!("agentos-guest-agent v{PROTOCOL_VERSION}: guest runtime not implemented yet (M1)");
        std::process::exit(1);
    }
}
