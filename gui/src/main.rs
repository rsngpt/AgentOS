//! Agent OS desktop app (Tauri shell).
//!
//! Thin client over the daemon's Unix socket, exactly like the CLI: every
//! privileged operation (boot, policy, kill) stays in `agentosd`. The GUI's
//! job is the PRD's dashboard: sandbox list, permission editor, live
//! terminal, network monitor, and the big red Terminate button.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use agentos_client::RunEvent;
use agentos_core::{
    AutoKillRules, MountSpec, NetPolicy, ResourceLimits, SandboxSpec,
};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

/// The GUI drives Agent OS through the same public SDK an embedder would use.
fn client() -> agentos_client::Client {
    agentos_client::Client::new()
}

fn parse_id(id: &str) -> Result<agentos_core::SandboxId, String> {
    serde_json::from_value(Value::String(id.to_string()))
        .map_err(|_| format!("not a valid sandbox id: {id}"))
}

#[tauri::command]
async fn list_sandboxes() -> Result<Value, String> {
    let rows = client().list().await.map_err(|e| e.to_string())?;
    let rows: Vec<Value> = rows
        .into_iter()
        .map(|sb| json!({ "id": sb.id, "name": sb.name, "state": sb.state }))
        .collect();
    Ok(Value::Array(rows))
}

#[tauri::command]
async fn kill_sandbox(id: String, save: bool) -> Result<Value, String> {
    client()
        .kill(&parse_id(&id)?, save)
        .await
        .map(|_| json!({ "killed": true }))
        .map_err(|e| e.to_string())
}

/// Lifecycle controls, so the GUI can do what the CLI can.
#[tauri::command]
async fn pause_sandbox(id: String) -> Result<Value, String> {
    client()
        .pause(&parse_id(&id)?)
        .await
        .map(|_| json!({ "paused": true }))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn resume_sandbox(id: String) -> Result<Value, String> {
    client()
        .resume(&parse_id(&id)?)
        .await
        .map(|_| json!({ "resumed": true }))
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn snapshot_sandbox(id: String) -> Result<Value, String> {
    client()
        .snapshot(&parse_id(&id)?)
        .await
        .map(|dir| json!({ "dir": dir }))
        .map_err(|e| e.to_string())
}

/// Restore streams like a run, so it reuses the same `run-line` channel.
#[tauri::command]
async fn restore_sandbox(app: AppHandle, id: String) -> Result<(), String> {
    let mut stream = client()
        .restore(&parse_id(&id)?)
        .await
        .map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn(async move {
        while let Ok(Some(event)) = stream.next().await {
            app.emit("run-line", run_event_json(event)).ok();
        }
        app.emit("run-line", json!({ "event": "stream_closed" })).ok();
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn run_sandbox(
    app: AppHandle,
    command: String,
    mounts: Vec<String>,
    net: String,
    template: Option<String>,
    repo: Option<String>,
    env: Vec<String>,
    vcpus: u8,
    mem_mib: u32,
    kill_over_mem: Option<u32>,
    kill_over_egress: Option<u32>,
    kill_after_secs: Option<u64>,
) -> Result<(), String> {
    let argv: Vec<String> = command.split_whitespace().map(String::from).collect();
    if argv.is_empty() {
        return Err("command is empty".into());
    }
    let mounts = mounts
        .iter()
        .filter(|m| !m.trim().is_empty())
        .map(|m| MountSpec::parse(m.trim()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    // A template presets the allowlist; an explicit non-default --net wins,
    // matching the CLI's precedence exactly.
    let net = match template.as_deref().filter(|t| !t.is_empty()) {
        Some(t) if net == "offline" => {
            agentos_core::template_net(t).map_err(|e| e.to_string())?
        }
        Some(t) => {
            agentos_core::template_net(t).map_err(|e| e.to_string())?;
            NetPolicy::parse(&net).map_err(|e| e.to_string())?
        }
        None => NetPolicy::parse(&net).map_err(|e| e.to_string())?,
    };
    let env = env
        .iter()
        .filter(|kv| !kv.trim().is_empty())
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.trim().to_string(), v.to_string()))
                .ok_or_else(|| format!("env expects KEY=VALUE, got {kv:?}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let repo = repo
        .filter(|r| !r.trim().is_empty())
        .map(|url| agentos_core::RepoSpec {
            url: url.trim().to_string(),
            git_ref: None,
        });

    let spec = SandboxSpec {
        name: argv[0].rsplit('/').next().unwrap_or("agent").to_string(),
        command: argv,
        env,
        mounts,
        repo,
        template: template.clone().filter(|t| !t.is_empty()),
        net,
        limits: ResourceLimits {
            vcpus,
            mem_mib,
            ..Default::default()
        },
        auto_kill: AutoKillRules {
            max_mem_mib: kill_over_mem,
            max_egress_mib: kill_over_egress,
            max_runtime_secs: kill_after_secs,
        },
    };

    let mut stream = client().run(&spec).await.map_err(|e| e.to_string())?;

    tauri::async_runtime::spawn(async move {
        loop {
            match stream.next().await {
                Ok(Some(event)) => {
                    app.emit("run-line", run_event_json(event)).ok();
                }
                Ok(None) => break,
                Err(e) => {
                    app.emit("run-line", json!({ "event": "error", "message": e.to_string() }))
                        .ok();
                    break;
                }
            }
        }
        app.emit("run-line", json!({ "event": "stream_closed" })).ok();
    });
    Ok(())
}

/// Render an SDK event in the shape the static frontend already consumes.
fn run_event_json(event: RunEvent) -> Value {
    match event {
        RunEvent::Created(id) => json!({ "event": "created", "id": id }),
        RunEvent::Restoring(id) => json!({ "event": "restoring", "id": id }),
        RunEvent::Cloning { url } => json!({ "event": "cloning", "url": url }),
        RunEvent::Running => json!({ "event": "running" }),
        RunEvent::Stdout(data) => json!({ "event": "stdout", "data": data }),
        RunEvent::Stderr(data) => json!({ "event": "stderr", "data": data }),
        RunEvent::Exited { code, signal } => {
            json!({ "event": "exited", "code": code, "signal": signal })
        }
        RunEvent::Terminated { reason, saved_dir } => {
            json!({ "event": "terminated", "reason": reason, "saved_dir": saved_dir })
        }
        RunEvent::Snapshotted { dir } => json!({ "event": "snapshotted", "dir": dir }),
        RunEvent::Unknown(v) => v,
    }
}

/// Panic kill switch: terminate the most-recently-started running sandbox.
/// Bound to a global hotkey so it works even when the window isn't focused.
async fn kill_most_recent_running() -> Result<Value, String> {
    match client().kill_newest_live().await.map_err(|e| e.to_string())? {
        Some(id) => Ok(json!({ "killed": true, "id": id })),
        None => Ok(json!({ "killed": false, "reason": "no running sandbox" })),
    }
}

/// The same action the global hotkey fires, callable from the window. Gives
/// the panic button a path that works even when the OS won't deliver the
/// hotkey (unsigned build, accessibility permission not granted).
#[tauri::command]
async fn panic_kill() -> Result<Value, String> {
    kill_most_recent_running().await
}

/// Forward the daemon's event bus to the frontend forever, reconnecting if
/// the daemon restarts.
async fn pump_events(app: AppHandle) {
    loop {
        if let Ok(mut sub) = client().events().await {
            while let Ok(Some(event)) = sub.next().await {
                app.emit("agentos-event", event).ok();
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

fn main() {
    use tauri_plugin_global_shortcut::{Code, Modifiers, Shortcut, ShortcutState};

    // Global panic hotkey: Cmd/Ctrl+Shift+K terminates the newest running VM.
    let panic_key = Shortcut::new(Some(Modifiers::SHIFT | Modifiers::SUPER), Code::KeyK);

    tauri::Builder::default()
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(move |app, shortcut, event| {
                    if shortcut == &panic_key && event.state() == ShortcutState::Pressed {
                        let app = app.clone();
                        tauri::async_runtime::spawn(async move {
                            let result = kill_most_recent_running().await;
                            app.emit("panic-kill", json!({ "result": format!("{result:?}") }))
                                .ok();
                        });
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            list_sandboxes,
            kill_sandbox,
            panic_kill,
            pause_sandbox,
            resume_sandbox,
            snapshot_sandbox,
            restore_sandbox,
            run_sandbox
        ])
        .setup(move |app| {
            use tauri_plugin_global_shortcut::GlobalShortcutExt;
            // Registration can fail (macOS accessibility not granted, or the
            // combo already taken). Tell the window rather than only stderr:
            // a kill switch the user believes in but that never fires is worse
            // than one they know is unavailable.
            let status = match app.global_shortcut().register(panic_key) {
                Ok(()) => json!({ "available": true }),
                Err(e) => {
                    eprintln!("global kill hotkey unavailable: {e}");
                    json!({ "available": false, "error": e.to_string() })
                }
            };
            app.handle().emit("hotkey-status", status).ok();

            let handle = app.handle().clone();
            tauri::async_runtime::spawn(pump_events(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Agent OS GUI");
}
