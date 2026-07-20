# Agent OS — Technical Architecture

**Status:** Draft v1 · **Scope:** MVP (v1.0) · **Companion doc:** the product PRD (microVM sandboxing, granular permissions, kill switch, quotas, GUI+CLI)

---

## 1. Overview & Goals

Agent OS runs untrusted AI agents inside hardware-isolated microVMs on a developer's local machine. The host user grants each sandbox an explicit set of directories and a network policy; everything else is invisible to the agent. A kill switch terminates a sandbox instantly and totally.

Design goals, in priority order:

1. **Containment is absolute.** The security boundary is the CPU's virtualization hardware, not a kernel namespace. No feature ships if it weakens this.
2. **Deny by default.** A fresh sandbox sees no host files and has no network. Every grant is explicit, visible, and revocable.
3. **Fast enough to not change behavior.** Sandbox boot must feel like starting a process (< 2 s target), or users will fall back to running agents bare.
4. **One core, three OSes.** A single Rust control plane with per-OS hypervisor backends behind one trait. macOS and Linux are implemented first; Windows is a documented stub.

Non-goals for v1: multi-host fleet management, agent snapshotting/migration, GPU passthrough, running non-Linux guests.

## 2. Threat Model

**In scope (what we defend against):**

- A hallucinating agent executing destructive commands (`rm -rf /`, `git push --force`, etc.) — blast radius is limited to the sandbox overlay and explicitly RW-mounted directories.
- A malicious agent binary (trojaned GitHub project) attempting to read SSH keys, browser profiles, env vars, or any unmounted path — impossible: those paths are never exposed to the guest.
- Data exfiltration over the network — all egress traverses a host-side policy proxy; offline/allowlist modes make exfiltration destinations unreachable.
- Lateral movement into localhost services or the LAN/corporate network — the guest has **no NIC**; the proxy always refuses loopback, RFC 1918, link-local, and ULA destinations unless the user explicitly grants them.
- Resource abuse (memory ballooning, disk filling, crypto-mining) — hard caps at VM creation plus auto-kill rules.

**Out of scope (accepted risks, documented honestly):**

- Hypervisor escape via a vulnerability in KVM / Virtualization.framework / Cloud Hypervisor. We inherit the platform vendor's security posture; mitigation is minimizing exposed virtual devices (vsock + virtio-fs + virtio-blk only).
- Malicious content *inside* an RW-mounted directory (the agent may corrupt what you gave it write access to — that is the grant working as designed).
- Prompt-injection making an agent misuse a *granted* permission (e.g., exfiltrating a mounted repo to an *allowlisted* domain). Agent OS constrains the channel, not the agent's judgment.
- Side channels (Spectre-class, cache timing).
- **A lying guest evading the `--kill-over-mem` auto-kill.** Memory and CPU in `Metrics` are *reported by the guest agent*, which a compromised agent controls, so it can under-report to avoid tripping that rule. This is acceptable because the real control is elsewhere and is not evadable: `limits.mem_mib` is a hard RAM ceiling enforced by the hypervisor at VM creation, and `limits.disk_mib` sizes the overlay — a guest simply cannot exceed either. The egress and runtime auto-kill rules are host-truth (proxy byte counters, wall clock) and likewise unevadable. Treat `--kill-over-mem` as "notice a runaway", not as a containment boundary.

## 3. Process & Component Model

```
┌─ Host ────────────────────────────────────────────────────────────┐
│                                                                   │
│  agentos (CLI)      AgentOS.app (GUI, Tauri — M4)                 │
│        │                  │                                       │
│        └──── JSON-RPC over Unix domain socket ────┐               │
│                                                   ▼               │
│  ┌─ agentosd (daemon) ─────────────────────────────────────────┐  │
│  │  sandbox registry · policy engine · egress proxy · monitor  │  │
│  └──────┬──────────────────────────────┬───────────────────────┘  │
│         │ spawns (child process)       │ vsock                    │
│  ┌──────▼──────────┐            ┌──────▼──────────┐               │
│  │ VMM process #1  │            │ VMM process #2  │  …            │
│  │ ┌─ microVM ───┐ │            │ ┌─ microVM ───┐ │               │
│  │ │ guest-agent │ │            │ │ guest-agent │ │               │
│  │ │  └ agent cmd│ │            │ │  └ agent cmd│ │               │
│  │ └─────────────┘ │            │ └─────────────┘ │               │
│  └─────────────────┘            └─────────────────┘               │
└───────────────────────────────────────────────────────────────────┘
```

- **`agentosd`** — long-lived daemon; owns all sandbox state, enforces policy, runs the egress proxy and the resource monitor. Listens on `~/.agentos/agentosd.sock` (mode 0600).
- **VMM processes** — one OS process per sandbox, always a *child of the daemon*. This makes the kill switch trivial and airtight: `SIGKILL` the child ⇒ the hardware VM and everything in it ceases to exist. No cleanup protocol an attacker could stall.
- **`agentos-guest-agent`** — a static musl binary that is PID 1 inside the guest. It mounts virtio-fs shares, applies guest-side env, execs the agent command, streams stdio, and reports metrics — all over vsock.
- **CLI / GUI** — thin clients over the daemon's JSON-RPC socket. The CLI attaches to a sandbox's stdio stream so `agentos run … -- <cmd>` feels like running `<cmd>` locally.

Crate map: `agentos-core` (types/policy/protocol, no I/O) · `agentos-vmm` (backend trait + per-OS impls) · `agentos-daemon` · `agentos-cli` · `agentos-guest-agent`.

## 4. Hypervisor Abstraction

One trait, three backends:

```rust
trait VmmBackend {
    async fn create(&self, spec: &SandboxSpec, paths: &SandboxPaths) -> Result<Box<dyn VmHandle>>;
}
trait VmHandle {
    fn state(&self) -> VmState;
    async fn connect_vsock(&self, port: u32) -> Result<VsockStream>;
    async fn kill(&mut self) -> Result<()>;      // SIGKILL-grade, never graceful
    async fn wait(&mut self) -> Result<ExitInfo>;
}
```

| OS | Backend | Rationale |
|---|---|---|
| macOS | **Virtualization.framework**, hosted in a small Swift helper (`vmhelper/`) spawned per sandbox | Apple-blessed API with first-class virtio-fs and vsock; no kext. The helper *is* the VM process — it boots the guest and relays the vsock control stream over its stdio — which makes kill-by-SIGKILL trivial and keeps the Objective-C surface out of the Rust core. Signed with the `com.apple.security.virtualization` entitlement. |
| Linux | **Cloud Hypervisor** (child process; CLI-configured, hybrid-vsock Unix sockets) | Firecracker was rejected because it has no virtio-fs — our mount model requires it. Cloud Hypervisor is KVM-based, boots in hundreds of ms, and supports virtio-fs via one unprivileged `virtiofsd` per share (`--readonly` enforced host-side; requires `--memory shared=on`). Host⇄guest control uses the Firecracker-style `CONNECT <port>` handshake on the vsock UDS; guest-initiated connections to port P surface at `<socket>_P`. |
| Windows | **Stub** (`Unsupported` error) | Recommended path: run the existing Cloud Hypervisor backend inside WSL2 (details below). Deferred until there's demand; cannot be CI-verified from macOS+Linux. |

**Windows plan (§4.1).** The intended Windows implementation is *not* a new hypervisor integration — it reuses the Linux `CloudHypervisorBackend` verbatim inside **WSL2**. WSL2 is a Hyper-V-backed Linux VM that exposes nested virtualization and `/dev/kvm`, so Cloud Hypervisor, one `virtiofsd` per mount, virtio-vsock, and the guest image all work unchanged. `agentosd` runs natively on Windows; only the VMM child process is launched into the WSL distro (`wsl.exe -d <distro> -- cloud-hypervisor …`), with the per-sandbox Unix sockets on the WSL filesystem and paths translated at the boundary. This trades a WSL2 dependency for reusing a backend that CI already boots real microVMs against, versus the larger alternative of a from-scratch Windows Hypervisor Platform (WHP) `VmHandle`. Everything above `agentos-vmm` is untouched either way.

The trait is deliberately narrow: the daemon never learns backend specifics, and adding Windows later touches only `agentos-vmm`.

## 5. Guest Image & Boot Flow

- **Guest image** in `~/.agentos/images/`, built by `scripts/build-guest-image.sh`, in two parts:
  - **initramfs** — `agentos-guest-agent` as `/init` plus the kernel modules it loads (vsock, virtiofs, virtio-blk, squashfs, ext4, overlay; Alpine's virt kernel builds them `=m`). The Alpine kernel arrives as an EFI-zboot PE, which the build script unwraps to the raw ARM64 Image that `VZLinuxBootLoader` requires.
  - **rootfs.squashfs** — a read-only Alpine root with the agent runtimes (Python 3 + pip, Node.js + npm, git, e2fsprogs), shared across all sandboxes. It's assembled host-side with no `apk`: `scripts/apk-fetch.py` resolves the Alpine `APKINDEX` dependency closure and extracts the `.apk`s, so the image builds identically on macOS and Linux.
- **Per-sandbox writable overlay**: the daemon creates a sparse disk sized to the `disk_mib` quota and attaches it as `/dev/vdb` (rootfs is `/dev/vda`). The guest agent formats it ext4 (via the rootfs's `mkfs.ext4`), unions it over the read-only rootfs with **overlayfs**, and `chroot`s the agent command into the union — so writes (`pip install`, build artifacts) copy-up into the overlay and never touch the shared image. "Wipe" deletes the overlay; "save" keeps it under the sandbox dir. Without a rootfs image the guest falls back to running the command directly in the initramfs (busybox only).
- **Per-sandbox writable overlay**: a sparse raw disk mounted as the upper layer of an overlayfs. "Wipe" deletes it; "save for debugging" keeps it and it can be re-inspected or re-attached.
- **Boot path to < 2 s**: direct kernel boot (no bootloader, no initramfs beyond the embedded init), minimal virtual device set, `virtio-blk` root, guest-agent as PID 1 (no init system). Cloud Hypervisor and Virtualization.framework both do direct kernel boot in the 200–800 ms range; the budget is boot ≤ 1 s + guest-agent handshake ≤ 200 ms.
- Kernel: a pinned minimal `Image`/`vmlinux` built with virtio + overlayfs + vsock only, shipped alongside the rootfs.

## 6. Filesystem Model

- Default: the guest sees **only** its rootfs + overlay. Zero host paths.
- Each `--mount host_path:mode` becomes one **virtio-fs share**, tagged and mounted by the guest-agent at `/mnt/<name>` (or a user-chosen guest path).
- **Read-only is enforced host-side** (the virtio-fs device/daemon is opened RO), not by guest mount flags — the guest is untrusted and cannot upgrade its own access.
- virtio-fs daemons run unprivileged as the user, so a share can never expose more than the user could read anyway.
- **Git integration without key exposure**: `agentos run --repo <url>` clones on the *host* into a scratch dir using the user's existing credentials, then mounts the clone RW. `~/.ssh`, `~/.gitconfig` credentials, and keychains never enter the guest. Pushes happen host-side on user approval (M2+).

## 7. Network Model

The guest has **no network interface**. Its only channel is vsock. Networking is provided — when policy allows — by a host-side egress proxy inside `agentosd`:

- The guest-agent configures the guest's proxy env (`HTTP_PROXY`/`HTTPS_PROXY`, plus a SOCKS5 endpoint) pointing at a vsock-backed local forwarder.
- The daemon terminates the vsock stream and applies **policy per connection**:
  - **Offline** — no proxy is offered at all; the guest physically cannot make a connection.
  - **Allowlist** — HTTP `CONNECT` / SOCKS destination host is matched against user patterns (`api.openai.com`, `*.github.com`). No TLS interception — we match names, never decrypt.
  - **Full** — everything allowed **except** loopback, RFC 1918, link-local, and ULA ranges (LAN lateral movement stays blocked even in "full").
- DNS resolves host-side through the same policy engine, closing DNS-tunnel exfiltration in offline/allowlist modes.
- Every allowed/denied connection is an `Event` (source sandbox, destination, verdict, bytes) — this feeds the GUI's live network monitor and the egress-volume auto-kill rule.

Trade-off, stated plainly: proxy-based egress means agents doing raw non-proxied sockets need proxy-aware tooling (nearly all HTTP-era AI tooling honors proxy env vars). In exchange, policy lives entirely in host code the guest can't touch, is identical across OSes, and requires no firewall/root privileges.

## 8. Control Protocol

- **CLI/GUI ⇄ daemon**: JSON-RPC 2.0 over the Unix socket. Methods: `sandbox.create`, `sandbox.list`, `sandbox.kill {save|wipe}`, `sandbox.attach` (upgrades the connection to a raw stdio stream), `sandbox.events` (subscription).
- **daemon ⇄ guest-agent**: length-prefixed JSON messages over vsock port 1024 (`Hello`, `Exec`, `Stdin`, `Stdout`, `Stderr`, `Exited`, `Metrics`). Message types live in `agentos-core::protocol` and are shared by both sides — one source of truth.

## 9. Sandbox Lifecycle & the Kill Switch

```
Provisioning → Booting → Running → Exited
                   │         │
                   └────► Killed ──► (overlay saved | wiped)
```

- Kill switch = daemon sends `SIGKILL` to the VMM child process. Because containment is hardware-level, no guest state survives; there is nothing the guest can do to delay or intercept it. Target latency: < 100 ms from click/hotkey to dead VM.
- After kill or exit, per-policy the overlay disk is deleted (default) or retained under `~/.agentos/sandboxes/<id>/` for forensics.
- Daemon crash safety: VMM children are spawned in a process group / with a parent-death signal so an `agentosd` crash also reaps every sandbox — fail-closed.

## 10. Resource Quotas & Monitoring

- **Hard caps at creation** (can't be raised while running): vCPU count, RAM MiB, overlay disk MiB.
- **Monitor loop** in the daemon samples, per sandbox: guest-reported CPU% and memory (from `/proc/stat` and `/proc/meminfo` in the guest — the VMM process's host-side figures don't capture hypervisor vCPU time or guest RAM on macOS) and proxy byte counters (host truth). Guest-reported numbers are advisory: see the threat-model note in §2 on what that means for `--kill-over-mem`.
- **Auto-kill rules** evaluated on each sample: `mem > limit`, `egress_bytes > limit`, optional wall-clock timeout. Trigger ⇒ same path as the manual kill switch, with the triggering rule recorded in the sandbox's event log.

## 11. Milestone Roadmap

- **M1 — "Hello, sandbox" (macOS)**: ✅ done. `agentos run -- echo hi` boots a microVM via Virtualization.framework, streams stdio, exits, wipes; `agentos ps`/`kill` work. Measured: ~0.8 s from spawn to guest exit (boot ≈ 60 ms).
- **M2 — Policy**: ✅ done. virtio-fs mounts (RO/RW, host-enforced — a root guest remounting rw still cannot write), egress proxy with all three network modes (localhost/LAN blocked even in `full`, DNS resolved host-side with local-address filtering), kill switch save|wipe dispositions, and auto-kill on memory/egress/runtime with incremental egress counting. This is the PRD's MVP feature bar.
- **M3 — Linux backend**: ✅ done, CI-verified. `CloudHypervisorBackend` spawns `cloud-hypervisor` (hybrid vsock, no NIC, one unprivileged `virtiofsd --sandbox none` per mount with host-side `--readonly` — virtiofsd ≥ 1.11 required; the backend fails closed, quoting virtiofsd's stderr, rather than ever running an RO mount unenforced); guest-initiated egress lands on the `<vsock_socket>_1025` Unix socket where the daemon binds its proxy (`VmmBackend::proxy_socket_path`). The shared e2e suite (`scripts/e2e-test.sh`, 11 assertions) passes on macOS/vz locally and on real KVM Cloud Hypervisor microVMs in CI (`.github/workflows/ci.yml`).
- **M4 — GUI**: ✅ done. Tauri desktop app (`gui/`, vanilla static frontend — no npm toolchain) over the same socket as the CLI, via the shared `agentos-client` crate: sandbox list with live states, permission form (mounts/net/limits/auto-kill), streaming terminal, network monitor fed by the daemon's event bus (`events.subscribe` broadcasts `StateChanged`/`NetVerdict`/`ResourceSample`/`AutoKillTriggered`), and per-sandbox Terminate / kill+save buttons. `agentos events` streams the same bus in the CLI.
- **Later**: Windows backend, enterprise fleet policy (per PRD §7).

## 12. Snapshotting (PRD §7)

Split into two halves with very different costs:

- **Pause/resume mid-task — done.** `agentos pause|resume <id>` freezes and unfreezes the guest's vCPUs via `VmHandle::pause`/`resume`: SIGUSR1/SIGUSR2 to the vmhelper on macOS (its stdio is already dedicated to relaying vsock, so signals are the free control channel), and `ch-remote pause|resume` against a per-VM `--api-socket` on Linux. No protocol change was needed — a frozen guest simply stops producing vsock frames, so the daemon's run task waits and the agent resumes exactly where it left off. The registry only permits `Running → Paused → Running`, and terminal sandboxes refuse both.
- **Save (`agentos snapshot`) — works.** SIGHUP makes the vmhelper close the control vsock, pause, `saveMachineStateTo` the sandbox dir, and exit; the sandbox becomes `Snapshotted` and its dir (state file + overlay + cloned workspace) is preserved rather than wiped. Verified on vz: ~35 MB state file from a live 2 GiB sandbox. On Cloud Hypervisor the equivalent is `ch-remote pause` + `snapshot file://…` into the same path.
- **The reconnect protocol — done** (it was the prerequisite). The guest agent no longer ties the command's lifetime to a connection: output flows through an `Outbox` whose sink is swapped on (re)connect, the child/pumps/reaper outlive any connection, the vsock listener is rebuilt if `accept` fails, and `Hello { running }` tells a reattaching daemon to stream instead of issuing a second `Exec`. An exit that happens while detached is remembered and replayed. `sandbox.restore` streams to a fresh client exactly like `sandbox.run`.
- **Restore (`agentos restore`) — implemented but BLOCKED on macOS.** `restoreMachineStateFrom` fails with `VZErrorDomain Code=12` ("invalid argument", i.e. `VZErrorRestore`) even though the framework accepts everything we can check. Ruled out by experiment, so don't re-test these:
  - *Configuration mismatch* — the `vmconfig.json` used to save and to restore are byte-identical apart from our own `restore_path` field (diffed).
  - *Unsupported device set* — `validateSaveRestoreSupport` reports SUPPORTED, with and without virtio-fs mounts (the vmhelper logs this verdict at every boot).
  - *Block devices* — the failure is identical with **no disks attached at all** (initramfs-only guest).
  - *A live virtio-socket connection in the saved state* — the helper now closes the control connection and removes the proxy listener before saving; the error is unchanged.

  What's left to try: the serial/console device (`VZFileHandleSerialPortAttachment`) is the main remaining device besides vsock/entropy/bootloader, so testing a save/restore with the console detached would isolate it. Apple's error carries no detail beyond "invalid argument", so this needs bisection rather than reasoning.
- **"Share with a colleague" is not achievable on macOS regardless.** `saveMachineStateTo` produces a file "protected by an encryption key tied to the host on which it is created", restorable only on that same host. Sharing would have to be rebuilt from the overlay disk plus the spec — restarting, not resuming. Cloud Hypervisor's snapshots carry no such host-key restriction, so the two backends would mean different things by "share", which the CLI would have to surface honestly.

  Save/restore is arm64-only and needs macOS 14+.

## 12. PRD Requirement Traceability

| PRD § | Requirement | Covered in |
|---|---|---|
| 4.1 | Hardware virtualization, per-agent microVM, < 2 s boot | §4, §5 |
| 4.2 | Mounts (RO/RW/none, deny-default), network modes, LAN/localhost blocking, git without SSH keys | §6, §7 |
| 4.3 | Kill switch: instant, absolute, save-or-wipe | §9 |
| 4.4 | Quotas, live dashboard, auto-kill triggers | §10 |
| 4.5 | GUI + CLI | §3, §11 (M4) |
