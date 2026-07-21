//! The guest agent: PID 1 inside every Agent OS microVM.
//!
//! Boot-to-exit flow (milestone M1):
//! 1. Mount /proc, /sys, /dev.
//! 2. Listen on vsock [`agentos_core::GUEST_CONTROL_PORT`], accept the
//!    daemon's connection (relayed by vmhelper), complete the `Hello`
//!    handshake.
//! 3. On `Exec`: apply env, spawn the command, pump stdio as
//!    `Stdout`/`Stderr` frames.
//! 4. Send `Exited`, then power the VM off.
//!
//! Built as a static musl binary and installed as `/init` in the initramfs.
//! On non-Linux targets this compiles to a stub so `cargo build --workspace`
//! works on any host.

fn main() {
    #[cfg(target_os = "linux")]
    {
        linux::run();
    }
    #[cfg(not(target_os = "linux"))]
    {
        eprintln!(
            "agentos-guest-agent (protocol v{}) only runs inside a Linux guest; \
             this is a host-build stub",
            agentos_core::protocol::PROTOCOL_VERSION
        );
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::{Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    use agentos_core::protocol::{GuestMessage, HostMessage, PROTOCOL_VERSION};
    use agentos_core::{ExitInfo, GUEST_CONTROL_PORT};

    pub fn run() -> ! {
        // We are PID 1: nothing is mounted yet and there is no one to report
        // errors to except the console.
        log("starting");
        mount_pseudo_filesystems();
        load_kernel_modules();

        match serve() {
            Ok(()) => log("session complete"),
            Err(e) => log(&format!("fatal: {e}")),
        }
        power_off();
    }

    fn log(msg: &str) {
        eprintln!("[guest-agent] {msg}");
    }

    fn mount_pseudo_filesystems() {
        std::fs::create_dir_all("/proc").ok();
        std::fs::create_dir_all("/sys").ok();
        std::fs::create_dir_all("/dev").ok();
        mount("proc", "/proc", "proc");
        mount("sysfs", "/sys", "sysfs");
        mount("devtmpfs", "/dev", "devtmpfs");
    }

    fn mount(src: &str, target: &str, fstype: &str) {
        let src = std::ffi::CString::new(src).unwrap();
        let target_c = std::ffi::CString::new(target).unwrap();
        let fstype = std::ffi::CString::new(fstype).unwrap();
        let rc = unsafe {
            libc::mount(
                src.as_ptr(),
                target_c.as_ptr(),
                fstype.as_ptr(),
                0,
                std::ptr::null(),
            )
        };
        if rc != 0 {
            log(&format!(
                "mount {target} failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    /// Load kernel modules staged by the image build under
    /// /lib/modules/agentos/, in the order listed in its `order` file.
    /// Absent dir means the kernel has everything built in — fine.
    fn load_kernel_modules() {
        let dir = "/lib/modules/agentos";
        let Ok(order) = std::fs::read_to_string(format!("{dir}/order")) else {
            return;
        };
        for name in order.lines().map(str::trim).filter(|l| !l.is_empty()) {
            let path = format!("{dir}/{name}");
            match std::fs::File::open(&path) {
                Ok(f) => {
                    let rc = unsafe {
                        libc::syscall(
                            libc::SYS_finit_module,
                            f.as_raw_fd(),
                            c"".as_ptr(),
                            0,
                        )
                    };
                    if rc != 0 {
                        log(&format!(
                            "finit_module {name}: {}",
                            std::io::Error::last_os_error()
                        ));
                    }
                }
                Err(e) => log(&format!("open module {path}: {e}")),
            }
        }
    }

    /// Where outgoing frames go. The sink is swapped when the daemon
    /// (re)connects, so losing the control connection — which a
    /// snapshot/restore necessarily does — never kills the running agent.
    struct Outbox {
        sink: std::sync::Mutex<Option<std::fs::File>>,
    }

    impl Outbox {
        fn new() -> Self {
            Self {
                sink: std::sync::Mutex::new(None),
            }
        }

        fn attach(&self, f: Option<std::fs::File>) {
            *self.sink.lock().unwrap() = f;
        }

        /// Write one frame. Returns false when no daemon is attached (the
        /// frame is dropped: output produced while detached is lost).
        fn send(&self, msg: &GuestMessage) -> bool {
            let mut guard = self.sink.lock().unwrap();
            match guard.as_mut() {
                Some(w) => {
                    if write_frame(w, msg).is_ok() {
                        true
                    } else {
                        *guard = None;
                        false
                    }
                }
                None => false,
            }
        }
    }

    /// State that outlives any single control connection.
    struct Session {
        outbox: std::sync::Arc<Outbox>,
        tx: mpsc::Sender<GuestMessage>,
        /// Set once the command has finished, so a reattaching daemon can be
        /// told even if the connection died before we could deliver it.
        exited: std::sync::Arc<std::sync::Mutex<Option<ExitInfo>>>,
        child_stdin: std::sync::Arc<std::sync::Mutex<Option<std::process::ChildStdin>>>,
        running: bool,
    }

    /// Serve control connections until the command has finished *and* the
    /// daemon has been told. Survives disconnects in between.
    fn serve() -> std::io::Result<()> {
        use std::sync::atomic::Ordering;

        let outbox = std::sync::Arc::new(Outbox::new());
        let (tx, rx) = mpsc::channel::<GuestMessage>();
        let exit_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));

        // One router thread owns the outgoing queue for the agent's lifetime.
        {
            let outbox = outbox.clone();
            let exit_delivered = exit_delivered.clone();
            std::thread::spawn(move || {
                for msg in rx {
                    let is_exit = matches!(msg, GuestMessage::Exited { .. });
                    if outbox.send(&msg) && is_exit {
                        // The command finished *and* the daemon has been told:
                        // flush, then halt so the daemon sees vsock EOF and
                        // reaps the VM. If we were detached when the child
                        // exited, this instead fires on the replay after
                        // reattach — so a snapshot/restore can't lose the
                        // exit status.
                        exit_delivered.store(true, Ordering::Relaxed);
                        std::thread::sleep(std::time::Duration::from_millis(150));
                        power_off();
                    }
                }
            });
        }

        let mut session = Session {
            outbox: outbox.clone(),
            tx,
            exited: Default::default(),
            child_stdin: Default::default(),
            running: false,
        };

        let mut listener = vsock_listen(GUEST_CONTROL_PORT)?;
        log(&format!("listening on vsock port {GUEST_CONTROL_PORT}"));

        loop {
            let conn = match vsock_accept(&listener) {
                Ok(c) => c,
                Err(e) => {
                    // Restoring a saved VM resets the virtio-vsock device, which
                    // can invalidate the old listener — rebuild it and carry on.
                    log(&format!("vsock accept failed ({e}); re-listening"));
                    std::thread::sleep(std::time::Duration::from_millis(200));
                    listener = vsock_listen(GUEST_CONTROL_PORT)?;
                    continue;
                }
            };
            log("daemon connected");
            outbox.attach(Some(unsafe { std::fs::File::from_raw_fd(dup(&conn)?) }));

            if let Err(e) = serve_connection(&conn, &mut session) {
                log(&format!("control connection ended: {e}"));
            }
            outbox.attach(None);
            // Termination is driven by the router thread (it halts once the
            // exit status is delivered), so a lost connection just means we
            // wait for the daemon to come back — e.g. after a restore.
            log("daemon detached; agent still running, awaiting reattach");
        }
    }

    /// Handshake, start the command on first attach (or replay the exit status
    /// on a reattach), then pump host→child stdin until the connection drops.
    fn serve_connection(conn: &OwnedFd, session: &mut Session) -> std::io::Result<()> {
        let mut reader = unsafe { std::fs::File::from_raw_fd(dup(conn)?) };

        // Handshake: daemon speaks first.
        match read_frame(&mut reader)? {
            HostMessage::Hello { version } if version == PROTOCOL_VERSION => {
                session.outbox.send(&GuestMessage::Hello {
                    version: PROTOCOL_VERSION,
                    running: session.running,
                });
            }
            other => {
                return Err(other_err(format!(
                    "expected Hello v{PROTOCOL_VERSION}, got {other:?}"
                )));
            }
        }

        if session.running {
            // Reattach: the command is already going (or has finished while we
            // were detached, in which case replay its exit status).
            log("daemon reattached to a running command");
            let exited = *session.exited.lock().unwrap();
            if let Some(info) = exited {
                session.tx.send(GuestMessage::Exited { info }).ok();
            }
        } else {
            let (command, env, mounts, net, cwd) = loop {
                match read_frame(&mut reader)? {
                    HostMessage::Exec { command, env, mounts, net, cwd } => {
                        break (command, env, mounts, net, cwd)
                    }
                    HostMessage::Hello { .. } | HostMessage::Stdin { .. } => continue,
                    HostMessage::Terminate => return Ok(()),
                }
            };
            if command.is_empty() {
                return Err(other_err("empty command".into()));
            }
            start_command(session, command, env, mounts, net, cwd)?;
            session.running = true;
        }

        // Host → child stdin for the life of this connection.
        loop {
            match read_frame(&mut reader) {
                Ok(HostMessage::Stdin { data }) => {
                    let mut guard = session.child_stdin.lock().unwrap();
                    if let Some(stdin) = guard.as_mut() {
                        if data.is_empty() || stdin.write_all(&data).is_err() {
                            *guard = None; // EOF or broken pipe
                        }
                    }
                }
                Ok(HostMessage::Terminate) => return Ok(()),
                Err(e) => return Err(e),
                Ok(_) => {}
            }
        }
    }

    /// Prepare the guest root, start the agent command, and wire its output to
    /// the outbox. Everything spawned here outlives the control connection.
    fn start_command(
        session: &Session,
        command: Vec<String>,
        env: Vec<(String, String)>,
        mounts: Vec<(String, String, bool)>,
        net: agentos_core::NetPolicy,
        cwd: Option<String>,
    ) -> std::io::Result<()> {
        let tx = session.tx.clone();

        // Set up the agent root: with a rootfs disk present, union a writable
        // overlay over the read-only runtime rootfs and chroot into it (so the
        // command sees python3/node/git and can write anywhere); otherwise run
        // in the initramfs. Shares mount under the chosen root.
        let root = setup_overlay_root();
        mount_shares(root.unwrap_or(""), &mounts);

        // Networking: the guest has no NIC. When policy allows egress, run a
        // local forwarder (loopback TCP -> vsock to the host's policy proxy)
        // and point proxy env vars at it. Offline: neither exists.
        let mut proxy_env: Vec<(String, String)> = Vec::new();
        if !matches!(net, agentos_core::NetPolicy::Offline) {
            bring_up_loopback();
            std::thread::spawn(proxy_forwarder);
            let url = format!("http://127.0.0.1:{PROXY_LISTEN_PORT}");
            for k in ["http_proxy", "HTTP_PROXY", "https_proxy", "HTTPS_PROXY"] {
                proxy_env.push((k.to_string(), url.clone()));
            }
        }

        log(&format!("exec: {command:?} (root={})", root.unwrap_or("initramfs")));
        let mut cmd = Command::new(&command[0]);
        cmd.args(&command[1..])
            .envs(proxy_env)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(newroot) = root {
            // chroot into the union after fork, before exec. The stdio pipes
            // and the loopback proxy (init's network namespace) survive it;
            // PATH is then searched inside the new root, so `python3` resolves.
            let newroot = newroot.to_string();
            // chdir into cwd (e.g. /workspace) relative to the new root, else /.
            let chdir_to = cwd.clone().unwrap_or_else(|| "/".into());
            unsafe {
                cmd.pre_exec(move || {
                    let root_c = std::ffi::CString::new(newroot.as_str()).unwrap();
                    let dir_c = std::ffi::CString::new(chdir_to.as_str()).unwrap();
                    if libc::chroot(root_c.as_ptr()) != 0 || libc::chdir(dir_c.as_ptr()) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        } else if let Some(dir) = &cwd {
            // No chroot (initramfs fallback): best-effort working directory.
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| other_err(format!("spawn {:?}: {e}", command[0])))?;

        // Stdin belongs to the session, not the connection, so a reattaching
        // daemon keeps writing to the same child.
        *session.child_stdin.lock().unwrap() = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let tx_out = tx.clone();
        let t_out = std::thread::spawn(move || pump(stdout, tx_out, true));
        let tx_err = tx.clone();
        let t_err = std::thread::spawn(move || pump(stderr, tx_err, false));

        let metrics_stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        {
            let tx = tx.clone();
            let stop = metrics_stop.clone();
            std::thread::spawn(move || {
                let mut prev_cpu = read_cpu_times();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    let now_cpu = read_cpu_times();
                    let cpu_percent = cpu_percent_between(prev_cpu, now_cpu);
                    prev_cpu = now_cpu;
                    if tx
                        .send(GuestMessage::Metrics {
                            mem_mib: mem_used_mib(),
                            disk_used_mib: disk_used_mib(),
                            cpu_percent,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
            });
        }

        // Reap the child on its own thread: the command's lifetime is
        // independent of any control connection.
        let exited = session.exited.clone();
        std::thread::spawn(move || {
            t_out.join().ok();
            t_err.join().ok();
            let info = match child.wait() {
                Ok(status) => {
                    use std::os::unix::process::ExitStatusExt;
                    ExitInfo {
                        code: status.code(),
                        signal: status.signal(),
                    }
                }
                Err(e) => {
                    log(&format!("waiting for child: {e}"));
                    ExitInfo { code: None, signal: None }
                }
            };
            metrics_stop.store(true, std::sync::atomic::Ordering::Relaxed);
            *exited.lock().unwrap() = Some(info);
            // Dropped if nobody is attached; the reattach handshake replays it.
            tx.send(GuestMessage::Exited { info }).ok();
        });
        Ok(())
    }

    /// Generic mount(2) wrapper. Returns false (and logs) on failure.
    fn do_mount(
        src: &str,
        target: &str,
        fstype: &str,
        flags: libc::c_ulong,
        data: Option<&str>,
    ) -> bool {
        let src_c = std::ffi::CString::new(src).unwrap();
        let target_c = std::ffi::CString::new(target).unwrap();
        let fstype_c = std::ffi::CString::new(fstype).unwrap();
        let data_c = data.map(|d| std::ffi::CString::new(d).unwrap());
        let data_ptr = data_c
            .as_ref()
            .map_or(std::ptr::null(), |c| c.as_ptr() as *const libc::c_void);
        let rc = unsafe {
            libc::mount(src_c.as_ptr(), target_c.as_ptr(), fstype_c.as_ptr(), flags, data_ptr)
        };
        if rc != 0 {
            log(&format!(
                "mount {src} -> {target} ({fstype}): {}",
                std::io::Error::last_os_error()
            ));
            false
        } else {
            true
        }
    }

    /// Build the agent's root filesystem for this run.
    ///
    /// The daemon attaches the shared runtime rootfs as `/dev/vda` (read-only
    /// squashfs) and a fresh per-sandbox writable disk as `/dev/vdb`. We union
    /// them with overlayfs and return the union path to chroot into. Writes go
    /// to the overlay (copy-up), so `pip install` etc. work and never touch the
    /// shared image. Returns `None` when no rootfs disk is present (the guest
    /// then runs the command directly in the initramfs — enough for busybox).
    fn setup_overlay_root() -> Option<&'static str> {
        const LOWER: &str = "/lower";
        const OVER: &str = "/over";
        const ROOT: &str = "/newroot";

        // virtio_blk was just loaded; give the device nodes a moment to appear.
        let mut waited = 0;
        while !std::path::Path::new("/dev/vda").exists() && waited < 40 {
            std::thread::sleep(std::time::Duration::from_millis(50));
            waited += 1;
        }
        if !std::path::Path::new("/dev/vda").exists() {
            log("no rootfs disk (/dev/vda); running in the initramfs");
            return None;
        }
        for d in [LOWER, OVER, ROOT] {
            std::fs::create_dir_all(d).ok();
        }
        if !do_mount("/dev/vda", LOWER, "squashfs", libc::MS_RDONLY, None) {
            return None;
        }

        if std::path::Path::new("/dev/vdb").exists() {
            format_overlay(LOWER, "/dev/vdb");
            if do_mount("/dev/vdb", OVER, "ext4", 0, None) {
                std::fs::create_dir_all(format!("{OVER}/upper")).ok();
                std::fs::create_dir_all(format!("{OVER}/work")).ok();
                let opts = format!("lowerdir={LOWER},upperdir={OVER}/upper,workdir={OVER}/work");
                if do_mount("overlay", ROOT, "overlay", 0, Some(&opts)) {
                    mount_pseudo_into(ROOT);
                    return Some(ROOT);
                }
            }
        }
        // No usable overlay: run on the read-only rootfs (runtimes present,
        // writes limited to a tmpfs /tmp).
        log("overlay disk unavailable; using read-only rootfs");
        mount_pseudo_into(LOWER);
        Some(LOWER)
    }

    /// Format the per-sandbox overlay disk ext4. mkfs.ext4 lives in the rootfs,
    /// so run it chrooted there with a devtmpfs exposing the block node. Lazy
    /// init keeps this well under a second even on a large sparse disk.
    fn format_overlay(lower: &str, dev: &str) {
        let devdir = format!("{lower}/dev");
        std::fs::create_dir_all(&devdir).ok();
        do_mount("devtmpfs", &devdir, "devtmpfs", 0, None);

        let lower_owned = lower.to_string();
        let mut cmd = Command::new("/sbin/mkfs.ext4");
        // nodiscard: some VMMs (Cloud Hypervisor over a raw file) error the
        // TRIM/DISCARD that mkfs issues first, which cascades into failed
        // writes. We don't need discard on a fresh throwaway disk.
        cmd.args([
            "-F",
            "-q",
            "-E",
            "lazy_itable_init=1,lazy_journal_init=1,nodiscard",
            dev,
        ])
            .env("PATH", "/sbin:/usr/sbin:/bin:/usr/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        unsafe {
            cmd.pre_exec(move || {
                let c = std::ffi::CString::new(lower_owned.as_str()).unwrap();
                if libc::chroot(c.as_ptr()) != 0 || libc::chdir(c"/".as_ptr()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        match cmd.output() {
            Ok(o) if o.status.success() => log(&format!("formatted {dev} ext4")),
            Ok(o) => log(&format!(
                "mkfs.ext4 failed: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => log(&format!("mkfs.ext4 spawn: {e}")),
        }
        let devdir_c = std::ffi::CString::new(devdir).unwrap();
        unsafe { libc::umount(devdir_c.as_ptr()) };
    }

    /// Mount proc/sys/dev and a writable tmpfs /tmp under a chroot target.
    fn mount_pseudo_into(root: &str) {
        for (src, sub, fstype) in [
            ("proc", "proc", "proc"),
            ("sysfs", "sys", "sysfs"),
            ("devtmpfs", "dev", "devtmpfs"),
            ("tmpfs", "tmp", "tmpfs"),
        ] {
            let target = format!("{root}/{sub}");
            std::fs::create_dir_all(&target).ok();
            do_mount(src, &target, fstype, 0, None);
        }
    }

    /// Mount virtio-fs shares announced in Exec: (tag, guest_path, read_only),
    /// rooted under `root` (the chroot target, or "" for the initramfs).
    /// Read-only is *also* enforced host-side; the guest flag is belt-and-braces.
    fn mount_shares(root: &str, mounts: &[(String, String, bool)]) {
        for (tag, guest_path, read_only) in mounts {
            let target = format!("{root}{guest_path}");
            std::fs::create_dir_all(&target).ok();
            let flags = if *read_only { libc::MS_RDONLY } else { 0 };
            if do_mount(tag, &target, "virtiofs", flags, None) {
                log(&format!("mounted {tag} -> {target} (ro={read_only})"));
            }
        }
    }

    /// Guest-side loopback port the proxy env vars point at.
    const PROXY_LISTEN_PORT: u16 = 3128;

    fn bring_up_loopback() {
        // SIOCSIFFLAGS on "lo": IFF_UP | IFF_RUNNING.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if fd < 0 {
                return;
            }
            let mut ifr: libc::ifreq = std::mem::zeroed();
            for (i, b) in b"lo\0".iter().enumerate() {
                ifr.ifr_name[i] = *b as libc::c_char;
            }
            // ioctl's request parameter is c_ulong on glibc but c_int on
            // musl; an inferred cast compiles against both.
            if libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut ifr) == 0 {
                ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
                libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr);
            }
            libc::close(fd);
        }
    }

    /// Accept loopback TCP connections and pipe each to a fresh vsock
    /// connection to the host's egress proxy. Dumb pipe: all policy and HTTP
    /// parsing happens host-side, where the guest can't touch it.
    fn proxy_forwarder() {
        let listener = match std::net::TcpListener::bind(("127.0.0.1", PROXY_LISTEN_PORT)) {
            Ok(l) => l,
            Err(e) => {
                log(&format!("proxy listener bind: {e}"));
                return;
            }
        };
        log(&format!("proxy forwarder on 127.0.0.1:{PROXY_LISTEN_PORT}"));
        for conn in listener.incoming() {
            let Ok(tcp) = conn else { continue };
            std::thread::spawn(move || {
                let host = match vsock_connect_host(agentos_core::HOST_PROXY_PORT) {
                    Ok(fd) => fd,
                    Err(e) => {
                        log(&format!("proxy vsock connect: {e}"));
                        return;
                    }
                };
                let mut host_r = unsafe { std::fs::File::from_raw_fd(dup(&host).unwrap_or(-1)) };
                let mut host_w = unsafe { std::fs::File::from_raw_fd(dup(&host).unwrap_or(-1)) };
                let mut tcp_r = tcp.try_clone().expect("clone tcp");
                let mut tcp_w = tcp;
                let t = std::thread::spawn(move || {
                    std::io::copy(&mut tcp_r, &mut host_w).ok();
                    // half-close towards host so responses can still drain
                    unsafe { libc::shutdown(host_w.as_raw_fd(), libc::SHUT_WR) };
                });
                std::io::copy(&mut host_r, &mut tcp_w).ok();
                tcp_w.shutdown(std::net::Shutdown::Write).ok();
                t.join().ok();
            });
        }
    }

    fn vsock_connect_host(port: u32) -> std::io::Result<OwnedFd> {
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let addr = libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as libc::sa_family_t,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: libc::VMADDR_CID_HOST,
            svm_zero: [0; 4],
        };
        let rc = unsafe {
            libc::connect(
                fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }

    /// Used memory in MiB from /proc/meminfo (MemTotal - MemAvailable).
    fn mem_used_mib() -> u32 {
        let Ok(s) = std::fs::read_to_string("/proc/meminfo") else {
            return 0;
        };
        let field = |name: &str| -> u64 {
            s.lines()
                .find(|l| l.starts_with(name))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
                .unwrap_or(0)
        };
        let used_kib = field("MemTotal:").saturating_sub(field("MemAvailable:"));
        (used_kib / 1024) as u32
    }

    /// Bytes written into the writable overlay, in MiB. Measured on the ext4
    /// overlay itself (`/over`) — that's the filesystem `disk_mib` caps. Falls
    /// back to the union root, then to whatever `/` is, so the initramfs-only
    /// path still reports something rather than lying with 0.
    fn disk_used_mib() -> u32 {
        for path in ["/over", "/newroot", "/"] {
            let Ok(c_path) = std::ffi::CString::new(path) else {
                continue;
            };
            let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
            if unsafe { libc::statvfs(c_path.as_ptr(), &mut st) } != 0 || st.f_blocks == 0 {
                continue;
            }
            let used_blocks = (st.f_blocks as u64).saturating_sub(st.f_bfree as u64);
            return ((used_blocks * st.f_frsize as u64) / (1024 * 1024)) as u32;
        }
        0
    }

    /// (busy_jiffies, total_jiffies) from the aggregate `cpu` line of /proc/stat.
    fn read_cpu_times() -> (u64, u64) {
        let Ok(s) = std::fs::read_to_string("/proc/stat") else {
            return (0, 0);
        };
        let Some(line) = s.lines().find(|l| l.starts_with("cpu ")) else {
            return (0, 0);
        };
        let vals: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|v| v.parse().ok())
            .collect();
        let total: u64 = vals.iter().sum();
        // idle = idle (index 3) + iowait (index 4).
        let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
        (total.saturating_sub(idle), total)
    }

    /// CPU% (0..=100, across all vCPUs) from two /proc/stat samples.
    fn cpu_percent_between(prev: (u64, u64), now: (u64, u64)) -> u32 {
        let busy = now.0.saturating_sub(prev.0);
        let total = now.1.saturating_sub(prev.1);
        if total == 0 {
            0
        } else {
            ((busy * 100) / total).min(100) as u32
        }
    }

    /// Read a child output stream and emit protocol frames.
    fn pump(mut from: impl Read, tx: mpsc::Sender<GuestMessage>, is_stdout: bool) {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let data = buf[..n].to_vec();
                    let msg = if is_stdout {
                        GuestMessage::Stdout { data }
                    } else {
                        GuestMessage::Stderr { data }
                    };
                    if tx.send(msg).is_err() {
                        break;
                    }
                }
            }
        }
    }

    // ---- framing: u32 LE length + JSON ----

    fn read_frame(r: &mut impl Read) -> std::io::Result<HostMessage> {
        let mut len = [0u8; 4];
        r.read_exact(&mut len)?;
        let len = u32::from_le_bytes(len) as usize;
        if len > 1 << 20 {
            return Err(other_err(format!("oversized frame: {len}")));
        }
        let mut body = vec![0u8; len];
        r.read_exact(&mut body)?;
        serde_json::from_slice(&body).map_err(|e| other_err(e.to_string()))
    }

    fn write_frame(w: &mut impl Write, msg: &GuestMessage) -> std::io::Result<()> {
        let body = serde_json::to_vec(msg).map_err(|e| other_err(e.to_string()))?;
        w.write_all(&(body.len() as u32).to_le_bytes())?;
        w.write_all(&body)?;
        w.flush()
    }

    fn other_err(msg: String) -> std::io::Error {
        std::io::Error::other(msg)
    }

    // ---- vsock via libc ----

    fn vsock_listen(port: u32) -> std::io::Result<OwnedFd> {
        use std::os::fd::FromRawFd as _;
        let fd = unsafe { libc::socket(libc::AF_VSOCK, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let addr = libc::sockaddr_vm {
            svm_family: libc::AF_VSOCK as libc::sa_family_t,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: libc::VMADDR_CID_ANY,
            svm_zero: [0; 4],
        };
        let rc = unsafe {
            libc::bind(
                fd.as_raw_fd(),
                &addr as *const _ as *const libc::sockaddr,
                std::mem::size_of::<libc::sockaddr_vm>() as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        if unsafe { libc::listen(fd.as_raw_fd(), 1) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }

    fn vsock_accept(listener: &OwnedFd) -> std::io::Result<OwnedFd> {
        let fd = unsafe { libc::accept(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    fn dup(fd: &OwnedFd) -> std::io::Result<i32> {
        let n = unsafe { libc::dup(fd.as_raw_fd()) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(n)
    }

    fn power_off() -> ! {
        // Flush console output before the world ends.
        std::io::stderr().flush().ok();
        unsafe {
            libc::sync();
            libc::reboot(libc::RB_POWER_OFF);
        }
        // If reboot() failed (not PID 1 in tests), just exit.
        std::process::exit(0)
    }
}
