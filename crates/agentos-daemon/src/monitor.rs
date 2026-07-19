//! Resource monitor & auto-kill (milestone M2).
//!
//! A per-sandbox sampling loop combines host-truth VMM stats
//! (`VmHandle::stats`), guest-advisory metrics (`GuestMessage::Metrics`),
//! and proxy egress counters, emits `EventKind::ResourceSample`, and fires
//! the kill switch when an `AutoKillRules` threshold is crossed.

use agentos_core::AutoKillRules;

/// Evaluate rules against one sample. Returns the name of the rule that
/// fired, if any — recorded in the event log and the `Killed` state.
#[allow(dead_code)] // called from the M2 monitor loop; exercised by tests today
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
