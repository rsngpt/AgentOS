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

#[allow(clippy::too_many_arguments)]
#[tauri::command]
async fn run_sandbox(
    app: AppHandle,
    command: String,
    mounts: Vec<String>,
    net: String,
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
    let net = NetPolicy::parse(&net).map_err(|e| e.to_string())?;
    let spec = SandboxSpec {
        name: argv[0].rsplit('/').next().unwrap_or("agent").to_string(),
        command: argv,
        env: vec![],
        mounts,
        repo: None,
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
    let c = client();
    let running = c
        .list()
        .await
        .map_err(|e| e.to_string())?
        .into_iter()
        .rev()
        .find(|sb| matches!(sb.state, agentos_core::SandboxState::Running));
    match running {
        Some(sb) => c
            .kill(&sb.id, false)
            .await
            .map(|_| json!({ "killed": true, "id": sb.id }))
            .map_err(|e| e.to_string()),
        None => Ok(json!({ "killed": false, "reason": "no running sandbox" })),
    }
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
            run_sandbox
        ])
        .setup(move |app| {
            use tauri_plugin_global_shortcut::GlobalShortcutExt;
            // Registration can fail (e.g. macOS accessibility not granted);
            // the in-window Terminate buttons still work regardless.
            if let Err(e) = app.global_shortcut().register(panic_key) {
                eprintln!("global kill hotkey unavailable: {e}");
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(pump_events(handle));
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running Agent OS GUI");
}
