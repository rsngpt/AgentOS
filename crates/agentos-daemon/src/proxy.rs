//! Egress policy proxy.
//!
//! The guest has no NIC; its only path out is a vsock connection that the
//! vmhelper bridges to this per-sandbox Unix socket. We speak standard HTTP
//! proxy protocol (CONNECT tunnels + absolute-URI requests), so any tool in
//! the guest honoring `http_proxy`/`https_proxy` works — while every
//! connection is subject to `NetPolicy` *in host code the guest can't touch*:
//!
//! - hostname verdict (offline / allowlist patterns / full)
//! - loopback, RFC 1918, link-local, and ULA destinations always refused —
//!   including after DNS resolution, so a public name can't rebind into the LAN
//! - all transferred bytes counted toward the sandbox's egress quota

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agentos_core::event::EventKind;
use agentos_core::{NetPolicy, SandboxId};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tracing::{debug, info, warn};

use crate::registry::Registry;

const MAX_HEADER: usize = 16 * 1024;

/// Ceiling on simultaneous egress connections per sandbox. The guest is
/// untrusted and can open loopback sockets in a loop; without a cap it would
/// make the host open unbounded outbound TCP — exhausting host descriptors and
/// turning the host into an amplifier against third parties.
const MAX_CONNECTIONS: usize = 64;

/// Serve the egress proxy for one sandbox on `socket_path` until aborted.
pub async fn serve(
    socket_path: &Path,
    policy: NetPolicy,
    bytes_total: Arc<AtomicU64>,
    registry: Registry,
    sandbox: SandboxId,
) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        let (mut conn, _) = listener.accept().await?;
        let policy = policy.clone();
        let bytes = bytes_total.clone();
        let registry = registry.clone();
        let sandbox = sandbox.clone();
        // Refuse rather than queue: queueing lets the guest pin unbounded
        // accepted sockets, which is the exhaustion we're preventing.
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            debug!("egress connection refused: {MAX_CONNECTIONS} already open");
            tokio::spawn(async move {
                let _ = refuse(&mut conn, "503 Service Unavailable").await;
            });
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit; // released when this connection finishes
            if let Err(e) = handle_connection(conn, policy, bytes, registry, sandbox).await {
                debug!(error = %e, "proxy connection ended");
            }
        });
    }
}

async fn handle_connection(
    mut guest: UnixStream,
    policy: NetPolicy,
    bytes_total: Arc<AtomicU64>,
    registry: Registry,
    sandbox: SandboxId,
) -> std::io::Result<()> {
    // Read the request head (everything through \r\n\r\n).
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let head_end = loop {
        let mut chunk = [0u8; 2048];
        let n = guest.read(&mut chunk).await?;
        if n == 0 {
            return Ok(()); // guest gave up
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            break pos;
        }
        if buf.len() > MAX_HEADER {
            return refuse(&mut guest, "431 Request Header Fields Too Large").await;
        }
    };

    let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let (host, port, is_connect) = if method.eq_ignore_ascii_case("CONNECT") {
        let (h, p) = split_host_port(&target, 443);
        (h, p, true)
    } else if let Some(rest) = target.strip_prefix("http://") {
        let authority = rest.split('/').next().unwrap_or_default();
        let (h, p) = split_host_port(authority, 80);
        (h, p, false)
    } else {
        return refuse(&mut guest, "400 Bad Request").await;
    };

    let emit_verdict = |allowed: bool| {
        registry.emit_event(
            sandbox.clone(),
            EventKind::NetVerdict {
                dest_host: host.clone(),
                dest_port: port,
                allowed,
            },
        );
    };

    if host.is_empty() || !verdict(&policy, &host) {
        info!(%host, port, verdict = "deny", "egress");
        emit_verdict(false);
        return refuse(&mut guest, "403 Forbidden").await;
    }

    // Resolve host-side and drop any local addresses (anti-rebinding: a
    // public hostname must not steer the connection into the LAN).
    let addrs: Vec<SocketAddr> = match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(iter) => iter.filter(|a| !is_local_ip(&a.ip())).collect(),
        Err(_) => Vec::new(),
    };
    if addrs.is_empty() {
        info!(%host, port, verdict = "deny-resolved-local-or-unresolvable", "egress");
        emit_verdict(false);
        return refuse(&mut guest, "403 Forbidden").await;
    }

    let mut upstream = match connect_first(&addrs).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%host, port, error = %e, "upstream connect failed");
            return refuse(&mut guest, "502 Bad Gateway").await;
        }
    };
    info!(%host, port, verdict = "allow", "egress");
    emit_verdict(true);

    if is_connect {
        guest
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        // Any bytes past the head already belong to the tunnel.
        if buf.len() > head_end {
            upstream.write_all(&buf[head_end..]).await?;
            bytes_total.fetch_add((buf.len() - head_end) as u64, Ordering::Relaxed);
        }
    } else {
        // Plain HTTP: forward the whole buffered request as-is.
        upstream.write_all(&buf).await?;
        bytes_total.fetch_add(buf.len() as u64, Ordering::Relaxed);
    }

    // Splice both directions, counting incrementally so quota enforcement
    // sees a long transfer *while it is happening*, not at connection close.
    let (mut gr, mut gw) = guest.split();
    let (mut ur, mut uw) = upstream.split();
    let up = async {
        counted_copy(&mut gr, &mut uw, &bytes_total).await;
        let _ = uw.shutdown().await;
    };
    let down = async {
        counted_copy(&mut ur, &mut gw, &bytes_total).await;
        let _ = gw.shutdown().await;
    };
    tokio::join!(up, down);
    Ok(())
}

async fn counted_copy(
    r: &mut (impl tokio::io::AsyncRead + Unpin),
    w: &mut (impl tokio::io::AsyncWrite + Unpin),
    counter: &AtomicU64,
) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if w.write_all(&buf[..n]).await.is_err() {
                    break;
                }
                counter.fetch_add(n as u64, Ordering::Relaxed);
            }
        }
    }
}

/// Per-address budget when dialling upstream. Without this, a single
/// blackholed address (a network advertising IPv6 that silently drops it is
/// common) stalls the request for the OS TCP timeout — minutes — long past any
/// client's patience. Browsers dodge this with Happy Eyeballs; we bound each
/// attempt and move on.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Dial the first address that answers, trying IPv4 before IPv6 and giving each
/// a bounded budget. Guests have no direct network, so every connection is
/// ours to make robust: IPv6-only destinations still work, they just come
/// after the more reliably-routed v4 candidates.
async fn connect_first(addrs: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut ordered: Vec<SocketAddr> = Vec::with_capacity(addrs.len());
    for addr in addrs.iter().filter(|a| a.is_ipv4()).chain(addrs.iter().filter(|a| a.is_ipv6())) {
        if !ordered.contains(addr) {
            ordered.push(*addr); // resolvers routinely hand back duplicates
        }
    }

    let mut last_err = None;
    for addr in ordered {
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => return Ok(stream),
            Ok(Err(e)) => {
                debug!(%addr, error = %e, "upstream candidate refused");
                last_err = Some(e);
            }
            Err(_) => {
                debug!(%addr, "upstream candidate timed out");
                last_err = Some(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("connecting to {addr} timed out after {CONNECT_TIMEOUT:?}"),
                ));
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no usable address")
    }))
}

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

async fn refuse(guest: &mut UnixStream, status: &str) -> std::io::Result<()> {
    guest
        .write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
}

fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
    // Strip any userinfo: in `allowed.com@evil.com` the real host is what
    // follows the LAST '@'. Parsing it off explicitly keeps the policy check
    // looking at the host we actually connect to.
    let authority = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };
    // [v6]:port | host:port | host
    if let Some(rest) = authority.strip_prefix('[') {
        if let Some((h, p)) = rest.split_once(']') {
            let port = p
                .strip_prefix(':')
                .and_then(|p| p.parse().ok())
                .unwrap_or(default_port);
            return (h.to_string(), port);
        }
    }
    match authority.rsplit_once(':') {
        Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !h.contains(':') => {
            (h.to_string(), p.parse().unwrap_or(default_port))
        }
        _ => (authority.to_string(), default_port),
    }
}

/// Per-connection policy decision on the *named* destination.
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

/// True for destinations never reachable from a sandbox, by name or literal IP.
fn is_local_destination(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(|ip| is_local_ip(&ip))
}

/// Every address range a sandbox must never reach: loopback, RFC 1918
/// private, link-local (incl. cloud metadata at 169.254.169.254), carrier-grade
/// NAT, ULA, multicast/broadcast, and the reserved 0.0.0.0/8. Deliberately
/// broad — this is the LAN-lateral-movement guarantee, so err toward blocking.
fn is_local_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                // 0.0.0.0/8 "this network"
                || o[0] == 0
                // 100.64.0.0/10 carrier-grade NAT — internal space on many
                // corporate and cloud networks.
                || (o[0] == 100 && (o[1] & 0xc0) == 64)
                // 192.0.0.0/24 IETF protocol assignments
                || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                // ULA fc00::/7 and link-local fe80::/10
                || (seg0 & 0xfe00) == 0xfc00
                || (seg0 & 0xffc0) == 0xfe80
                // v4-mapped (::ffff:a.b.c.d) and the deprecated v4-compatible
                // (::a.b.c.d) form both re-enter the full v4 checks, so a
                // wrapped 10.0.0.1 or 100.64.x is caught too.
                || v6
                    .to_ipv4_mapped()
                    .or_else(|| match v6.segments() {
                        [0, 0, 0, 0, 0, 0, ..] => v6.to_ipv4(),
                        _ => None,
                    })
                    .is_some_and(|v4| is_local_ip(&IpAddr::V4(v4)))
        }
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
            assert!(!verdict(&policy, "::ffff:192.168.1.1"));
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

    #[test]
    fn authority_parsing() {
        assert_eq!(split_host_port("example.com:8080", 80), ("example.com".into(), 8080));
        assert_eq!(split_host_port("example.com", 80), ("example.com".into(), 80));
        assert_eq!(split_host_port("[::2]:9000", 443), ("::2".into(), 9000));
    }

    /// Userinfo must not be mistaken for the host: the destination of
    /// `allowed.com@evil.com` is evil.com, and policy must judge *that*.
    #[test]
    fn userinfo_is_not_the_host() {
        assert_eq!(
            split_host_port("allowed.com@evil.com", 80),
            ("evil.com".into(), 80)
        );
        assert_eq!(
            split_host_port("user:pass@evil.com:8443", 443),
            ("evil.com".into(), 8443)
        );
        let p = NetPolicy::Allowlist(vec!["allowed.com".into()]);
        let (host, _) = split_host_port("allowed.com@evil.com", 80);
        assert!(!verdict(&p, &host), "userinfo must not smuggle past the allowlist");
    }

    /// The LAN-lateral-movement guarantee: these must be unreachable in every
    /// mode, including via IPv6 wrappers of the same v4 address.
    #[test]
    fn extended_local_ranges_blocked() {
        for addr in [
            "100.64.0.1",         // carrier-grade NAT
            "100.127.255.255",    // CGNAT upper bound
            "169.254.169.254",    // cloud metadata
            "0.0.0.0",
            "0.1.2.3",            // 0.0.0.0/8
            "255.255.255.255",    // broadcast
            "224.0.0.1",          // multicast
            "192.0.0.1",          // IETF protocol assignments
            "::ffff:10.0.0.1",    // v4-mapped private
            "::ffff:100.64.0.1",  // v4-mapped CGNAT
            "ff02::1",            // v6 multicast
        ] {
            assert!(
                !verdict(&NetPolicy::Full, addr),
                "{addr} must be blocked even in full mode"
            );
        }
    }

    /// CGNAT's neighbours are public and must still be reachable — the block
    /// is 100.64.0.0/10, not all of 100.0.0.0/8.
    #[test]
    fn cgnat_block_does_not_overreach() {
        assert!(verdict(&NetPolicy::Full, "100.63.255.255"));
        assert!(verdict(&NetPolicy::Full, "100.128.0.1"));
        assert!(verdict(&NetPolicy::Full, "8.8.8.8"));
    }
}
