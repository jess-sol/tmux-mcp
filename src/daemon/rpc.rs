/// RPC method dispatch and handlers.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::pane::registry::PaneRegistry;
use crate::proc;
use crate::tmux::connection::TmuxCommands;

/// RPC error with JSON-RPC error code.
#[derive(Debug)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
}

impl RpcError {
    pub fn method_not_found(method: &str) -> Self {
        Self { code: -32601, message: format!("Unknown method: {}", method) }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self { code: -32602, message: msg.into() }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self { code: -32603, message: msg.into() }
    }

    pub fn to_json(&self) -> Value {
        json!({ "code": self.code, "message": self.message })
    }
}

/// Shared daemon state accessible to RPC handlers.
pub struct DaemonState {
    pub conn: Mutex<TmuxCommands>,
    pub registry: Mutex<PaneRegistry>,
}

/// Dispatch an RPC method call.
pub async fn dispatch(
    method: &str,
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    match method {
        "list_panes" => handle_list_panes(params, state).await,
        "command_history" => handle_command_history(params, state).await,
        "command_read" => handle_command_read(params, state).await,
        "command_run" => handle_command_run(params, state).await,
        _ => Err(RpcError::method_not_found(method)),
    }
}

// --- Window scoping ---

/// Resolve the caller's window from their origin_pane.
/// Returns the window_id, or an error if the pane is not tracked.
fn resolve_caller_window(registry: &PaneRegistry, params: &Value) -> Result<String, RpcError> {
    let origin_pane = params["origin_pane"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("origin_pane is required"))?;

    registry
        .window_for_pane(origin_pane)
        .map(|w| w.to_string())
        .ok_or_else(|| RpcError::invalid_params(format!(
            "Origin pane {} not tracked by daemon", origin_pane
        )))
}

/// Validate that a target pane belongs to the caller's window.
fn validate_pane_access(
    registry: &PaneRegistry,
    pane_id: &str,
    caller_window: &str,
) -> Result<(), RpcError> {
    let pane_window = registry
        .window_for_pane(pane_id)
        .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?;

    if pane_window != caller_window {
        return Err(RpcError::invalid_params(format!(
            "Pane {} is in window {}, not your window {}",
            pane_id, pane_window, caller_window
        )));
    }
    Ok(())
}

// --- Handlers ---

#[derive(Serialize)]
struct PaneEntry {
    pane_id: String,
    pid: Option<u32>,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    cwd: Option<String>,
    foreground: Option<String>,
    activity: String,
}

async fn handle_list_panes(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let registry = state.registry.lock().await;
    let caller_window = resolve_caller_window(&registry, params)?;

    let mut entries = Vec::new();
    for (_, tp) in registry.iter() {
        if tp.window_id != caller_window {
            continue;
        }

        let (cwd, foreground) = if let Some(pid) = tp.pid {
            let info = proc::proc_info(pid);
            (
                info.as_ref().and_then(|i| i.cwd.as_ref().map(|p| p.display().to_string())),
                info.as_ref().and_then(|i| i.foreground.as_ref().map(|f| f.comm.clone())),
            )
        } else {
            (None, None)
        };

        entries.push(PaneEntry {
            pane_id: tp.pane_id.clone(),
            pid: tp.pid,
            width: tp.processor.columns(),
            height: tp.processor.screen_lines(),
            x: tp.x,
            y: tp.y,
            cwd,
            foreground,
            activity: format!("{:?}", tp.processor.state().activity),
        });
    }

    entries.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    serde_json::to_value(&entries).map_err(|e| RpcError::internal(e.to_string()))
}

async fn handle_command_history(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let count = params["count"].as_u64().unwrap_or(10) as usize;

    let registry = state.registry.lock().await;
    let caller_window = resolve_caller_window(&registry, params)?;
    validate_pane_access(&registry, pane_id, &caller_window)?;

    let tp = registry.get(pane_id).unwrap();
    let cmds = tp.processor.state().recent_commands(count);
    let entries: Vec<Value> = cmds
        .iter()
        .map(|cmd| {
            json!({
                "command": cmd.command,
                "exit_code": cmd.exit_code,
                "output_lines": cmd.output.lines().count(),
            })
        })
        .collect();

    Ok(json!(entries))
}

async fn handle_command_read(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let count = params["count"].as_u64().unwrap_or(1) as usize;

    let registry = state.registry.lock().await;
    let caller_window = resolve_caller_window(&registry, params)?;
    validate_pane_access(&registry, pane_id, &caller_window)?;

    let tp = registry.get(pane_id).unwrap();
    let cmds = tp.processor.state().recent_commands(count);
    let entries: Vec<Value> = cmds
        .iter()
        .map(|cmd| {
            json!({
                "command": cmd.command,
                "exit_code": cmd.exit_code,
                "output": cmd.output,
            })
        })
        .collect();

    Ok(json!(entries))
}

async fn handle_command_run(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let command = params["command"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("command is required"))?;
    let timeout_secs = params["timeout_secs"].as_u64().unwrap_or(30);

    // Validate window access
    {
        let registry = state.registry.lock().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
    }

    // Snapshot completion_seq before sending — D marker always bumps this,
    // even if the command isn't recorded in history (e.g., no C marker).
    let seq_before = {
        let registry = state.registry.lock().await;
        registry.get(pane_id).map(|tp| tp.processor.state().completion_seq).unwrap_or(0)
    };

    // Send the command
    {
        let mut conn = state.conn.lock().await;
        conn.send_command(pane_id, command)
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send command: {}", e)))?;
    }

    // Poll for completion (new command appearing in history)
    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        {
            let registry = state.registry.lock().await;
            if let Some(tp) = registry.get(pane_id) {
                let pane_state = tp.processor.state();
                if pane_state.completion_seq > seq_before {
                    // Command completed. Return newest command if available,
                    // or a minimal result with just the exit code.
                    if let Some(cmd) = pane_state.commands.front() {
                        if cmd.seq > 0 {
                            return Ok(json!({
                                "command": cmd.command,
                                "exit_code": cmd.exit_code,
                                "output": cmd.output,
                            }));
                        }
                    }
                    // No command recorded (e.g., no C marker) — return exit code from D
                    return Ok(json!({
                        "command": null,
                        "exit_code": pane_state.last_exit_code,
                        "output": "",
                    }));
                }
            }
        }

        if tokio::time::Instant::now() >= deadline {
            return Err(RpcError::internal(format!(
                "Command timed out after {}s",
                timeout_secs
            )));
        }
    }
}
