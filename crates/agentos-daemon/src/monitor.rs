//! Resource monitor & auto-kill.
//!
//! One `watch` task per running sandbox samples guest-reported memory
//! (advisory), proxy egress byte counters (host truth), and wall-clock
//! runtime, and fires the kill switch when an `AutoKillRules` threshold is
//! crossed. The task is aborted by the run orchestration when the sandbox
//! terminates.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use agentos_core::{AutoKillRules, SandboxId, TerminationDisposition};
use tracing::warn;

use crate::registry::Registry;

/// Evaluate rules against one sample. Returns the name of the rule that
/// fired, if any — recorded in the event log and the `Killed` state.
pub fn breached_rule(
    rules: &AutoKillRules,
    mem_mib: u32,
    egress_mib: u64,
    runtime_secs: u64,
) -> Option<&'static str> {
    if rules.max_mem_mib.is_some_and(|max| mem_mib > max) {
        return Some("max_mem_mib");
    }
    if rules.max_egress_mib.is_some_and(|max| egress_mib > u64::from(max)) {
        return Some("max_egress_mib");
    }
    if rules.max_runtime_secs.is_some_and(|max| runtime_secs > max) {
        return Some("max_runtime_secs");
    }
    None
}

/// Sample once a second; kill through the registry (the same absolute path
/// as the manual kill switch) when a rule fires.
pub async fn watch(
    registry: Registry,
    id: SandboxId,
    rules: AutoKillRules,
    guest_mem_mib: Arc<AtomicU32>,
    egress_bytes: Arc<AtomicU64>,
) {
    if rules == AutoKillRules::default() {
        return; // nothing to enforce
    }
    let started = Instant::now();
    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let mem = guest_mem_mib.load(Ordering::Relaxed);
        let egress_mib = egress_bytes.load(Ordering::Relaxed) / (1024 * 1024);
        let runtime = started.elapsed().as_secs();
        if let Some(rule) = breached_rule(&rules, mem, egress_mib, runtime) {
            warn!(%id, rule, mem, egress_mib, runtime, "auto-kill rule fired");
            if let Err(e) = registry
                .kill(&id, &format!("auto-kill: {rule}"), TerminationDisposition::Wipe)
                .await
            {
                warn!(%id, error = %e, "auto-kill failed");
            }
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_rules_never_fires() {
        assert_eq!(breached_rule(&AutoKillRules::default(), 99999, 99999, 99999), None);
    }

    #[test]
    fn first_breached_rule_wins() {
        let rules = AutoKillRules {
            max_mem_mib: Some(4096),
            max_egress_mib: Some(1024),
            max_runtime_secs: None,
        };
        assert_eq!(breached_rule(&rules, 5000, 0, 0), Some("max_mem_mib"));
        assert_eq!(breached_rule(&rules, 100, 2048, 0), Some("max_egress_mib"));
        assert_eq!(breached_rule(&rules, 100, 100, 0), None);
    }
}
