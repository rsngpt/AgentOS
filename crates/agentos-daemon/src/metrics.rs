//! PRD §8 success metrics, measured locally.
//!
//! **This never sends anything anywhere.** Two of the four metrics — adoption
//! and zero-escape rate — are the kind normally collected by phoning home, and
//! for a product whose entire promise is "your data does not leave this
//! machine" that would be self-defeating: a security tool that quietly opens
//! its own outbound connection has undermined the thing it sells. So the
//! daemon records to `~/.agentos/metrics.jsonl` and `agentos metrics` reads it
//! back. If a fleet ever needs the numbers centrally, the file is line-oriented
//! JSON that an admin can collect with tooling they already trust — an explicit
//! act, not a default.
//!
//! What each metric honestly is:
//!
//! - **Time to boot** — measured directly: VM spawn to guest handshake.
//! - **Performance overhead** — the daemon's own RSS/CPU, sampled on demand.
//! - **Adoption** — installs can't be counted from inside one install. What
//!   this reports is *this* install: a random id, when it was first seen, how
//!   many days it has been used, and how many sandboxes it has run.
//! - **Zero-escape rate** — an escape is a hypervisor compromise, which by
//!   definition we cannot observe from the host side; claiming to measure it
//!   would be theatre. What is real and worth counting is *containment
//!   events*: egress the proxy refused, and how many of those were attempts at
//!   local/LAN addresses — the signature of the lateral movement in PRD §2.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One recorded event. Append-only, one JSON object per line, so a crash can
/// never corrupt more than the last line.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Record {
    /// A sandbox booted. `boot_ms` is spawn → guest handshake.
    SandboxStarted { unix_secs: u64, boot_ms: u64 },
    /// A sandbox reached a terminal state.
    SandboxEnded {
        unix_secs: u64,
        /// `exited` | `killed` | `auto_killed` | `error`
        outcome: String,
    },
    /// The egress proxy refused a connection. `local` marks a destination in
    /// loopback/LAN/metadata space — an attempted lateral move, not just a
    /// blocked download.
    EgressDenied {
        unix_secs: u64,
        local: bool,
    },
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn log_path(home: &Path) -> PathBuf {
    home.join("metrics.jsonl")
}

fn install_id_path(home: &Path) -> PathBuf {
    home.join("install-id")
}

/// A random identifier for this install, created on first use. Local only: it
/// exists so `agentos metrics` can say "this install", not so anyone can be
/// tracked — nothing transmits it.
pub fn install_id(home: &Path) -> String {
    let path = install_id_path(home);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    let fresh = agentos_core::SandboxId::new().to_string();
    let _ = std::fs::create_dir_all(home);
    let _ = std::fs::write(&path, &fresh);
    fresh
}

/// Append one record. Best effort by design: metrics must never be able to fail
/// a run, so every error here is swallowed.
pub fn record(home: &Path, rec: Record) {
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path(home))
    else {
        return;
    };
    if let Ok(line) = serde_json::to_string(&rec) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn started(home: &Path, boot_ms: u64) {
    record(
        home,
        Record::SandboxStarted {
            unix_secs: now_secs(),
            boot_ms,
        },
    );
}

pub fn ended(home: &Path, outcome: &str) {
    record(
        home,
        Record::SandboxEnded {
            unix_secs: now_secs(),
            outcome: outcome.to_string(),
        },
    );
}

pub fn egress_denied(home: &Path, local: bool) {
    record(
        home,
        Record::EgressDenied {
            unix_secs: now_secs(),
            local,
        },
    );
}

/// The computed answer to PRD §8, as `agentos metrics` prints it.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub install_id: String,
    pub first_seen_unix_secs: u64,
    pub active_days: u64,
    pub sandboxes_started: u64,
    pub sandboxes_ended: u64,
    pub outcomes: std::collections::BTreeMap<String, u64>,
    pub boot_ms_p50: u64,
    pub boot_ms_p95: u64,
    pub boot_ms_min: u64,
    pub boot_ms_max: u64,
    /// Connections the proxy refused.
    pub egress_denied: u64,
    /// …of which were aimed at local/LAN/metadata addresses.
    pub egress_denied_local: u64,
    /// Daemon RSS in MiB, sampled now (0 when the daemon isn't running).
    pub daemon_rss_mib: u64,
    pub daemon_cpu_percent: f32,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Fold the log into a summary. Unparseable lines are skipped rather than
/// failing the whole report — a truncated last line shouldn't hide months of
/// data.
pub fn summarize(home: &Path) -> Summary {
    let mut s = Summary {
        install_id: install_id(home),
        ..Default::default()
    };
    let text = std::fs::read_to_string(log_path(home)).unwrap_or_default();
    let mut boots: Vec<u64> = Vec::new();
    let mut days = std::collections::BTreeSet::new();
    for line in text.lines() {
        let Ok(rec) = serde_json::from_str::<Record>(line) else {
            continue;
        };
        let ts = match &rec {
            Record::SandboxStarted { unix_secs, .. }
            | Record::SandboxEnded { unix_secs, .. }
            | Record::EgressDenied { unix_secs, .. } => *unix_secs,
        };
        if ts > 0 {
            days.insert(ts / 86_400);
            if s.first_seen_unix_secs == 0 || ts < s.first_seen_unix_secs {
                s.first_seen_unix_secs = ts;
            }
        }
        match rec {
            Record::SandboxStarted { boot_ms, .. } => {
                s.sandboxes_started += 1;
                boots.push(boot_ms);
            }
            Record::SandboxEnded { outcome, .. } => {
                s.sandboxes_ended += 1;
                *s.outcomes.entry(outcome).or_default() += 1;
            }
            Record::EgressDenied { local, .. } => {
                s.egress_denied += 1;
                if local {
                    s.egress_denied_local += 1;
                }
            }
        }
    }
    s.active_days = days.len() as u64;
    boots.sort_unstable();
    s.boot_ms_p50 = percentile(&boots, 0.50);
    s.boot_ms_p95 = percentile(&boots, 0.95);
    s.boot_ms_min = boots.first().copied().unwrap_or(0);
    s.boot_ms_max = boots.last().copied().unwrap_or(0);
    let (rss, cpu) = daemon_usage();
    s.daemon_rss_mib = rss;
    s.daemon_cpu_percent = cpu;
    s
}

/// PRD §8 "performance overhead": what the idle background process costs.
/// Reads our own process, so it needs no permissions and no other tooling.
fn daemon_usage() -> (u64, f32) {
    #[cfg(unix)]
    {
        let pid = std::process::id();
        if let Ok(out) = std::process::Command::new("ps")
            .args(["-o", "rss=,%cpu=", "-p", &pid.to_string()])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            let mut parts = text.split_whitespace();
            let rss_kib: u64 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
            let cpu: f32 = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0.0);
            return (rss_kib / 1024, cpu);
        }
    }
    (0, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("agentos-metrics-{}", uuid_like()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn uuid_like() -> String {
        agentos_core::SandboxId::new().to_string()
    }

    #[test]
    fn an_empty_history_summarises_to_zeroes_not_an_error() {
        let d = tmpdir();
        let s = summarize(&d);
        assert_eq!(s.sandboxes_started, 0);
        assert_eq!(s.boot_ms_p50, 0);
        assert!(!s.install_id.is_empty());
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn install_id_is_stable_across_calls() {
        let d = tmpdir();
        assert_eq!(install_id(&d), install_id(&d));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn boot_times_and_containment_events_are_counted() {
        let d = tmpdir();
        for ms in [50, 60, 70, 900] {
            started(&d, ms);
        }
        ended(&d, "exited");
        ended(&d, "auto_killed");
        egress_denied(&d, true);
        egress_denied(&d, false);

        let s = summarize(&d);
        assert_eq!(s.sandboxes_started, 4);
        assert_eq!(s.boot_ms_min, 50);
        assert_eq!(s.boot_ms_max, 900);
        assert_eq!(s.egress_denied, 2);
        assert_eq!(s.egress_denied_local, 1, "lateral-move attempts counted apart");
        assert_eq!(s.outcomes.get("auto_killed"), Some(&1));
        std::fs::remove_dir_all(&d).ok();
    }

    /// A half-written last line must not hide the rest of the history.
    #[test]
    fn a_corrupt_line_is_skipped() {
        let d = tmpdir();
        started(&d, 42);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(log_path(&d))
            .unwrap();
        writeln!(f, "{{\"kind\":\"sandbox_st").unwrap();
        drop(f);
        assert_eq!(summarize(&d).sandboxes_started, 1);
        std::fs::remove_dir_all(&d).ok();
    }
}
