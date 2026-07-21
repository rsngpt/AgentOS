//! Enterprise fleet policy (PRD §7): machine-wide limits an IT admin sets
//! once, which every sandbox on the machine is then held to.
//!
//! **Where it is enforced matters.** The policy is applied inside `agentosd`
//! when a spec arrives — never in the CLI or GUI — because a user can run
//! their own client, or none at all. Anything a client could skip is not a
//! control.
//!
//! **What it is and isn't.** This is a guardrail for managed fleets, not a
//! security boundary against the machine's owner: `agentosd` runs as the user,
//! so someone with local admin can replace the binary or the policy file. It
//! stops an *agent* (and a careless user) from exceeding what IT allows; it
//! does not stop a determined owner from disabling Agent OS altogether. Deploy
//! the policy file root-owned and read-only for real assurance.
//!
//! A policy can only ever *tighten* what a sandbox asks for. Requests that
//! exceed a hard limit (network, mounts, git) are **refused** with a message
//! naming the policy, so the user knows why. Resource requests above a cap are
//! **clamped**, matching how quotas already behave.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::spec::{host_matches, MountMode, NetPolicy, SandboxSpec};

/// System-wide policy file. Root-owned and read-only in a managed deployment.
#[cfg(target_os = "macos")]
pub const SYSTEM_POLICY_PATH: &str = "/Library/Application Support/AgentOS/policy.json";
#[cfg(not(target_os = "macos"))]
pub const SYSTEM_POLICY_PATH: &str = "/etc/agentos/policy.json";

/// Limits every sandbox on this machine is held to. Absent fields impose
/// nothing, so an empty `{}` is a valid no-op policy.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FleetPolicy {
    /// Shown to users when the policy refuses something ("contact IT" etc).
    #[serde(default)]
    pub message: Option<String>,

    /// Most network a sandbox may have. `Offline` bans networking entirely;
    /// `Allowlist(hosts)` means a sandbox's allowlist must be a subset of it;
    /// `Full` (or absent) imposes no ceiling.
    #[serde(default)]
    pub max_net: Option<NetPolicy>,

    /// Paths that may never be mounted, matched against the *canonical* host
    /// path and covering everything beneath them (e.g. `~/.ssh`, `/Users`).
    #[serde(default)]
    pub deny_mounts: Vec<PathBuf>,

    /// Force every mount read-only regardless of what was requested.
    #[serde(default)]
    pub force_mounts_read_only: bool,

    /// Refuse `--repo` (host-side clones) entirely.
    #[serde(default)]
    pub allow_repo: Option<bool>,

    #[serde(default)]
    pub max_vcpus: Option<u8>,
    #[serde(default)]
    pub max_mem_mib: Option<u32>,
    #[serde(default)]
    pub max_disk_mib: Option<u32>,

    /// Auto-kill ceilings the sandbox may not exceed or omit. A sandbox with
    /// no rule gets the policy's; one with a looser rule is tightened.
    #[serde(default)]
    pub max_runtime_secs: Option<u64>,
    #[serde(default)]
    pub max_egress_mib: Option<u32>,
}

impl FleetPolicy {
    /// Load the machine's policy.
    ///
    /// The system path wins whenever it exists. `AGENTOS_POLICY` is honoured
    /// **only** in its absence — that keeps the override usable for testing
    /// and for per-user experimentation without letting an env var weaken a
    /// policy IT has actually deployed.
    pub fn load() -> Result<Self> {
        let system = Path::new(SYSTEM_POLICY_PATH);
        if system.exists() {
            return Self::from_file(system);
        }
        match std::env::var_os("AGENTOS_POLICY") {
            Some(p) if !p.is_empty() => Self::from_file(Path::new(&p)),
            _ => Ok(Self::default()),
        }
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            Error::InvalidSpec(format!("reading fleet policy {}: {e}", path.display()))
        })?;
        serde_json::from_str(&text).map_err(|e| {
            Error::InvalidSpec(format!("parsing fleet policy {}: {e}", path.display()))
        })
    }

    /// True when nothing is constrained.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn refuse(&self, what: &str) -> Error {
        let mut msg = format!("fleet policy forbids {what}");
        if let Some(extra) = &self.message {
            msg.push_str(": ");
            msg.push_str(extra);
        }
        Error::InvalidSpec(msg)
    }

    /// Hold `spec` to this policy, refusing what exceeds a hard limit and
    /// clamping what merely exceeds a cap. Mount paths must already be
    /// canonical — the daemon canonicalises before calling this, so a symlink
    /// cannot smuggle a denied path past `deny_mounts`.
    pub fn apply(&self, mut spec: SandboxSpec) -> Result<SandboxSpec> {
        // --- network ceiling ---
        if let Some(ceiling) = &self.max_net {
            spec.net = self.clamp_net(ceiling, &spec.net)?;
        }

        // --- mounts ---
        // Mount paths arrive canonical (the daemon resolves them so a symlink
        // can't disguise a denied path), so the deny list must be canonicalised
        // too — otherwise an admin writing `/tmp/x` on macOS, where /tmp is a
        // symlink to /private/tmp, would silently deny nothing.
        for m in &spec.mounts {
            if let Some(denied) = self.deny_mounts.iter().find(|d| {
                let canonical = d.canonicalize();
                let d_resolved = canonical.as_deref().unwrap_or(d.as_path());
                m.host_path == *d_resolved
                    || m.host_path.starts_with(d_resolved)
                    // Also compare literally, so a policy naming a path that
                    // doesn't exist on this machine still applies if it later does.
                    || m.host_path == **d
                    || m.host_path.starts_with(d)
            }) {
                return Err(self.refuse(&format!(
                    "mounting {} (under {})",
                    m.host_path.display(),
                    denied.display()
                )));
            }
        }
        if self.force_mounts_read_only {
            for m in &mut spec.mounts {
                m.mode = MountMode::ReadOnly;
            }
        }

        // --- git ---
        if spec.repo.is_some() && self.allow_repo == Some(false) {
            return Err(self.refuse("cloning git repositories"));
        }

        // --- resource caps: clamp, matching quota semantics ---
        if let Some(max) = self.max_vcpus {
            spec.limits.vcpus = spec.limits.vcpus.min(max);
        }
        if let Some(max) = self.max_mem_mib {
            spec.limits.mem_mib = spec.limits.mem_mib.min(max);
        }
        if let Some(max) = self.max_disk_mib {
            spec.limits.disk_mib = spec.limits.disk_mib.min(max);
        }

        // --- auto-kill floors: tighten, and supply when absent ---
        spec.auto_kill.max_runtime_secs =
            tighten(spec.auto_kill.max_runtime_secs, self.max_runtime_secs);
        spec.auto_kill.max_egress_mib =
            tighten(spec.auto_kill.max_egress_mib, self.max_egress_mib);

        Ok(spec)
    }

    /// The requested policy, or an error when it exceeds the ceiling.
    fn clamp_net(&self, ceiling: &NetPolicy, requested: &NetPolicy) -> Result<NetPolicy> {
        match (ceiling, requested) {
            // Offline permits only offline.
            (NetPolicy::Offline, NetPolicy::Offline) => Ok(NetPolicy::Offline),
            (NetPolicy::Offline, _) => Err(self.refuse("network access")),

            // Full imposes no ceiling.
            (NetPolicy::Full, r) => Ok(r.clone()),

            // An allowlist ceiling permits offline, or a subset of itself.
            (NetPolicy::Allowlist(_), NetPolicy::Offline) => Ok(NetPolicy::Offline),
            (NetPolicy::Allowlist(_), NetPolicy::Full) => {
                Err(self.refuse("unrestricted network access"))
            }
            (NetPolicy::Allowlist(allowed), NetPolicy::Allowlist(wanted)) => {
                for host in wanted {
                    let covered = allowed
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(host) || host_matches(a, host));
                    if !covered {
                        return Err(self.refuse(&format!("network access to {host}")));
                    }
                }
                Ok(NetPolicy::Allowlist(wanted.clone()))
            }
        }
    }
}

/// The stricter of a requested limit and a policy limit, preferring whichever
/// exists when only one does.
fn tighten<T: Ord>(requested: Option<T>, policy: Option<T>) -> Option<T> {
    match (requested, policy) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{MountSpec, RepoSpec, ResourceLimits};

    fn spec() -> SandboxSpec {
        SandboxSpec {
            name: "t".into(),
            command: vec!["true".into()],
            env: vec![],
            mounts: vec![],
            repo: None,
            template: None,
            net: NetPolicy::Offline,
            limits: ResourceLimits::default(),
            auto_kill: Default::default(),
        }
    }

    #[test]
    fn empty_policy_changes_nothing() {
        let s = spec();
        assert_eq!(FleetPolicy::default().apply(s.clone()).unwrap(), s);
    }

    #[test]
    fn offline_ceiling_refuses_any_network() {
        let p = FleetPolicy {
            max_net: Some(NetPolicy::Offline),
            message: Some("ask IT".into()),
            ..Default::default()
        };
        let mut s = spec();
        s.net = NetPolicy::Full;
        let err = p.apply(s).unwrap_err().to_string();
        assert!(err.contains("forbids network access"), "{err}");
        assert!(err.contains("ask IT"), "policy message should reach the user: {err}");

        // Offline is still fine.
        assert!(p.apply(spec()).is_ok());
    }

    #[test]
    fn allowlist_ceiling_permits_only_subsets() {
        let p = FleetPolicy {
            max_net: Some(NetPolicy::Allowlist(vec![
                "*.github.com".into(),
                "pypi.org".into(),
            ])),
            ..Default::default()
        };

        let mut ok = spec();
        ok.net = NetPolicy::Allowlist(vec!["api.github.com".into(), "pypi.org".into()]);
        assert!(p.apply(ok).is_ok());

        let mut bad = spec();
        bad.net = NetPolicy::Allowlist(vec!["evil.com".into()]);
        assert!(p.apply(bad).unwrap_err().to_string().contains("evil.com"));

        // Full exceeds an allowlist ceiling.
        let mut full = spec();
        full.net = NetPolicy::Full;
        assert!(p.apply(full).is_err());

        // Offline is always under any ceiling.
        assert!(p.apply(spec()).is_ok());
    }

    #[test]
    fn denied_mounts_cover_subdirectories() {
        let p = FleetPolicy {
            deny_mounts: vec![PathBuf::from("/Users/me/.ssh")],
            ..Default::default()
        };
        let mut s = spec();
        s.mounts = vec![MountSpec {
            host_path: PathBuf::from("/Users/me/.ssh/keys"),
            guest_path: PathBuf::from("/mnt/k"),
            mode: MountMode::ReadWrite,
        }];
        assert!(p.apply(s).unwrap_err().to_string().contains(".ssh"));
    }

    /// The daemon hands us canonical mount paths, so a deny entry written
    /// through a symlink (/tmp on macOS) must still bite. Getting this wrong
    /// makes the policy silently permit what it claims to deny.
    #[test]
    fn denied_mounts_match_through_symlinks() {
        let dir = std::env::temp_dir().join("agentos-policy-symlink-test");
        std::fs::create_dir_all(&dir).unwrap();
        let canonical = dir.canonicalize().unwrap();

        let p = FleetPolicy {
            // The admin writes the pre-symlink path…
            deny_mounts: vec![dir.clone()],
            ..Default::default()
        };
        let mut s = spec();
        // …while the daemon supplies the resolved one.
        s.mounts = vec![MountSpec {
            host_path: canonical,
            guest_path: PathBuf::from("/mnt/x"),
            mode: MountMode::ReadOnly,
        }];
        assert!(
            p.apply(s).is_err(),
            "deny_mounts must resolve symlinks or it protects nothing"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn force_read_only_downgrades_mounts() {
        let p = FleetPolicy {
            force_mounts_read_only: true,
            ..Default::default()
        };
        let mut s = spec();
        s.mounts = vec![MountSpec {
            host_path: PathBuf::from("/tmp/x"),
            guest_path: PathBuf::from("/mnt/x"),
            mode: MountMode::ReadWrite,
        }];
        let out = p.apply(s).unwrap();
        assert_eq!(out.mounts[0].mode, MountMode::ReadOnly);
    }

    #[test]
    fn repo_can_be_banned() {
        let p = FleetPolicy {
            allow_repo: Some(false),
            ..Default::default()
        };
        let mut s = spec();
        s.repo = Some(RepoSpec {
            url: "https://x/y.git".into(),
            git_ref: None,
        });
        assert!(p.apply(s).unwrap_err().to_string().contains("git"));
    }

    #[test]
    fn resources_clamp_and_auto_kill_tightens() {
        let p = FleetPolicy {
            max_vcpus: Some(2),
            max_mem_mib: Some(1024),
            max_runtime_secs: Some(60),
            max_egress_mib: Some(100),
            ..Default::default()
        };
        let mut s = spec();
        s.limits = ResourceLimits {
            vcpus: 8,
            mem_mib: 8192,
            disk_mib: 4096,
        };
        s.auto_kill.max_runtime_secs = Some(9999); // looser than policy
        let out = p.apply(s).unwrap();
        assert_eq!(out.limits.vcpus, 2);
        assert_eq!(out.limits.mem_mib, 1024);
        assert_eq!(out.limits.disk_mib, 4096, "uncapped limits are untouched");
        assert_eq!(out.auto_kill.max_runtime_secs, Some(60), "policy wins when stricter");
        assert_eq!(out.auto_kill.max_egress_mib, Some(100), "policy supplies a missing rule");
    }

    #[test]
    fn a_stricter_request_is_kept() {
        let p = FleetPolicy {
            max_runtime_secs: Some(600),
            ..Default::default()
        };
        let mut s = spec();
        s.auto_kill.max_runtime_secs = Some(30);
        assert_eq!(p.apply(s).unwrap().auto_kill.max_runtime_secs, Some(30));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        // A typo'd key must not silently disable a limit the admin intended.
        let err = serde_json::from_str::<FleetPolicy>(r#"{"max_netz":"offline"}"#);
        assert!(err.is_err());
    }
}
