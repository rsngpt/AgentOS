# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

Agent OS runs untrusted AI agents inside hardware-isolated microVMs (Virtualization.framework on macOS, Cloud Hypervisor/KVM on Linux; Windows is a stub). The full PRD feature set works and is CI-verified on macOS + Linux (`rsngpt/AgentOS` on GitHub; pushes to `main` trigger a macOS build job and a Linux job that boots real microVMs on KVM): CLI + daemon + both backends, deny-by-default mounts, the NIC-less policy proxy, quotas with auto-kill, the kill switch, the Tauri GUI, agent runtimes on a shared rootfs with a per-sandbox writable overlay, host-side git integration, templates, live CPU/RAM/network monitoring, pause/resume + snapshot/restore (which survives a daemon restart), machine-wide fleet policy, and an embeddable SDK. Read **ARCHITECTURE.md** first — it is the authoritative design (threat model, network model, milestone status) and is kept up to date as milestones land. The security invariants there are non-negotiable: guests get **no NIC** (egress only via the host-side policy proxy over vsock), mounts are deny-by-default with read-only enforced **host-side** (fail closed if that's impossible — e.g. virtiofsd < 1.11 lacking `--readonly`), and the kill switch must remain a plain SIGKILL of the VMM child process — never a cooperative shutdown.

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

cargo build -p agentos-gui           # desktop app; plain cargo, no npm/tauri-cli

./scripts/build-vmhelper.sh          # Swift VM helper; needs Xcode; ad-hoc signs
                                     # with the virtualization entitlement
./scripts/build-guest-image.sh       # kernel + initramfs -> ~/.agentos/images
                                     # (downloads Alpine artifacts on first run;
                                     # needs `brew install squashfs`; guest arch
                                     # defaults to host, override GUEST_ARCH=x86_64)
./scripts/build-guest-image.sh --variant devops   # extra rootfs-devops.squashfs
                                     # (base + aws-cli + terraform), ~5 min
```

Rebuild order matters: guest-agent changes require re-running `build-guest-image.sh` (the agent is baked into the initramfs); vmhelper changes require `build-vmhelper.sh`. Daemon changes require killing the running daemon (`pkill -f target/debug/agentosd`) — the CLI auto-respawns it, otherwise you'll test against stale code.

End-to-end smoke test (boots a real microVM, ~1s):

```sh
./target/debug/agentos run -- echo hi
./target/debug/agentos run --mount /tmp/x:rw --net allowlist:example.com -- sh -c '...'
./scripts/e2e-test.sh          # full backend-agnostic policy test suite
```

Linux backend (Cloud Hypervisor) can only be compile-checked from macOS —
check **both** libcs before pushing (glibc/musl disagree on signatures like
`ioctl`; use `as _` casts):

```sh
cargo check --workspace --exclude agentos-gui --target aarch64-unknown-linux-musl
cargo check --workspace --exclude agentos-gui --target x86_64-unknown-linux-gnu
```

Runtime verification happens in CI (`.github/workflows/ci.yml`, KVM-enabled
Ubuntu runner running the full e2e suite) or on a real Linux host with
`cloud-hypervisor` and `virtiofsd` **≥ 1.11** on PATH (older virtiofsd lacks
`--readonly`; Ubuntu 24.04's apt package is too old — CI cargo-installs it).
CI status can be read unauthenticated via the GitHub REST API with curl; job
*logs* require a token (`gh` is installed but unauthenticated on this machine).

Debugging a failed boot/run: `agentos kill --save <id>` or any error keeps `~/.agentos/sandboxes/<id>/` with `console.log` (guest kernel + guest-agent stderr), `helper.log` (vmhelper), `vmconfig.json`. Daemon log: `~/.agentos/agentosd.log`.

## Architecture (the parts that span multiple files)

Process chain for one sandbox:

```
agentos (CLI) / agentos-gui --UDS JSON lines--> agentosd --spawns--> VMM child process
                                                                        └─ microVM: agentos-guest-agent (PID 1) └─ agent command
```

The VMM child is `agentos-vmhelper` (Swift, Virtualization.framework) on
macOS, or `cloud-hypervisor` plus one `virtiofsd` per mount on Linux. Either
way the child *is* the VM — SIGKILLing it is the kill switch.

- **stdin**: the run connection is bidirectional — after the request line the client sends `{"stdin":[bytes]}` (`null` = EOF), which `run.rs` forwards as `HostMessage::Stdin`. The vsock is *split* so stdin flows while output streams; without that, interactive agents (anything that prompts) hang. The CLI pumps its own stdin and aborts that task on exit so a blocked terminal read can't keep the process alive.
- **Control path**: daemon ⇄ guest-agent speak length-prefixed (u32 LE) JSON frames — types in `agentos-core/src/protocol.rs`, sync codec in the guest agent, async codec in `agentos-daemon/src/frames.rs`. The vmhelper is a dumb byte relay: guest vsock port 1024 ⇄ helper stdio ⇄ daemon. `sandbox.run` is a *streaming* RPC — the UDS connection is dedicated to the run and carries `{event: ...}` lines; other methods are unary JSON-RPC (`agentos-daemon/src/rpc.rs`).
- **Egress path**: guest loopback TCP :3128 (forwarder in guest agent) → guest-initiated vsock to port 1025 → a per-sandbox Unix socket → `agentos-daemon/src/proxy.rs` parses HTTP CONNECT/absolute-URI and applies `NetPolicy`. The UDS location is backend-specific (`VmmBackend::proxy_socket_path`): the vz helper bridges to an arbitrary path via `VZVirtioSocketListener`, while Cloud Hypervisor's hybrid vsock *requires* `<vsock_socket>_1025`. Host-initiated CH connections need the `CONNECT <port>\n` / `OK` handshake (see `backends/cloud_hypervisor.rs`). All policy, DNS, and byte counting are host-side by design; the guest side is deliberately a dumb pipe. Byte counting must stay *incremental* (per chunk) — counting at connection close breaks egress quotas.
- **Mount tags**: the daemon and the backends independently derive virtio-fs tags from `SandboxSpec.mounts` order via `agentos_vmm::share_tag(i)` — keep them in sync through that function only. On Linux, RO enforcement is virtiofsd's `--readonly` (spawned with `--sandbox none` because Ubuntu 24.04's AppArmor blocks unprivileged user namespaces); on macOS it's `VZSharedDirectory(readOnly:)`.
- **Guest session model** (`agentos-guest-agent`, easy to break): the running command is deliberately **decoupled from any control connection**, because a snapshot/restore destroys it. Output goes through an `Outbox` whose sink is swapped when the daemon (re)connects; the child, its pumps, the metrics ticker and the reaper all outlive connections; `serve()` loops on `accept`, rebuilding the vsock listener if it fails. Two consequences to preserve: **the router thread — not `main` — powers the VM off**, once an `Exited` frame is actually delivered (getting this wrong hangs every run, because the daemon waits on a VM that never exits), and an exit that happens while detached is stored in `session.exited` and replayed on the next handshake. `Hello { running }` tells a reattaching daemon to stream instead of issuing a second `Exec`.
- **Kill / pause / snapshot semantics**: `registry.kill()` SIGKILLs the helper; the run task in `agentos-daemon/src/run.rs` notices vsock EOF, reaps, and applies the save|wipe disposition. The registry refuses state transitions out of terminal states (kill-vs-exit races resolve to whoever landed first) and only allows `Running ⇄ Paused`. Because the helper's stdio is already the vsock relay, **signals are its control channel**: SIGUSR1 pause, SIGUSR2 resume, SIGHUP snapshot-and-exit. On Linux the equivalents are `ch-remote pause|resume|snapshot` against the per-VM `--api-socket`. A snapshot keeps the sandbox dir (state + overlay + workspace) instead of wiping it.
- **Guest image**: `scripts/build-guest-image.sh` produces (a) the initramfs — `agentos-guest-agent` as `/init` plus staged `.ko`s (Alpine's virt kernel builds vsock/virtiofs/virtio_blk/squashfs/ext4/overlay `=m`; the agent loads them from `/lib/modules/agentos/order`, deps-first) — and (b) `rootfs.squashfs`, a read-only Alpine root with python3/pip, nodejs/npm, git, e2fsprogs. The rootfs is built with **no `apk`**: `scripts/apk-fetch.py` resolves the `APKINDEX` closure and extracts `.apk`s, so it works on macOS too. It unwraps Alpine's EFI-zboot kernel to the raw ARM64 Image (VZ can't boot zboot PEs). Adding a kernel module: add it to `stage_module` (and its deps, ordered before it) *and* to the guest agent's module `order`.
- **Agent root & overlay**: per run the daemon attaches the shared rootfs as `/dev/vda` (ro) and a fresh sparse overlay disk (sized to `disk_mib`) as `/dev/vdb`. The guest agent (`setup_overlay_root`) formats the overlay ext4 via the rootfs's `mkfs.ext4` (run `chroot`ed into the rootfs), unions overlay-over-rootfs with overlayfs, and `chroot`s the agent command in — so `python3`/`node`/`git` resolve and writes copy-up into the overlay. Virtio-fs shares mount under that root; the loopback egress proxy still works because chroot doesn't change the network namespace. Disk order (vda rootfs, vdb overlay) must stay in sync across `run.rs`, both backends, and the guest agent. No rootfs image ⇒ guest runs in the initramfs (busybox only) — fine for `echo hi`, no runtimes.
- **Adding a hypervisor backend** (e.g. Windows): implement `VmmBackend`/`VmHandle` in `agentos-vmm/src/backends/`, gate with `#[cfg(target_os)]`, wire into `default_backend()`. Nothing above `agentos-vmm` may reference backend specifics.
- **Clients / the SDK**: `crates/agentos-client` is the *public* API (typed `Client`, `RunEvent`, `StdinSender`, `Error` separating daemon-refused from unreachable). The CLI and GUI are both built on it deliberately — if driving Agent OS from our own clients needs something the SDK can't express, an embedder would hit it too, so add it to the SDK rather than reaching past it. `find_daemon` must keep looking beyond "next to the current exe" (`AGENTOSD_PATH` → exe dir → parent → `PATH`): an embedder's binary is never a sibling of `agentosd`. The GUI (`gui/`) is Tauri 2 with a plain static frontend in `gui/dist/` — no npm/node build step; `cargo build -p agentos-gui` is the whole build (tauri requires `gui/icons/icon.png` to exist). The daemon's event bus (`events.subscribe`, broadcast in `registry.rs`) feeds the GUI's network monitor; `agentos events` taps it from the CLI.
- **Fleet policy** (`agentos-core/src/policy.rs`, ARCHITECTURE.md §13): applied in `run_sandbox` **before the sandbox is registered** — never in a client, since a user can run their own. Mounts are canonicalised first, and `deny_mounts` entries are canonicalised too (miss that and `/tmp/x` on macOS silently denies nothing, because it resolves to `/private/tmp/x`). Allowlist subset checks reuse the proxy's matcher via `agentos_core::spec::host_matches` — one implementation only. The system path outranks `AGENTOS_POLICY`, so the env override can't weaken a deployed policy.
- **Snapshot durability**: the registry is in memory, so `spec.json` is written beside `vmstate` and `rehydrate_snapshots` re-registers those sandboxes at daemon startup. Without it a restore after a daemon restart reports "unknown sandbox" while the state file sits unusable on disk.
- **Git integration** (`--repo`): the daemon clones host-side in `clone_repo` (run.rs) using the host's git/creds, mounts the tree at `/workspace`, and passes `cwd` through the `Exec` protocol. Keys never enter the guest — never mount `~/.ssh`.
- **Templates** (`agentos_core::spec::TEMPLATES`) carry two things: a net allowlist (`template_net`) and an optional `rootfs_variant` (`template_rootfs_variant`). `python|node|github` are net-only and boot the base rootfs; `devops` boots `rootfs-devops.squashfs` (aws-cli + terraform), which `run.rs` selects by name and which is **opt-in** — the daemon must keep failing with the build command in the message rather than silently falling back to the base image, or an agent would run without the tools it asked for. Adding a variant means touching three places: `TEMPLATES`, the `--variant` case in `build-guest-image.sh`, and nothing in the backends (the image is just a different vda). Terraform is fetched from HashiCorp, not Alpine — Alpine dropped it after the BUSL change.
- **Monitoring & the panic switch**: **Live CPU%** is computed *in the guest* from `/proc/stat` deltas and sent in `Metrics` — host-side `ps` of the VMM process can't see hypervisor vCPU time or guest RAM on macOS, so don't reach for it. **Global kill hotkey** (⇧⌘K) is a Tauri `global-shortcut`; registration is best-effort (macOS accessibility) and its success/failure is emitted as `hotkey-status` so the window can say the hotkey is unavailable — a kill switch the user believes in but that never fires is worse than one they know is broken. All three panic paths (hotkey, GUI button, `agentos kill --newest`) call one SDK method, `Client::kill_newest_live`, precisely so the untestable one can be verified through the testable one; keep them converged rather than reimplementing "find the newest live sandbox" per surface.
- **Windows**: `backends/windows.rs` is a stub. The intended real impl runs the *unchanged* Cloud Hypervisor backend inside WSL2 (nested virt + /dev/kvm), not a new WHP VMM — see ARCHITECTURE.md §4. Can't be CI-verified here.

## Security invariants found the hard way (an audit closed these — don't regress them)

- `proxy.rs::is_local_ip` is deliberately **broad**: loopback, RFC 1918, link-local (incl. `169.254.169.254` metadata), **carrier-grade NAT `100.64.0.0/10`**, `0.0.0.0/8`, broadcast, multicast, ULA. CGNAT is internal space on many corporate/cloud networks; leaving it out was a real lateral-movement hole. The IPv6 arm re-enters the v4 checks so `::ffff:100.64.0.1` and `::a.b.c.d` wrappers are caught. Never narrow this to "just RFC 1918".
- Egress connections are capped per sandbox (`MAX_CONNECTIONS`), refusing rather than queueing — an untrusted guest can otherwise make the host open unbounded outbound TCP.
- `run.rs::reject_transport_helper` refuses `ext::`/`fd::` git URLs, which execute commands **on the host**; `--` does not stop them.
- Guest-reported `Metrics` are attacker-controlled, so `--kill-over-mem` is evadable by design. The real controls are the hypervisor's RAM/disk caps and the host-truth egress/runtime rules — don't move enforcement onto guest numbers.

## Conventions

- Guest-agent code is `std` + `libc` only (static musl build); no tokio in the guest.
- Swift changes live in the single `vmhelper/main.swift`; beware Swift's `&buf[off]`-style inout-to-pointer of an array element — it passes a pointer to a temporary copy, not into the buffer (this caused a real relay-corruption bug; use `withUnsafeBytes`). Any signal handled via `DispatchSource` also needs `signal(SIG, SIG_IGN)` first, or the default disposition kills the helper before the handler runs.
- When a VMM returns an opaque error, read *its* own output before theorising — `helper.log` is the VMM's stderr, and the SDK headers state constraints the web docs don't. Both multi-cycle spirals here would have been one step otherwise: Cloud Hypervisor silently disabling sector-0 writes without `image_type=raw` (its log said so), and `VZErrorRestore` caused by an unpinned `machineIdentifier` (the header says outright that it must match the saved VM's).
- Logs a sandbox may outlive must be opened **append-only** (`helper.log`, `fs*.log`): a sandbox now spans several VMM processes across snapshot/restore, and truncating destroys the diagnostics of the run that failed.
- Commit messages describe what was *verified*, not just what was written.

## Testing blind spot worth naming

Two use-case-breaking bugs shipped because every test exercised the path just built, in the shape it was built: stdin was never forwarded (no test was interactive, so nothing prompted), and snapshots died with the daemon (no test outlived one). When adding a feature, write at least one assertion for what a *user* would do with it, not what the code does — e.g. `sort` for stdin, because it only terminates if EOF actually propagates, where `cat` would pass either way.

The corollary for things that *can't* be driven headlessly (a system-wide hotkey, a GUI click): don't settle for "it compiles". Give the untestable surface a testable twin that shares the code under test — `agentos kill --newest` exists so the ⇧⌘K panic path has an e2e assertion. Clippy also has to be run per target: `--target *-linux-*` lints code `#[cfg]`-ed out on macOS, and warnings accumulate there unseen.
