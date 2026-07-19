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

    /// Accept one control connection and run one session on it.
    fn serve() -> std::io::Result<()> {
        let listener = vsock_listen(GUEST_CONTROL_PORT)?;
        log(&format!("listening on vsock port {GUEST_CONTROL_PORT}"));
        let conn = vsock_accept(&listener)?;
        log("daemon connected");
        session(conn)
    }

    fn session(conn: OwnedFd) -> std::io::Result<()> {
        let mut reader = unsafe { std::fs::File::from_raw_fd(dup(&conn)?) };
        let mut writer = unsafe { std::fs::File::from_raw_fd(dup(&conn)?) };

        // Handshake: daemon speaks first.
        match read_frame(&mut reader)? {
            HostMessage::Hello { version } if version == PROTOCOL_VERSION => {
                write_frame(&mut writer, &GuestMessage::Hello { version: PROTOCOL_VERSION })?;
            }
            other => {
                return Err(other_err(format!("expected Hello v{PROTOCOL_VERSION}, got {other:?}")));
            }
        }

        // Wait for Exec.
        let (command, env) = loop {
            match read_frame(&mut reader)? {
                HostMessage::Exec { command, env, .. } => break (command, env),
                HostMessage::Hello { .. } => continue,
                HostMessage::Stdin { .. } => continue, // no child yet
                HostMessage::Terminate => return Ok(()),
            }
        };
        if command.is_empty() {
            return Err(other_err("empty command".into()));
        }
        log(&format!("exec: {command:?}"));

        let mut child = Command::new(&command[0])
            .args(&command[1..])
            .envs(env.iter().map(|(k, v)| (k, v)))
            .env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| other_err(format!("spawn {:?}: {e}", command[0])))?;

        // Three pumps feed one outgoing channel: stdout frames, stderr frames,
        // and the reader thread forwarding Stdin/Terminate to the child.
        let (tx, rx) = mpsc::channel::<GuestMessage>();

        let mut child_stdin = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let tx_out = tx.clone();
        let t_out = std::thread::spawn(move || pump(stdout, tx_out, true));
        let tx_err = tx.clone();
        let t_err = std::thread::spawn(move || pump(stderr, tx_err, false));
        drop(tx);

        // Host → child stdin, on its own thread so we can block on frames.
        std::thread::spawn(move || {
            loop {
                match read_frame(&mut reader) {
                    Ok(HostMessage::Stdin { data }) => {
                        if let Some(stdin) = child_stdin.as_mut() {
                            if data.is_empty() {
                                child_stdin = None; // EOF
                            } else if stdin.write_all(&data).is_err() {
                                child_stdin = None;
                            }
                        }
                    }
                    Ok(HostMessage::Terminate) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });

        // Forward output frames until both pumps finish.
        for msg in rx {
            write_frame(&mut writer, &msg)?;
        }
        t_out.join().ok();
        t_err.join().ok();

        let status = child.wait()?;
        #[allow(clippy::useless_conversion)]
        let info = {
            use std::os::unix::process::ExitStatusExt;
            ExitInfo {
                code: status.code(),
                signal: status.signal(),
            }
        };
        write_frame(&mut writer, &GuestMessage::Exited { info })?;
        Ok(())
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
