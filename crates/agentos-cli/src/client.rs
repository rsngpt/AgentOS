//! CLI presentation on top of the `agentos-client` SDK.
//!
//! This file is deliberately thin: if driving Agent OS from the CLI needs
//! anything the SDK can't express, that's a gap an embedder would hit too.

use std::io::Write as _;

use agentos_client::{Client, RunEvent};
use agentos_core::{SandboxId, SandboxSpec};

fn client() -> Client {
    Client::new()
}

/// Parse a user-supplied sandbox id.
pub fn parse_id(id: &str) -> Result<SandboxId, String> {
    serde_json::from_value(serde_json::Value::String(id.to_string()))
        .map_err(|_| format!("not a valid sandbox id: {id}"))
}

/// Run a sandbox, streaming its output to our stdio; returns the exit code.
pub async fn run(spec: SandboxSpec) -> Result<i32, String> {
    let stream = client().run(&spec).await.map_err(|e| e.to_string())?;
    stream_run(stream).await
}

/// Restore a snapshotted sandbox and stream it exactly like `run`.
pub async fn restore(id: &str) -> Result<i32, String> {
    let id = parse_id(id)?;
    let stream = client().restore(&id).await.map_err(|e| e.to_string())?;
    stream_run(stream).await
}

async fn stream_run(mut stream: agentos_client::RunStream) -> Result<i32, String> {
    // Forward our stdin to the sandbox so interactive agents work. Runs
    // concurrently with the output loop, and is aborted when the run ends so
    // a blocked read on a terminal doesn't keep the process alive.
    let stdin_task = stream.stdin().map(|mut sender| {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt as _;
            let mut input = tokio::io::stdin();
            let mut buf = [0u8; 8192];
            loop {
                match input.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sender.send(&buf[..n]).await.is_err() {
                            return;
                        }
                    }
                }
            }
            // Our stdin hit EOF: close the guest's too, so `cat`-style
            // commands finish instead of waiting forever.
            sender.close().await.ok();
        })
    });

    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    loop {
        let event = stream.next().await.map_err(|e| e.to_string())?;
        let Some(event) = event else {
            return Err("daemon connection closed unexpectedly".into());
        };
        match event {
            RunEvent::Created(id) => eprintln!("sandbox {id} created"),
            RunEvent::Restoring(id) => eprintln!("restoring sandbox {id}"),
            RunEvent::Cloning { url } => eprintln!("cloning {url}"),
            RunEvent::Running | RunEvent::Unknown(_) => {}
            RunEvent::Stdout(data) => {
                stdout.write_all(&data).map_err(|e| e.to_string())?;
                stdout.flush().ok();
            }
            RunEvent::Stderr(data) => {
                stderr.write_all(&data).map_err(|e| e.to_string())?;
                stderr.flush().ok();
            }
            RunEvent::Exited { code, .. } => {
                if let Some(t) = &stdin_task {
                    t.abort();
                }
                return Ok(code.unwrap_or(1));
            }
            RunEvent::Terminated { reason, saved_dir } => {
                eprintln!("sandbox terminated ({reason})");
                if let Some(dir) = saved_dir {
                    eprintln!("sandbox state saved at {dir}");
                }
                if let Some(t) = &stdin_task {
                    t.abort();
                }
                return Ok(137);
            }
            RunEvent::Snapshotted { dir } => {
                eprintln!("sandbox snapshotted; state in {dir}");
                if let Some(t) = &stdin_task {
                    t.abort();
                }
                return Ok(0);
            }
        }
    }
}

pub async fn list() -> Result<i32, String> {
    let rows = client().list().await.map_err(|e| e.to_string())?;
    if rows.is_empty() {
        println!("no sandboxes");
        return Ok(0);
    }
    println!("{:<38} {:<16} STATE", "ID", "NAME");
    for sb in rows {
        // The state enum is tagged, so its JSON tag is the display name.
        let state = serde_json::to_value(&sb.state)
            .ok()
            .and_then(|v| v["state"].as_str().map(String::from))
            .unwrap_or_else(|| "?".into());
        println!("{:<38} {:<16} {}", sb.id.to_string(), sb.name, state);
    }
    Ok(0)
}

pub async fn kill(id: &str, save: bool) -> Result<i32, String> {
    client()
        .kill(&parse_id(id)?, save)
        .await
        .map_err(|e| e.to_string())?;
    println!("killed");
    Ok(0)
}

/// Adjust a running sandbox's grants. Prints what is *actually* in force
/// afterwards rather than echoing the request, because a fleet policy may have
/// clamped it — the user needs to see the difference.
pub async fn set_permissions(
    id: &str,
    net: Option<&str>,
    kill_over_mem: Option<u32>,
    kill_over_egress: Option<u32>,
    kill_after_secs: Option<u64>,
) -> Result<i32, String> {
    let rules = if kill_over_mem.is_some() || kill_over_egress.is_some() || kill_after_secs.is_some()
    {
        Some(agentos_core::AutoKillRules {
            max_mem_mib: kill_over_mem,
            max_egress_mib: kill_over_egress,
            max_runtime_secs: kill_after_secs,
        })
    } else {
        None
    };
    let now = client()
        .set_permissions(&parse_id(id)?, net, rules)
        .await
        .map_err(|e| e.to_string())?;
    println!("net: {}", now.net);
    let ak = now.auto_kill;
    if ak.max_mem_mib.is_none() && ak.max_egress_mib.is_none() && ak.max_runtime_secs.is_none() {
        println!("auto-kill: none");
    } else {
        let describe = |label: &str, v: Option<String>| v.map(|v| format!("{label}={v}"));
        let parts: Vec<String> = [
            describe("mem_mib", ak.max_mem_mib.map(|v| v.to_string())),
            describe("egress_mib", ak.max_egress_mib.map(|v| v.to_string())),
            describe("runtime_secs", ak.max_runtime_secs.map(|v| v.to_string())),
        ]
        .into_iter()
        .flatten()
        .collect();
        println!("auto-kill: {}", parts.join(" "));
    }
    Ok(0)
}

/// Export a snapshot as a shareable bundle.
pub async fn export(id: &str, out: Option<&str>) -> Result<i32, String> {
    let sid = parse_id(id)?;
    let dest = match out {
        Some(p) => p.to_string(),
        None => format!("{sid}.agentos"),
    };
    let bytes = client()
        .export(&sid, &dest)
        .await
        .map_err(|e| e.to_string())?;
    println!("exported {dest} ({:.1} MiB)", bytes as f64 / (1024.0 * 1024.0));
    println!("import it elsewhere with: agentos import {dest}");
    Ok(0)
}

/// Import a bundle someone else exported.
pub async fn import(path: &str, remap: &[String]) -> Result<i32, String> {
    let id = client()
        .import(path, remap)
        .await
        .map_err(|e| e.to_string())?;
    println!("imported as {id}");
    println!("resume it with: agentos restore {id}");
    Ok(0)
}

/// PRD §8 success metrics, computed locally.
pub async fn metrics() -> Result<i32, String> {
    let m = client().metrics().await.map_err(|e| e.to_string())?;
    let n = |k: &str| m[k].as_u64().unwrap_or(0);
    let started = n("sandboxes_started");

    println!("Agent OS metrics — this install only, nothing is transmitted\n");

    println!("Time to boot (PRD §8)");
    if started == 0 {
        println!("  no sandboxes recorded yet");
    } else {
        println!(
            "  p50 {} ms   p95 {} ms   min {} ms   max {} ms   (n={started})",
            n("boot_ms_p50"),
            n("boot_ms_p95"),
            n("boot_ms_min"),
            n("boot_ms_max"),
        );
    }

    println!("\nPerformance overhead");
    println!(
        "  daemon: {} MiB resident, {:.1}% CPU",
        n("daemon_rss_mib"),
        m["daemon_cpu_percent"].as_f64().unwrap_or(0.0),
    );

    println!("\nAdoption (this install)");
    println!("  install id:    {}", m["install_id"].as_str().unwrap_or("?"));
    println!("  active days:   {}", n("active_days"));
    println!("  sandboxes run: {started}");
    if let Some(outcomes) = m["outcomes"].as_object() {
        if !outcomes.is_empty() {
            let parts: Vec<String> = outcomes
                .iter()
                .map(|(k, v)| format!("{k}={}", v.as_u64().unwrap_or(0)))
                .collect();
            println!("  outcomes:      {}", parts.join(" "));
        }
    }

    println!("\nContainment (what 'zero-escape rate' can honestly report)");
    println!("  egress refused by policy:   {}", n("egress_denied"));
    println!("  …aimed at local/LAN space:  {}", n("egress_denied_local"));
    println!(
        "  sandbox escapes detected:   0 — but note this is not a measurement.\n\
         \x20 An escape is a hypervisor compromise, which the host cannot observe\n\
         \x20 from outside. The counts above are what is genuinely evidenced: the\n\
         \x20 boundary being tested and holding."
    );
    Ok(0)
}

/// Panic kill: the newest live sandbox, wiped. Shares `kill_newest_live` with
/// the GUI's global hotkey, so exercising this exercises that path too.
pub async fn kill_newest() -> Result<i32, String> {
    match client().kill_newest_live().await.map_err(|e| e.to_string())? {
        Some(id) => {
            println!("killed {id}");
            Ok(0)
        }
        None => {
            eprintln!("no running sandbox to kill");
            Ok(1)
        }
    }
}

pub async fn pause(id: &str) -> Result<i32, String> {
    client().pause(&parse_id(id)?).await.map_err(|e| e.to_string())?;
    println!("paused");
    Ok(0)
}

pub async fn resume(id: &str) -> Result<i32, String> {
    client().resume(&parse_id(id)?).await.map_err(|e| e.to_string())?;
    println!("resumed");
    Ok(0)
}

pub async fn snapshot(id: &str) -> Result<i32, String> {
    let dir = client()
        .snapshot(&parse_id(id)?)
        .await
        .map_err(|e| e.to_string())?;
    println!("snapshotted; state in {dir}");
    Ok(0)
}

/// Stream the daemon's event bus as JSON lines.
pub async fn events() -> Result<i32, String> {
    let mut sub = client().events().await.map_err(|e| e.to_string())?;
    while let Some(event) = sub.next().await.map_err(|e| e.to_string())? {
        println!("{}", serde_json::to_string(&event).unwrap_or_default());
    }
    Ok(0)
}
