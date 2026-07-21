# Agent OS

[![CI](https://github.com/rsngpt/AgentOS/actions/workflows/ci.yml/badge.svg)](https://github.com/rsngpt/AgentOS/actions/workflows/ci.yml)

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
./target/debug/agentos start -- echo hi        # `start` is an alias
./target/debug/agentos run \
    --mount ./project:rw --mount /data/corpus \
    --net allowlist:api.openai.com \
    --kill-over-egress 512 --kill-after-secs 3600 \
    -- python3 agent.py
./target/debug/agentos ps
./target/debug/agentos pause <id>              # freeze an agent mid-task
./target/debug/agentos resume <id>             # continue where it left off
./target/debug/agentos kill <id>               # the kill switch (--save keeps logs)
./target/debug/agentos events                  # stream the daemon event bus
./target/debug/agentos-gui                     # desktop app (Tauri)

./scripts/e2e-test.sh                          # full policy test suite in real microVMs
```

The guest ships Python 3, Node.js, and git on a shared read-only rootfs, with a per-sandbox writable overlay (`pip install` and build artifacts land in the overlay, never the host or the shared image):

```sh
# templates preset the ecosystem's network allowlist so tooling just works
./target/debug/agentos run --template python -- pip install requests

# clone a repo host-side (with your creds, no SSH keys in the guest) into /workspace
./target/debug/agentos run --repo https://github.com/octocat/Hello-World.git \
    --template github -- sh -c 'git log --oneline -1'

# a template can also boot a heavier toolchain image, built opt-in:
./scripts/build-guest-image.sh --variant devops
./target/debug/agentos run --template devops -- terraform version
```

Live CPU/memory/egress per sandbox stream over `agentos events` and the GUI monitor. The panic kill switch has three faces backed by one call: the GUI's global **⇧⌘K**, its red button, and `agentos kill --newest`. `agentos pause|resume|snapshot|restore` freeze an agent mid-task or park its whole VM on disk and pick it up later.

## Embedding

[`agentos-client`](crates/agentos-client) is the API for running agents from your own program — the CLI and GUI are both built on it:

```rust
use agentos_client::{Client, RunEvent};
use agentos_core::SandboxSpec;

let spec = SandboxSpec::command(["python3", "agent.py"]); // no files, no network
let mut run = Client::new().run(&spec).await?;
while let Some(event) = run.next().await? {
    if let RunEvent::Stdout(bytes) = event {
        print!("{}", String::from_utf8_lossy(&bytes));
    }
}
```

`cargo run -p agentos-client --example embed -- python3 -c 'print(2**64)'`

## Fleet policy

A machine-wide policy file (`/etc/agentos/policy.json`, or `/Library/Application Support/AgentOS/policy.json`) caps what any sandbox may do — network ceiling, forbidden mount paths, resource limits — enforced by the daemon, not the client. `agentos policy` shows what's in force. See ARCHITECTURE.md §13 for what it does and doesn't guarantee.

Status: **the full PRD feature set works** on macOS + Linux — hardware-isolated microVMs, deny-by-default mounts, NIC-less network policy, quotas + auto-kill, kill switch, GUI, agent runtimes + writable overlay, git integration, templates, and live CPU/RAM/network monitoring. The Linux Cloud Hypervisor backend is exercised by CI on KVM runners; Windows is a documented stub (WSL2 path in ARCHITECTURE.md §4). See ARCHITECTURE.md §11.
