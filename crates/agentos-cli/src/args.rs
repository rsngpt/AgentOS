//! CLI argument surface and its conversion into a `SandboxSpec`.

use agentos_core::{
    AutoKillRules, MountSpec, NetPolicy, ResourceLimits, Result, SandboxSpec,
};
use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agentos",
    about = "Run AI agents in hardware-isolated microVM sandboxes",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
// Run carries the whole flag surface; the other variants are tiny. Boxing an
// `Args` variant isn't supported by clap's derive, so accept the size gap.
#[allow(clippy::large_enum_variant)]
pub enum Command {
    /// Boot a sandbox and run a command in it, attached to its stdio.
    Run(RunArgs),
    /// List sandboxes and their states.
    Ps,
    /// The kill switch: terminate a sandbox immediately.
    Kill {
        /// Sandbox id (from `agentos ps`).
        id: String,
        /// Keep the overlay disk for debugging instead of wiping it.
        #[arg(long)]
        save: bool,
    },
    /// Freeze a running sandbox's vCPUs, keeping its VM and grants intact.
    Pause {
        /// Sandbox id (from `agentos ps`).
        id: String,
    },
    /// Resume a paused sandbox exactly where it left off.
    Resume {
        /// Sandbox id (from `agentos ps`).
        id: String,
    },
    /// Save a sandbox's VM state to disk and tear the VM down. Restore it
    /// later with `agentos restore` — the command picks up mid-task.
    Snapshot {
        /// Sandbox id (from `agentos ps`).
        id: String,
    },
    /// Bring a snapshotted sandbox back and stream its output.
    Restore {
        /// Sandbox id (from `agentos ps`).
        id: String,
    },
    /// Show the fleet policy this machine enforces, if any.
    Policy,
    /// Stream daemon events (state changes, network verdicts, resource samples).
    Events,
}

#[derive(Args)]
pub struct RunArgs {
    /// Name shown in `agentos ps` (defaults to the command's program name).
    #[arg(long)]
    pub name: Option<String>,

    /// Mount a host directory: PATH[:ro|rw]. Repeatable. Default mode: ro.
    /// Nothing is mounted unless requested.
    #[arg(long = "mount", value_name = "PATH[:ro|rw]")]
    pub mounts: Vec<String>,

    /// Clone a git repo host-side (with your credentials) and mount it at
    /// /workspace — SSH keys and tokens never enter the guest.
    #[arg(long = "repo", value_name = "URL")]
    pub repo: Option<String>,

    /// Branch/tag/commit to check out with --repo (default branch otherwise).
    #[arg(long = "branch", value_name = "REF", requires = "repo")]
    pub branch: Option<String>,

    /// Starter environment presetting a network allowlist for its ecosystem:
    /// python, node, or github. An explicit --net overrides it.
    #[arg(long = "template", value_name = "NAME")]
    pub template: Option<String>,

    /// Network policy: offline (default), full, or allowlist:host1,host2.
    /// Localhost and LAN destinations are always blocked.
    #[arg(long = "net", default_value = "offline", value_name = "POLICY")]
    pub net: String,

    /// vCPUs for the sandbox.
    #[arg(long, default_value_t = ResourceLimits::default().vcpus)]
    pub vcpus: u8,

    /// RAM cap in MiB.
    #[arg(long = "mem", default_value_t = ResourceLimits::default().mem_mib, value_name = "MIB")]
    pub mem_mib: u32,

    /// Overlay disk cap in MiB.
    #[arg(long = "disk", default_value_t = ResourceLimits::default().disk_mib, value_name = "MIB")]
    pub disk_mib: u32,

    /// Auto-kill if guest memory exceeds this many MiB.
    #[arg(long, value_name = "MIB")]
    pub kill_over_mem: Option<u32>,

    /// Auto-kill if total egress exceeds this many MiB.
    #[arg(long, value_name = "MIB")]
    pub kill_over_egress: Option<u32>,

    /// Auto-kill after this many seconds of runtime.
    #[arg(long, value_name = "SECS")]
    pub kill_after_secs: Option<u64>,

    /// Pass an environment variable into the guest: KEY=VALUE. Repeatable.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,

    /// The agent command to run inside the sandbox.
    #[arg(required = true, last = true)]
    pub command: Vec<String>,
}

impl RunArgs {
    pub fn into_spec(self) -> Result<SandboxSpec> {
        let mounts = self
            .mounts
            .iter()
            .map(|m| MountSpec::parse(m))
            .collect::<Result<Vec<_>>>()?;
        // A template presets the network allowlist; an explicit --net wins.
        // Resolve the template regardless so an unknown name is rejected.
        let net = match &self.template {
            Some(t) => {
                let template_net = agentos_core::template_net(t)?;
                if self.net == "offline" {
                    template_net
                } else {
                    NetPolicy::parse(&self.net)?
                }
            }
            None => NetPolicy::parse(&self.net)?,
        };
        let env = self
            .env
            .iter()
            .map(|kv| {
                kv.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .ok_or_else(|| {
                        agentos_core::Error::InvalidSpec(format!(
                            "--env expects KEY=VALUE, got {kv:?}"
                        ))
                    })
            })
            .collect::<Result<Vec<_>>>()?;
        let name = self
            .name
            .unwrap_or_else(|| self.command[0].rsplit('/').next().unwrap_or("agent").to_string());

        let repo = self.repo.map(|url| agentos_core::RepoSpec {
            url,
            git_ref: self.branch,
        });

        Ok(SandboxSpec {
            name,
            command: self.command,
            env,
            mounts,
            repo,
            net,
            limits: ResourceLimits {
                vcpus: self.vcpus,
                mem_mib: self.mem_mib,
                disk_mib: self.disk_mib,
            },
            auto_kill: AutoKillRules {
                max_mem_mib: self.kill_over_mem,
                max_egress_mib: self.kill_over_egress,
                max_runtime_secs: self.kill_after_secs,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::MountMode;

    fn parse_run(argv: &[&str]) -> RunArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Run(r) => r,
            _ => panic!("expected run"),
        }
    }

    #[test]
    fn full_flag_surface_maps_to_spec() {
        let run = parse_run(&[
            "agentos",
            "run",
            "--mount",
            "./project:rw",
            "--mount",
            "/data/corpus",
            "--net",
            "allowlist:api.openai.com,*.github.com",
            "--vcpus",
            "4",
            "--mem",
            "4096",
            "--kill-over-mem",
            "3500",
            "--env",
            "FOO=bar",
            "--",
            "python3",
            "agent.py",
        ]);
        let spec = run.into_spec().unwrap();
        assert_eq!(spec.name, "python3");
        assert_eq!(spec.command, vec!["python3", "agent.py"]);
        assert_eq!(spec.mounts.len(), 2);
        assert_eq!(spec.mounts[0].mode, MountMode::ReadWrite);
        assert_eq!(spec.mounts[1].mode, MountMode::ReadOnly);
        assert_eq!(
            spec.net,
            NetPolicy::Allowlist(vec!["api.openai.com".into(), "*.github.com".into()])
        );
        assert_eq!(spec.limits.vcpus, 4);
        assert_eq!(spec.limits.mem_mib, 4096);
        assert_eq!(spec.auto_kill.max_mem_mib, Some(3500));
        assert_eq!(spec.env, vec![("FOO".to_string(), "bar".to_string())]);
    }

    #[test]
    fn defaults_are_deny_by_default() {
        let spec = parse_run(&["agentos", "run", "--", "echo", "hi"]).into_spec().unwrap();
        assert!(spec.mounts.is_empty());
        assert_eq!(spec.net, NetPolicy::Offline);
        assert!(spec.repo.is_none());
    }

    #[test]
    fn repo_with_branch() {
        let spec = parse_run(&[
            "agentos", "run", "--repo", "https://x/y.git", "--branch", "dev", "--", "make",
        ])
        .into_spec()
        .unwrap();
        let repo = spec.repo.unwrap();
        assert_eq!(repo.url, "https://x/y.git");
        assert_eq!(repo.git_ref.as_deref(), Some("dev"));
    }

    #[test]
    fn branch_requires_repo() {
        assert!(Cli::try_parse_from(["agentos", "run", "--branch", "dev", "--", "make"]).is_err());
    }

    #[test]
    fn template_presets_net_allowlist() {
        let spec = parse_run(&["agentos", "run", "--template", "python", "--", "pip", "install", "x"])
            .into_spec()
            .unwrap();
        match spec.net {
            NetPolicy::Allowlist(hosts) => assert!(hosts.iter().any(|h| h == "pypi.org")),
            other => panic!("expected allowlist, got {other:?}"),
        }
    }

    #[test]
    fn explicit_net_overrides_template() {
        let spec = parse_run(&["agentos", "run", "--template", "python", "--net", "full", "--", "x"])
            .into_spec()
            .unwrap();
        assert_eq!(spec.net, NetPolicy::Full);
    }

    #[test]
    fn unknown_template_rejected() {
        assert!(parse_run(&["agentos", "run", "--template", "cobol", "--", "x"])
            .into_spec()
            .is_err());
    }

    #[test]
    fn bad_env_rejected() {
        let run = parse_run(&["agentos", "run", "--env", "NOEQUALS", "--", "true"]);
        assert!(run.into_spec().is_err());
    }
}
