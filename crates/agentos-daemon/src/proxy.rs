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
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use agentos_core::NetPolicy;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpStream, UnixListener, UnixStream};
use tracing::{debug, info, warn};

const MAX_HEADER: usize = 16 * 1024;

/// Serve the egress proxy for one sandbox on `socket_path` until aborted.
pub async fn serve(
    socket_path: &Path,
    policy: NetPolicy,
    bytes_total: Arc<AtomicU64>,
) -> std::io::Result<()> {
    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path)?;
    loop {
        let (conn, _) = listener.accept().await?;
        let policy = policy.clone();
        let bytes = bytes_total.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(conn, policy, bytes).await {
                debug!(error = %e, "proxy connection ended");
            }
        });
    }
}

async fn handle_connection(
    mut guest: UnixStream,
    policy: NetPolicy,
    bytes_total: Arc<AtomicU64>,
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

    if host.is_empty() || !verdict(&policy, &host) {
        info!(%host, port, verdict = "deny", "egress");
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
        return refuse(&mut guest, "403 Forbidden").await;
    }

    let mut upstream = match TcpStream::connect(addrs.as_slice()).await {
        Ok(s) => s,
        Err(e) => {
            warn!(%host, port, error = %e, "upstream connect failed");
            return refuse(&mut guest, "502 Bad Gateway").await;
        }
    };
    info!(%host, port, verdict = "allow", "egress");

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

fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

async fn refuse(guest: &mut UnixStream, status: &str) -> std::io::Result<()> {
    guest
        .write_all(format!("HTTP/1.1 {status}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
}

fn split_host_port(authority: &str, default_port: u16) -> (String, u16) {
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

/// Loopback, private (RFC 1918), link-local, ULA, and unspecified ranges.
fn is_local_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // ULA fc00::/7 and link-local fe80::/10
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // v4-mapped
                || v6.to_ipv4_mapped().is_some_and(|v4| {
                    v4.is_loopback() || v4.is_private() || v4.is_link_local() || v4.is_unspecified()
                })
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
}
