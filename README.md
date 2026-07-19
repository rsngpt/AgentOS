# Agent OS

Run untrusted AI agents in hardware-isolated microVM sandboxes on your own machine — granular filesystem grants, policy-controlled networking, and an absolute kill switch.

**Start with [ARCHITECTURE.md](ARCHITECTURE.md)** — the technical design (threat model, hypervisor backends, network model, milestones).

## Layout

| Crate | What it is |
|---|---|
| [`agentos-core`](crates/agentos-core) | Shared types: sandbox specs, policies, events, host⇄guest protocol |
| [`agentos-vmm`](crates/agentos-vmm) | Hypervisor abstraction + per-OS backends (macOS `vz`, Linux Cloud Hypervisor, Windows stub) |
| [`agentos-daemon`](crates/agentos-daemon) | `agentosd`: control plane, egress proxy, monitor, kill switch |
| [`agentos-cli`](crates/agentos-cli) | `agentos` CLI (`run`, `ps`, `kill`) |
| [`agentos-guest-agent`](crates/agentos-guest-agent) | PID 1 inside the guest; static musl binary |

## Build

```sh
cargo build --workspace
cargo test --workspace
cargo run -p agentos-cli -- run --mount ./project:rw --net allowlist:api.openai.com -- python3 agent.py
```

Status: architecture + scaffold. VMM integration begins with milestone M1 (see ARCHITECTURE.md §11).
