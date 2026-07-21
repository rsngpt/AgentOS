//! Sandbox specification types: what the user grants a sandbox.
//!
//! Everything here is deny-by-default: an empty `SandboxSpec` yields a VM with
//! no host filesystem access and no network.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Unique identifier for a sandbox, stable across the daemon's lifetime.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SandboxId(uuid::Uuid);

impl SandboxId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}

impl Default for SandboxId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SandboxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for SandboxId {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        s.parse::<uuid::Uuid>()
            .map(Self)
            .map_err(|_| Error::InvalidSpec(format!("not a sandbox id: {s}")))
    }
}

/// Access mode for a host directory mounted into the guest.
///
/// Read-only is enforced host-side (the virtio-fs share is opened RO); the
/// guest cannot upgrade its own access.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountMode {
    ReadOnly,
    ReadWrite,
}

/// One host directory exposed to the guest as a virtio-fs share.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountSpec {
    /// Host directory to share. Must be a directory the invoking user can read.
    pub host_path: PathBuf,
    /// Mount point inside the guest. Defaults to `/mnt/<basename>` when not given.
    pub guest_path: PathBuf,
    pub mode: MountMode,
}

impl MountSpec {
    /// Parse the CLI form `HOST_PATH[:ro|rw]` (default `ro`).
    ///
    /// Examples: `./project`, `./project:rw`, `/data/corpus:ro`.
    pub fn parse(s: &str) -> Result<Self> {
        let (path, mode) = match s.rsplit_once(':') {
            Some((p, "ro")) => (p, MountMode::ReadOnly),
            Some((p, "rw")) => (p, MountMode::ReadWrite),
            // A colon that isn't a mode suffix is part of the path.
            _ => (s, MountMode::ReadOnly),
        };
        if path.is_empty() {
            return Err(Error::InvalidSpec(format!("empty mount path in {s:?}")));
        }
        let host_path = PathBuf::from(path);
        let basename = host_path
            .file_name()
            .ok_or_else(|| Error::InvalidSpec(format!("mount path {path:?} has no basename")))?;
        let guest_path = PathBuf::from("/mnt").join(basename);
        Ok(Self {
            host_path,
            guest_path,
            mode,
        })
    }
}

/// Network egress policy for a sandbox.
///
/// The guest never has a NIC; these modes configure the daemon's host-side
/// egress proxy. Loopback, RFC 1918, link-local, and ULA destinations are
/// always refused, in every mode.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "hosts")]
pub enum NetPolicy {
    /// No proxy is offered; the guest cannot open any connection.
    #[default]
    Offline,
    /// Only destinations matching these patterns are reachable.
    /// Patterns are exact hostnames or single-label wildcards (`*.github.com`).
    Allowlist(Vec<String>),
    /// All public destinations reachable; local ranges still blocked.
    Full,
}

impl NetPolicy {
    /// Parse the CLI form: `offline`, `full`, or `allowlist:host1,host2,...`.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "offline" => Ok(Self::Offline),
            "full" => Ok(Self::Full),
            _ => match s.split_once(':') {
                Some(("allowlist", hosts)) => {
                    let hosts: Vec<String> = hosts
                        .split(',')
                        .map(str::trim)
                        .filter(|h| !h.is_empty())
                        .map(String::from)
                        .collect();
                    if hosts.is_empty() {
                        return Err(Error::InvalidSpec(
                            "allowlist requires at least one host, e.g. allowlist:api.openai.com"
                                .into(),
                        ));
                    }
                    Ok(Self::Allowlist(hosts))
                }
                _ => Err(Error::InvalidSpec(format!(
                    "unknown net policy {s:?}; expected offline, full, or allowlist:<hosts>"
                ))),
            },
        }
    }
}

/// Hard resource caps fixed at VM creation; cannot be raised while running.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceLimits {
    pub vcpus: u8,
    pub mem_mib: u32,
    /// Size cap of the writable overlay disk.
    pub disk_mib: u32,
}

impl Default for ResourceLimits {
    fn default() -> Self {
        Self {
            vcpus: 2,
            mem_mib: 2048,
            disk_mib: 8192,
        }
    }
}

/// Conditions under which the daemon kills a sandbox automatically,
/// via the same path as the manual kill switch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoKillRules {
    /// Kill if guest memory use exceeds this (MiB).
    pub max_mem_mib: Option<u32>,
    /// Kill if cumulative network egress exceeds this (MiB).
    pub max_egress_mib: Option<u32>,
    /// Kill after this wall-clock runtime (seconds).
    pub max_runtime_secs: Option<u64>,
}

/// What to do with a sandbox's writable overlay after kill/exit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationDisposition {
    /// Delete the overlay disk (default).
    #[default]
    Wipe,
    /// Keep the overlay under the sandbox directory for forensics.
    Save,
}

/// A git repository to clone into the sandbox.
///
/// The clone happens **host-side** in the daemon (using the host's git and
/// whatever credentials it already has), then the working tree is mounted
/// read-write into the guest. The guest never sees `~/.ssh`, credential
/// helpers, or tokens — only the checked-out files.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSpec {
    /// Clone URL (https or ssh; resolved with host credentials).
    pub url: String,
    /// Branch/tag/commit to check out (default branch when `None`).
    #[serde(default)]
    pub git_ref: Option<String>,
}

/// Guest path the cloned repo is mounted at (and the command's working dir).
pub const REPO_GUEST_PATH: &str = "/workspace";

/// Does allowlist entry `pattern` cover `host`? Exact match, or a leading
/// `*.` wildcard covering any subdomain but **not** the apex.
///
/// Lives here because both the egress proxy (per-connection verdicts) and the
/// fleet policy (is this allowlist within the admin's?) must agree exactly —
/// two implementations of this would be a policy-bypass waiting to happen.
pub fn host_matches(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.len() > suffix.len() + 1
            && host.to_ascii_lowercase().ends_with(&suffix.to_ascii_lowercase())
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
    } else {
        pattern.eq_ignore_ascii_case(host)
    }
}

/// Named starter environments (PRD §7): each presets a network allowlist so
/// the ecosystem's package tooling works without hand-writing `--net`. The
/// runtimes themselves already ship in the guest rootfs.
pub const TEMPLATES: &[(&str, &str)] = &[
    ("python", "pypi.org, files.pythonhosted.org"),
    ("node", "registry.npmjs.org"),
    ("github", "github.com, *.github.com, codeload.github.com"),
];

/// The [`NetPolicy`] for a named template, or an error listing valid names.
pub fn template_net(name: &str) -> Result<NetPolicy> {
    match TEMPLATES.iter().find(|(n, _)| *n == name) {
        Some((_, hosts)) => NetPolicy::parse(&format!("allowlist:{hosts}")),
        None => Err(Error::InvalidSpec(format!(
            "unknown template {name:?}; known: {}",
            TEMPLATES.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
        ))),
    }
}

/// The complete, user-approved grant for one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    /// Human-readable name shown in `agentos ps` and the GUI.
    pub name: String,
    /// Command to exec inside the guest (argv form; argv[0] is the program).
    pub command: Vec<String>,
    /// Extra environment variables for the guest command.
    #[serde(default)]
    pub env: Vec<(String, String)>,
    #[serde(default)]
    pub mounts: Vec<MountSpec>,
    /// Repo to clone host-side and mount at [`REPO_GUEST_PATH`].
    #[serde(default)]
    pub repo: Option<RepoSpec>,
    #[serde(default)]
    pub net: NetPolicy,
    #[serde(default)]
    pub limits: ResourceLimits,
    #[serde(default)]
    pub auto_kill: AutoKillRules,
}

impl SandboxSpec {
    /// A deny-by-default sandbox running `command`: no host files, no network,
    /// default resource limits. Grant capabilities by setting fields on the
    /// result — nothing is implicit.
    ///
    /// ```
    /// # use agentos_core::{SandboxSpec, NetPolicy};
    /// let spec = SandboxSpec::command(["python3", "agent.py"]);
    /// assert_eq!(spec.name, "python3");
    /// assert!(spec.mounts.is_empty());
    /// assert_eq!(spec.net, NetPolicy::Offline);
    /// ```
    pub fn command<I, S>(command: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let command: Vec<String> = command.into_iter().map(Into::into).collect();
        let name = command
            .first()
            .map(|c| c.rsplit('/').next().unwrap_or(c).to_string())
            .unwrap_or_else(|| "agent".to_string());
        Self {
            name,
            command,
            env: Vec::new(),
            mounts: Vec::new(),
            repo: None,
            net: NetPolicy::Offline,
            limits: ResourceLimits::default(),
            auto_kill: AutoKillRules::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_parse_defaults_to_read_only() {
        let m = MountSpec::parse("./project").unwrap();
        assert_eq!(m.mode, MountMode::ReadOnly);
        assert_eq!(m.guest_path, PathBuf::from("/mnt/project"));
    }

    #[test]
    fn mount_parse_rw() {
        let m = MountSpec::parse("/data/repo:rw").unwrap();
        assert_eq!(m.mode, MountMode::ReadWrite);
        assert_eq!(m.host_path, PathBuf::from("/data/repo"));
    }

    #[test]
    fn mount_parse_rejects_empty() {
        assert!(MountSpec::parse(":rw").is_err());
    }

    #[test]
    fn net_policy_parse_variants() {
        assert_eq!(NetPolicy::parse("offline").unwrap(), NetPolicy::Offline);
        assert_eq!(NetPolicy::parse("full").unwrap(), NetPolicy::Full);
        assert_eq!(
            NetPolicy::parse("allowlist:api.openai.com, *.github.com").unwrap(),
            NetPolicy::Allowlist(vec!["api.openai.com".into(), "*.github.com".into()])
        );
        assert!(NetPolicy::parse("allowlist:").is_err());
        assert!(NetPolicy::parse("lan").is_err());
    }

    #[test]
    fn spec_serde_round_trip() {
        let spec = SandboxSpec {
            name: "demo".into(),
            command: vec!["python3".into(), "agent.py".into()],
            env: vec![("OPENAI_API_KEY".into(), "sk-test".into())],
            mounts: vec![MountSpec::parse("./proj:rw").unwrap()],
            repo: Some(RepoSpec {
                url: "https://github.com/example/agent.git".into(),
                git_ref: Some("main".into()),
            }),
            net: NetPolicy::Allowlist(vec!["api.openai.com".into()]),
            limits: ResourceLimits::default(),
            auto_kill: AutoKillRules {
                max_mem_mib: Some(4096),
                ..Default::default()
            },
        };
        let json = serde_json::to_string(&spec).unwrap();
        let back: SandboxSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(spec, back);
    }
}
