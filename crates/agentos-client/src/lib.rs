//! Embeddable client for Agent OS — the API to drive hardware-isolated agent
//! sandboxes from your own program.
//!
//! Everything privileged (booting VMs, policy, the kill switch) lives in the
//! `agentosd` daemon; this crate is a typed client for it. The daemon is
//! started automatically if it isn't already running.
//!
//! ```no_run
//! use agentos_client::{Client, RunEvent};
//! use agentos_core::{NetPolicy, SandboxSpec};
//!
//! # async fn example() -> Result<(), agentos_client::Error> {
//! let client = Client::new();
//! let mut spec = SandboxSpec::command(["python3", "-c", "print('hi')"]);
//! spec.net = NetPolicy::Allowlist(vec!["pypi.org".into()]);
//!
//! let mut run = client.run(&spec).await?;
//! while let Some(event) = run.next().await? {
//!     match event {
//!         RunEvent::Stdout(bytes) => print!("{}", String::from_utf8_lossy(&bytes)),
//!         RunEvent::Exited { code, .. } => println!("exited: {code:?}"),
//!         _ => {}
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Wire format (internal): one JSON object per line. Unary methods get one
//! response line; streaming methods dedicate the connection and yield a line
//! per event.

use agentos_core::event::Event;
use agentos_core::{SandboxId, SandboxSpec, SandboxState};
use serde::Deserialize;
use serde_json::{json, Value};

use tokio::net::unix::OwnedWriteHalf;

/// Anything that can go wrong talking to the daemon.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The daemon refused the request — e.g. fleet policy, or an invalid spec.
    #[error("{0}")]
    Daemon(String),
    /// The daemon could not be reached or started.
    #[error("cannot reach agentosd: {0}")]
    Unreachable(String),
    /// The connection dropped or a reply could not be decoded.
    #[error("protocol error: {0}")]
    Protocol(String),
}

/// Result of a typed client call.
pub type ClientResult<T> = std::result::Result<T, Error>;

/// One sandbox as reported by [`Client::list`].
#[derive(Debug, Clone, Deserialize)]
pub struct SandboxInfo {
    pub id: SandboxId,
    pub name: String,
    pub state: SandboxState,
}

/// What a running (or restoring) sandbox tells you.
#[derive(Debug, Clone)]
pub enum RunEvent {
    /// The sandbox exists; its VM is booting.
    Created(SandboxId),
    /// A snapshotted sandbox is being brought back.
    Restoring(SandboxId),
    /// Cloning `--repo` host-side, before the guest starts.
    Cloning { url: String },
    /// The agent command is now executing.
    Running,
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
    /// The command finished on its own.
    Exited { code: Option<i32>, signal: Option<i32> },
    /// The kill switch fired (manually or via an auto-kill rule).
    Terminated {
        reason: String,
        /// Present when the sandbox directory was kept for inspection.
        saved_dir: Option<String>,
    },
    /// The VM state was written to disk and the VM torn down.
    Snapshotted { dir: String },
    /// An event this client version doesn't know about; safe to ignore.
    Unknown(serde_json::Value),
}

/// Writes to the sandbox command's standard input.
///
/// Taken once from a [`RunStream`] so it can be moved into its own task —
/// an interactive agent needs stdin flowing *while* output streams back.
pub struct StdinSender {
    write: OwnedWriteHalf,
}

impl StdinSender {
    /// Feed bytes to the command's stdin.
    pub async fn send(&mut self, data: &[u8]) -> ClientResult<()> {
        self.write_line(&json!({ "stdin": data })).await
    }

    /// Close stdin, so a command reading to EOF (`cat`, `sort`) finishes.
    pub async fn close(&mut self) -> ClientResult<()> {
        self.write_line(&json!({ "stdin": Value::Null })).await
    }

    async fn write_line(&mut self, v: &Value) -> ClientResult<()> {
        let mut line = serde_json::to_vec(v)
            .map_err(|e| Error::Protocol(format!("encoding stdin: {e}")))?;
        line.push(b'\n');
        self.write
            .write_all(&line)
            .await
            .map_err(|e| Error::Protocol(format!("writing stdin: {e}")))?;
        self.write
            .flush()
            .await
            .map_err(|e| Error::Protocol(format!("flushing stdin: {e}")))
    }
}

/// The event stream of one `run` or `restore`.
pub struct RunStream {
    inner: EventStream,
}

impl RunStream {
    /// Take the handle for writing to the command's stdin. Returns `None` on
    /// the second call — there is only one stdin.
    pub fn stdin(&mut self) -> Option<StdinSender> {
        self.inner.write.take().map(|write| StdinSender { write })
    }

    /// Next event, or `None` when the sandbox is finished.
    pub async fn next(&mut self) -> ClientResult<Option<RunEvent>> {
        let Some(v) = self.inner.next().await.map_err(Error::Protocol)? else {
            return Ok(None);
        };
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .or(Some(err))
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            return Err(Error::Daemon(msg.to_string()));
        }
        let bytes = |v: &serde_json::Value| -> Vec<u8> {
            v.as_array()
                .map(|a| a.iter().filter_map(|b| b.as_u64().map(|b| b as u8)).collect())
                .unwrap_or_default()
        };
        let id = |v: &serde_json::Value| -> ClientResult<SandboxId> {
            serde_json::from_value(v["id"].clone())
                .map_err(|e| Error::Protocol(format!("bad sandbox id: {e}")))
        };
        Ok(Some(match v["event"].as_str() {
            Some("created") => RunEvent::Created(id(&v)?),
            Some("restoring") => RunEvent::Restoring(id(&v)?),
            Some("cloning") => RunEvent::Cloning {
                url: v["url"].as_str().unwrap_or_default().to_string(),
            },
            Some("running") => RunEvent::Running,
            Some("stdout") => RunEvent::Stdout(bytes(&v["data"])),
            Some("stderr") => RunEvent::Stderr(bytes(&v["data"])),
            Some("exited") => RunEvent::Exited {
                code: v["code"].as_i64().map(|c| c as i32),
                signal: v["signal"].as_i64().map(|s| s as i32),
            },
            Some("terminated") => RunEvent::Terminated {
                reason: v["reason"].as_str().unwrap_or("kill switch").to_string(),
                saved_dir: v["saved_dir"].as_str().map(String::from),
            },
            Some("snapshotted") => RunEvent::Snapshotted {
                dir: v["dir"].as_str().unwrap_or_default().to_string(),
            },
            Some("error") => {
                return Err(Error::Daemon(
                    v["message"].as_str().unwrap_or("unknown error").to_string(),
                ))
            }
            _ => RunEvent::Unknown(v),
        }))
    }
}

/// The daemon's machine-wide event bus: state changes, network verdicts, and
/// resource samples for every sandbox.
pub struct EventSubscription {
    inner: EventStream,
}

impl EventSubscription {
    pub async fn next(&mut self) -> ClientResult<Option<Event>> {
        match self.inner.next().await.map_err(Error::Protocol)? {
            None => Ok(None),
            Some(v) => serde_json::from_value(v)
                .map(Some)
                .map_err(|e| Error::Protocol(format!("bad event: {e}"))),
        }
    }
}

/// Handle to the local Agent OS daemon.
///
/// Cheap to create and safe to share; each call opens its own short-lived
/// connection, so a `Client` holds no state that can go stale.
#[derive(Debug, Clone, Default)]
pub struct Client;

impl Client {
    pub fn new() -> Self {
        Self
    }

    /// Boot a sandbox and stream it. The daemon applies fleet policy here, so
    /// this can fail with [`Error::Daemon`] before anything runs.
    pub async fn run(&self, spec: &SandboxSpec) -> ClientResult<RunStream> {
        let params = serde_json::to_value(spec)
            .map_err(|e| Error::Protocol(format!("serialising spec: {e}")))?;
        Ok(RunStream {
            inner: open_stream("sandbox.run", params).await.map_err(Error::Unreachable)?,
        })
    }

    /// Bring a snapshotted sandbox back, resuming its command mid-execution.
    pub async fn restore(&self, id: &SandboxId) -> ClientResult<RunStream> {
        Ok(RunStream {
            inner: open_stream("sandbox.restore", serde_json::json!({ "id": id }))
                .await
                .map_err(Error::Unreachable)?,
        })
    }

    pub async fn list(&self) -> ClientResult<Vec<SandboxInfo>> {
        let v = self.call("sandbox.list", serde_json::json!(null)).await?;
        serde_json::from_value(v).map_err(|e| Error::Protocol(format!("bad sandbox list: {e}")))
    }

    /// The kill switch: destroy the VM immediately. `save` keeps the sandbox
    /// directory (logs, overlay) for inspection instead of wiping it.
    pub async fn kill(&self, id: &SandboxId, save: bool) -> ClientResult<()> {
        self.call("sandbox.kill", serde_json::json!({ "id": id, "save": save }))
            .await
            .map(drop)
    }

    /// Freeze a running sandbox's vCPUs.
    pub async fn pause(&self, id: &SandboxId) -> ClientResult<()> {
        self.call("sandbox.pause", serde_json::json!({ "id": id })).await.map(drop)
    }

    /// Continue a paused sandbox where it left off.
    pub async fn resume(&self, id: &SandboxId) -> ClientResult<()> {
        self.call("sandbox.resume", serde_json::json!({ "id": id })).await.map(drop)
    }

    /// Write the VM's state to disk and tear it down; returns the directory
    /// holding the snapshot. Bring it back with [`Client::restore`].
    pub async fn snapshot(&self, id: &SandboxId) -> ClientResult<String> {
        let v = self.call("sandbox.snapshot", serde_json::json!({ "id": id })).await?;
        Ok(v["dir"].as_str().unwrap_or_default().to_string())
    }

    /// Subscribe to the daemon's event bus.
    pub async fn events(&self) -> ClientResult<EventSubscription> {
        Ok(EventSubscription {
            inner: open_stream("events.subscribe", serde_json::json!(null))
                .await
                .map_err(Error::Unreachable)?,
        })
    }

    async fn call(&self, method: &str, params: serde_json::Value) -> ClientResult<serde_json::Value> {
        unary(method, params).await.map_err(|e| {
            // The transport layer reports both "can't reach the daemon" and
            // "the daemon said no" as strings; keep them distinguishable.
            if e.starts_with("daemon not running") || e.starts_with("cannot") || e.contains("did not come up") {
                Error::Unreachable(e)
            } else {
                Error::Daemon(e)
            }
        })
    }
}

use std::path::PathBuf;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::UnixStream;

fn socket_path() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").expect("HOME not set")).join(".agentos/agentosd.sock")
}

/// Locate the `agentosd` binary.
///
/// An embedder's executable is *not* a sibling of the daemon the way our own
/// CLI is, so look wider: an explicit override, then next to the current
/// executable, then one level up (cargo puts examples and tests in a
/// subdirectory), then `PATH` — the normal case for an installed system.
fn find_daemon() -> std::result::Result<PathBuf, String> {
    let mut tried = Vec::new();

    if let Some(p) = std::env::var_os("AGENTOSD_PATH") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        tried.push(p.display().to_string());
    }

    if let Ok(exe) = std::env::current_exe() {
        for candidate in [
            exe.with_file_name("agentosd"),
            // target/debug/examples/foo -> target/debug/agentosd
            exe.parent()
                .and_then(|d| d.parent())
                .map(|d| d.join("agentosd"))
                .unwrap_or_default(),
        ] {
            if candidate.is_file() {
                return Ok(candidate);
            }
            tried.push(candidate.display().to_string());
        }
    }

    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join("agentosd");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
        tried.push("agentosd on PATH".to_string());
    }

    Err(format!(
        "daemon not running and agentosd could not be found (looked at: {}). \
         Start it manually, put it on PATH, or set AGENTOSD_PATH.",
        tried.join(", ")
    ))
}

/// Connect to the daemon, starting `agentosd` if it isn't running yet.
pub async fn connect() -> Result<UnixStream, String> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path).await {
        return Ok(s);
    }

    let daemon = find_daemon()?;
    let log_dir = path.parent().expect("socket has parent");
    std::fs::create_dir_all(log_dir).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(log_dir.join("agentosd.log")).map_err(|e| e.to_string())?;
    std::process::Command::new(&daemon)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            log.try_clone().map_err(|e| e.to_string())?,
        ))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", daemon.display()))?;

    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if let Ok(s) = UnixStream::connect(&path).await {
            return Ok(s);
        }
    }
    Err("daemon did not come up within 5s (see ~/.agentos/agentosd.log)".into())
}

async fn send_request(stream: &mut UnixStream, method: &str, params: &Value) -> Result<(), String> {
    let mut line =
        serde_json::to_vec(&json!({ "id": 1, "method": method, "params": params }))
            .map_err(|e| e.to_string())?;
    line.push(b'\n');
    stream.write_all(&line).await.map_err(|e| e.to_string())?;
    stream.flush().await.map_err(|e| e.to_string())
}

/// One-shot request/response (e.g. `sandbox.list`, `sandbox.kill`).
pub async fn unary(method: &str, params: Value) -> Result<Value, String> {
    let mut stream = connect().await?;
    send_request(&mut stream, method, &params).await?;
    let (read, _keep_write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let line = lines
        .next_line()
        .await
        .map_err(|e| e.to_string())?
        .ok_or("daemon closed connection")?;
    let v: Value = serde_json::from_str(&line).map_err(|e| e.to_string())?;
    if let Some(err) = v.get("error") {
        return Err(err["message"].as_str().unwrap_or("unknown error").to_string());
    }
    Ok(v["result"].clone())
}

/// A dedicated streaming connection; `next()` yields one JSON value per line.
pub struct EventStream {
    lines: Lines<BufReader<OwnedReadHalf>>,
    // The write half stays open for the life of the stream: it carries stdin
    // for a run, and closing it early would look like the client went away.
    write: Option<OwnedWriteHalf>,
}

impl EventStream {
    pub async fn next(&mut self) -> Result<Option<Value>, String> {
        match self.lines.next_line().await.map_err(|e| e.to_string())? {
            None => Ok(None),
            Some(line) => serde_json::from_str(&line)
                .map(Some)
                .map_err(|e| e.to_string()),
        }
    }
}

/// Open a streaming method (`sandbox.run` with a spec, or `events.subscribe`).
pub async fn open_stream(method: &str, params: Value) -> Result<EventStream, String> {
    let mut stream = connect().await?;
    send_request(&mut stream, method, &params).await?;
    let (read, write) = stream.into_split();
    Ok(EventStream {
        lines: BufReader::new(read).lines(),
        write: Some(write),
    })
}
