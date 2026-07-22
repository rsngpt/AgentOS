//! Permissions a running sandbox's grants can be changed through.
//!
//! The PRD's dashboard lets a user "adjust permission sliders", which only
//! means anything if it works on an agent that is already running. Two of the
//! three grant kinds can:
//!
//! - **network policy** — enforced entirely host-side in `proxy.rs`, which
//!   consults this on every connection, so a tightened allowlist takes effect
//!   on the *next* connection the guest opens;
//! - **auto-kill rules** — read by `monitor.rs` on each 1s tick.
//!
//! **Mounts cannot.** The virtio-fs device set is fixed when the VM is created,
//! and the guest has already mounted the shares; revoking one would need device
//! hot-unplug that neither backend exposes. `set_permissions` refuses a mount
//! change outright rather than accepting it and silently not enforcing it — a
//! permission UI that lies is worse than one that says no.
//!
//! Read under a plain `std::sync::RwLock`: the values are tiny, every holder
//! clones and drops the guard immediately, and no lock is ever held across an
//! `.await`.

use std::sync::RwLock;

use agentos_core::{AutoKillRules, NetPolicy};

/// The mutable half of a sandbox's grants, shared between the run task, its
/// egress proxy, and its resource monitor.
#[derive(Debug)]
pub struct LivePermissions {
    net: RwLock<NetPolicy>,
    auto_kill: RwLock<AutoKillRules>,
}

impl LivePermissions {
    pub fn new(net: NetPolicy, auto_kill: AutoKillRules) -> Self {
        Self {
            net: RwLock::new(net),
            auto_kill: RwLock::new(auto_kill),
        }
    }

    /// The policy to judge one egress connection by. Cloned so the lock is
    /// released before any I/O happens.
    pub fn net(&self) -> NetPolicy {
        self.net.read().expect("net policy lock").clone()
    }

    pub fn auto_kill(&self) -> AutoKillRules {
        *self.auto_kill.read().expect("auto-kill lock")
    }

    pub fn set_net(&self, policy: NetPolicy) {
        *self.net.write().expect("net policy lock") = policy;
    }

    pub fn set_auto_kill(&self, rules: AutoKillRules) {
        *self.auto_kill.write().expect("auto-kill lock") = rules;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tightening_network_is_visible_to_the_next_reader() {
        let live = LivePermissions::new(NetPolicy::Full, AutoKillRules::default());
        assert!(matches!(live.net(), NetPolicy::Full));
        live.set_net(NetPolicy::Offline);
        // What the proxy would see on the next connection.
        assert!(matches!(live.net(), NetPolicy::Offline));
    }

    #[test]
    fn auto_kill_rules_can_be_tightened_mid_run() {
        let live = LivePermissions::new(NetPolicy::Offline, AutoKillRules::default());
        assert_eq!(live.auto_kill().max_runtime_secs, None);
        live.set_auto_kill(AutoKillRules {
            max_runtime_secs: Some(5),
            ..Default::default()
        });
        assert_eq!(live.auto_kill().max_runtime_secs, Some(5));
    }
}
