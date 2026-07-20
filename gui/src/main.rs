//! Agent OS desktop app (Tauri shell).
//!
//! Thin client over the daemon's Unix socket, exactly like the CLI: every
//! privileged operation (boot, policy, kill) stays in `agentosd`. The GUI's
//! job is the PRD's dashboard: sandbox list, permission editor, live
//! terminal, network monitor, and the big red Terminate button.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use agentos_core::{
    AutoKillRules, MountSpec, NetPolicy, ResourceLimits, SandboxSpec,
};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter};

#[tauri::command]
async fn list_sandboxes() -> Result<Value, String> {
    agentos_client::unary("sandbox.list", json!(null)).await
}

#[tauri::command]
async fn kill_sandbox(id: String, save: bool) -> Result<Value, String> {
    agentos_client::unary("sandbox.kill", json!({ "id": id, "save": save })).await
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

    let mut stream = agentos_client::open_stream(
        "sandbox.run",
        serde_json::to_value(&spec).map_err(|e| e.to_string())?,
    )
    .await?;

    tauri::async_runtime::spawn(async move {
        loop {
            match stream.next().await {
                Ok(Some(v)) => {
                    app.emit("run-line", v).ok();
                }
                Ok(None) => break,
                Err(e) => {
                    app.emit("run-line", json!({ "event": "error", "message": e })).ok();
                    break;
                }
            }
        }
        app.emit("run-line", json!({ "event": "stream_closed" })).ok();
    });
    Ok(())
}

/// Panic kill switch: terminate the most-recently-started running sandbox.
/// Bound to a global hotkey so it works even when the window isn't focused.
async fn kill_most_recent_running() -> Result<Value, String> {
    let list = agentos_client::unary("sandbox.list", json!(null)).await?;
    let running: Option<String> = list
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .rev()
                .find(|r| r["state"]["state"] == "running")
                .and_then(|r| r["id"].as_str().map(String::from))
        });
    match running {
        Some(id) => agentos_client::unary("sandbox.kill", json!({ "id": id, "save": false })).await,
        None => Ok(json!({ "killed": false, "reason": "no running sandbox" })),
    }
}

/// Forward the daemon's event bus to the frontend forever, reconnecting if
/// the daemon restarts.
async fn pump_events(app: AppHandle) {
    loop {
        if let Ok(mut stream) = agentos_client::open_stream("events.subscribe", json!(null)).await {
            while let Ok(Some(v)) = stream.next().await {
                app.emit("agentos-event", v).ok();
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
