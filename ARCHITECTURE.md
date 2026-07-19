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
| macOS | **Virtualization.framework** via `objc2-virtualization` | Apple-blessed API with first-class virtio-fs, vsock, and Rosetta; no kext, works on Apple Silicon + Intel; App Store–compatible entitlement (`com.apple.security.virtualization`). |
| Linux | **Cloud Hypervisor** (child process, driven over its REST API socket) | Firecracker was rejected because it has no virtio-fs — our mount model requires it. Cloud Hypervisor is KVM-based, boots in hundreds of ms, and supports virtio-fs via an external `virtiofsd`. |
| Windows | **Stub** (`Unsupported` error) | Future path: WHP-based backend, or reusing the Linux backend inside a WSL2 utility VM. Deferred until macOS + Linux are proven. |

The trait is deliberately narrow: the daemon never learns backend specifics, and adding Windows later touches only `agentos-vmm`.

## 5. Guest Image & Boot Flow

- **One shared, versioned rootfs image** (Alpine-based, < 100 MB): busybox userland, Python 3, Node.js, git, and `agentos-guest-agent` as `/init`. Shipped read-only in `~/.agentos/images/`.
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
- **Monitor loop** in the daemon samples, per sandbox: VMM process CPU/RSS (host truth), guest-reported memory/disk (advisory), and proxy byte counters (egress truth).
- **Auto-kill rules** evaluated on each sample: `mem > limit`, `egress_bytes > limit`, optional wall-clock timeout. Trigger ⇒ same path as the manual kill switch, with the triggering rule recorded in the sandbox's event log.

## 11. Milestone Roadmap

- **M1 — "Hello, sandbox" (macOS)**: `agentos run -- echo hi` boots a microVM via Virtualization.framework, streams stdio, exits, wipes. Boot-time budget proven here.
- **M2 — Policy**: virtio-fs mounts (RO/RW), egress proxy with all three network modes, kill switch (save|wipe), quotas + auto-kill. This is the PRD's MVP feature bar.
- **M3 — Linux backend**: Cloud Hypervisor implementation of `VmmBackend`; CI proving both backends against one integration-test suite.
- **M4 — GUI**: Tauri desktop app over the same JSON-RPC socket — sandbox list, permission editor, live terminal, network monitor, big red Terminate button.
- **Later**: Windows backend, enterprise fleet policy, snapshotting, prebuilt agent templates (per PRD §7).

## 12. PRD Requirement Traceability

| PRD § | Requirement | Covered in |
|---|---|---|
| 4.1 | Hardware virtualization, per-agent microVM, < 2 s boot | §4, §5 |
| 4.2 | Mounts (RO/RW/none, deny-default), network modes, LAN/localhost blocking, git without SSH keys | §6, §7 |
| 4.3 | Kill switch: instant, absolute, save-or-wipe | §9 |
| 4.4 | Quotas, live dashboard, auto-kill triggers | §10 |
| 4.5 | GUI + CLI | §3, §11 (M4) |
