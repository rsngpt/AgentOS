//! Egress policy proxy (milestone M2).
//!
//! The guest has no NIC; when policy allows networking, the guest agent
//! forwards HTTP CONNECT / SOCKS5 traffic over vsock to this module, which
//! applies `NetPolicy` per connection: allowlist matching on the destination
//! hostname (no TLS interception), and an unconditional refusal of loopback,
//! RFC 1918, link-local, and ULA ranges in every mode. Verdicts and byte
//! counts are emitted as `EventKind::NetVerdict` / `ResourceSample`.

use agentos_core::NetPolicy;

/// Per-connection policy decision. Pure function so it is trivially testable;
/// the vsock plumbing around it lands in M2.
#[allow(dead_code)] // called from the M2 proxy loop; exercised by tests today
pub fn verdict(policy: &NetPolicy, dest_host: &str) -> bool {
    if is_local_destination(dest_host) {
        return false;
    }
    match policy {
        NetPolicy::Offline => false,
        NetPolicy::Full => true,
        NetPolicy::Allowlist(patterns) => patterns.iter().any(|p| host_matches(p, dest_host)),
    }
}

/// True for destinations that are never reachable from a sandbox: loopback,
/// private (RFC 1918), link-local, and unique-local ranges, plus name forms
/// that resolve there trivially. DNS-resolution-time enforcement lands in M2;
/// this literal check is the first gate.
fn is_local_destination(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    match host.parse::<std::net::IpAddr>() {
        Ok(ip) => match ip {
            std::net::IpAddr::V4(v4) => {
                v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
            }
            std::net::IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_unspecified()
                    // ULA fc00::/7 and link-local fe80::/10
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        },
        Err(_) => false,
    }
}

/// Match `pattern` against `host`: exact, or a leading `*.` wildcard that
/// matches any subdomain (but not the apex).
fn host_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.len() > suffix.len() + 1
            && host.to_ascii_lowercase().ends_with(&suffix.to_ascii_lowercase())
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
    } else {
        pattern.eq_ignore_ascii_case(host)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ranges_blocked_in_every_mode() {
        for policy in [
            NetPolicy::Full,
            NetPolicy::Allowlist(vec!["localhost".into(), "10.0.0.5".into()]),
        ] {
            assert!(!verdict(&policy, "localhost"));
            assert!(!verdict(&policy, "127.0.0.1"));
            assert!(!verdict(&policy, "10.0.0.5"));
            assert!(!verdict(&policy, "192.168.1.10"));
            assert!(!verdict(&policy, "169.254.0.1"));
            assert!(!verdict(&policy, "::1"));
            assert!(!verdict(&policy, "fd00::1"));
        }
    }

    #[test]
    fn allowlist_matching() {
        let p = NetPolicy::Allowlist(vec!["api.openai.com".into(), "*.github.com".into()]);
        assert!(verdict(&p, "api.openai.com"));
        assert!(verdict(&p, "API.OPENAI.COM"));
        assert!(verdict(&p, "raw.github.com"));
        assert!(!verdict(&p, "github.com")); // wildcard does not match apex
        assert!(!verdict(&p, "evil.com"));
        assert!(!verdict(&p, "notgithub.com"));
    }

    #[test]
    fn offline_blocks_everything() {
        assert!(!verdict(&NetPolicy::Offline, "api.openai.com"));
    }
}
