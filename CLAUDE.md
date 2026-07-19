# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Agent OS runs untrusted AI agents inside hardware-isolated microVMs (Virtualization.framework on macOS today; Cloud Hypervisor on Linux is milestone M3). Read **ARCHITECTURE.md** first — it is the authoritative design (threat model, network model, milestone status) and is kept up to date as milestones land. The security invariants there are non-negotiable: guests get **no NIC** (egress only via the host-side policy proxy over vsock), mounts are deny-by-default with read-only enforced **host-side**, and the kill switch must remain a plain SIGKILL of the VMM child process — never a cooperative shutdown.

## Build & test

`cargo` may not be on PATH in fresh shells (installed via Homebrew rustup): `export PATH="$HOME/.cargo/bin:/opt/homebrew/opt/rustup/bin:$PATH"`

```sh
cargo build --workspace              # host binaries (agentos, agentosd)
cargo test --workspace               # unit tests
cargo test -p agentos-daemon proxy   # single crate/module tests
cargo clippy --workspace --all-targets   # keep at zero warnings

# Guest agent: static Linux binary, cross-compiled from macOS (linker config
# in .cargo/config.toml — rust-lld, no external toolchain needed)
cargo build -p agentos-guest-agent --release --target aarch64-unknown-linux-musl

./scripts/build-vmhelper.sh          # Swift VM helper; needs Xcode; ad-hoc signs
                                     # with the virtualization entitlement
./scripts/build-guest-image.sh       # kernel + initramfs -> ~/.agentos/images
                                     # (downloads Alpine artifacts on first run;
                                     # needs `brew install squashfs`)
```

Rebuild order matters: guest-agent changes require re-running `build-guest-image.sh` (the agent is baked into the initramfs); vmhelper changes require `build-vmhelper.sh`. Daemon changes require killing the running daemon (`pkill -f target/debug/agentosd`) — the CLI auto-respawns it, otherwise you'll test against stale code.

End-to-end smoke test (boots a real microVM, ~1s):

```sh
./target/debug/agentos run -- echo hi
./target/debug/agentos run --mount /tmp/x:rw --net allowlist:example.com -- sh -c '...'
```

Debugging a failed boot/run: `agentos kill --save <id>` or any error keeps `~/.agentos/sandboxes/<id>/` with `console.log` (guest kernel + guest-agent stderr), `helper.log` (vmhelper), `vmconfig.json`. Daemon log: `~/.agentos/agentosd.log`.

## Architecture (the parts that span multiple files)

Process chain for one sandbox:

```
agentos (CLI) --UDS JSON lines--> agentosd --spawns--> agentos-vmhelper (Swift)
                                                          └─ microVM: agentos-guest-agent (PID 1) └─ agent command
```

- **Control path**: daemon ⇄ guest-agent speak length-prefixed (u32 LE) JSON frames — types in `agentos-core/src/protocol.rs`, sync codec in the guest agent, async codec in `agentos-daemon/src/frames.rs`. The vmhelper is a dumb byte relay: guest vsock port 1024 ⇄ helper stdio ⇄ daemon. `sandbox.run` is a *streaming* RPC — the UDS connection is dedicated to the run and carries `{event: ...}` lines; other methods are unary JSON-RPC (`agentos-daemon/src/rpc.rs`).
- **Egress path**: guest loopback TCP :3128 (forwarder in guest agent) → guest-initiated vsock to port 1025 → vmhelper's `VZVirtioSocketListener` bridges to `~/.agentos/sandboxes/<id>/proxy.sock` → `agentos-daemon/src/proxy.rs` parses HTTP CONNECT/absolute-URI and applies `NetPolicy`. All policy, DNS, and byte counting are host-side by design; the guest side is deliberately a dumb pipe. Byte counting must stay *incremental* (per chunk) — counting at connection close breaks egress quotas.
- **Mount tags**: the daemon and the vz backend independently derive virtio-fs tags from `SandboxSpec.mounts` order via `agentos_vmm::share_tag(i)` — keep them in sync through that function only.
- **Kill semantics**: `registry.kill()` SIGKILLs the helper; the run task in `agentos-daemon/src/run.rs` notices vsock EOF, reaps, and applies the save|wipe disposition. The registry refuses state transitions out of terminal states (kill-vs-exit races resolve to whoever landed first).
- **Guest image**: `scripts/build-guest-image.sh` unwraps Alpine's EFI-zboot kernel to the raw ARM64 Image (Virtualization.framework can't boot zboot PEs) and stages vsock+fuse+virtiofs `.ko`s into the initramfs (Alpine's virt kernel builds them `=m`); the guest agent loads them from `/lib/modules/agentos/order`. If you need another kernel module, add it to both the script's `stage_module` list and check its deps.
- **Adding a hypervisor backend** (e.g. M3 Linux): implement `VmmBackend`/`VmHandle` in `agentos-vmm/src/backends/`, gate with `#[cfg(target_os)]`, wire into `default_backend()`. Nothing above `agentos-vmm` may reference backend specifics.

## Conventions

- Guest-agent code is `std` + `libc` only (static musl build); no tokio in the guest.
- Swift changes live in the single `vmhelper/main.swift`; beware Swift's `&buf[off]`-style inout-to-pointer of an array element — it passes a pointer to a temporary copy, not into the buffer (this caused a real relay-corruption bug; use `withUnsafeBytes`).
- Commit messages describe what was *verified*, not just what was written.
