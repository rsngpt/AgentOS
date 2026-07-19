//! Host⇄guest control protocol, shared by the daemon and the guest agent.
//!
//! Transport: length-prefixed JSON messages over vsock
//! (see [`crate::GUEST_CONTROL_PORT`]). Each frame is a little-endian `u32`
//! byte length followed by one JSON-encoded message. The daemon speaks
//! [`HostMessage`]; the guest agent replies with [`GuestMessage`].

use serde::{Deserialize, Serialize};

use crate::spec::NetPolicy;
use crate::state::ExitInfo;

/// Protocol revision; both sides refuse to talk across a mismatch.
pub const PROTOCOL_VERSION: u32 = 1;

/// Messages sent daemon → guest agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum HostMessage {
    /// First message after connect; guest must answer with `Hello`.
    Hello { version: u32 },
    /// Mount configured shares, apply env + proxy settings, exec the command.
    Exec {
        command: Vec<String>,
        env: Vec<(String, String)>,
        /// Shares to mount, as (virtio-fs tag, guest path, read_only).
        mounts: Vec<(String, String, bool)>,
        /// Determines whether the guest configures proxy env vars at all.
        net: NetPolicy,
    },
    /// Stdin bytes for the running command.
    Stdin { data: Vec<u8> },
    /// Politely ask the command to stop (SIGTERM in-guest). The kill switch
    /// never uses this — it destroys the VMM process from outside.
    Terminate,
}

/// Messages sent guest agent → daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum GuestMessage {
    /// Handshake reply; sent once the guest is ready for `Exec`.
    Hello { version: u32 },
    Stdout { data: Vec<u8> },
    Stderr { data: Vec<u8> },
    /// The command finished; final message on a healthy connection.
    Exited { info: ExitInfo },
    /// Periodic guest-side resource report (advisory; host measurements win).
    Metrics { mem_mib: u32, disk_used_mib: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_round_trip() {
        let msgs = [
            HostMessage::Hello {
                version: PROTOCOL_VERSION,
            },
            HostMessage::Exec {
                command: vec!["sh".into(), "-c".into(), "echo hi".into()],
                env: vec![],
                mounts: vec![("share0".into(), "/mnt/project".into(), true)],
                net: NetPolicy::Offline,
            },
        ];
        for m in msgs {
            let json = serde_json::to_string(&m).unwrap();
            assert_eq!(m, serde_json::from_str(&json).unwrap());
        }

        let g = GuestMessage::Exited {
            info: ExitInfo {
                code: Some(0),
                signal: None,
            },
        };
        let json = serde_json::to_string(&g).unwrap();
        assert_eq!(g, serde_json::from_str(&json).unwrap());
    }
}
