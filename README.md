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

## Build & run (macOS, Apple Silicon)

```sh
# one-time: guest-agent cross target
rustup target add aarch64-unknown-linux-musl

cargo build --workspace
cargo build -p agentos-guest-agent --release --target aarch64-unknown-linux-musl
./scripts/build-vmhelper.sh        # Swift VM helper (needs Xcode; ad-hoc signed)
./scripts/build-guest-image.sh     # Alpine kernel + initramfs -> ~/.agentos/images

./target/debug/agentos run -- echo hi          # boots a real microVM (~1s)
./target/debug/agentos ps
./target/debug/agentos kill <id>               # the kill switch
```

Status: **M1 complete** — real microVMs on macOS with stdio streaming and the kill switch. Next: M2 (mounts, network policy, quotas); see ARCHITECTURE.md §11.
