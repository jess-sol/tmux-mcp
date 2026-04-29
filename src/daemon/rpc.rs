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
    pub started_at: std::time::Instant,
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
        "capture_pane" => handle_capture_pane(params, state).await,
        "inject_osc133" => handle_inject_osc133(params, state).await,
        "send_keys" => handle_send_keys(params, state).await,
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

// --- Helpers ---

/// Get the leaf PID for a pane: the foreground process PID, or the shell PID if idle.
fn get_leaf_pid(shell_pid: u32) -> u32 {
    proc::proc_info(shell_pid)
        .and_then(|info| info.foreground.map(|f| f.pid))
        .unwrap_or(shell_pid)
}

/// Capture the last N lines of visible screen text from a pane's processor.
fn capture_screen(registry: &PaneRegistry, pane_id: &str, lines: usize) -> String {
    let Some(tp) = registry.get(pane_id) else { return String::new() };
    let all_lines = tp.processor.screen_text();
    let total = all_lines.len();
    let start = total.saturating_sub(lines);
    let selected: Vec<&str> = all_lines[start..].iter().map(|s| s.as_str()).collect();
    let end = selected.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(0);
    selected[..end].join("\n")
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
    /// CWD from /proc (local process).
    process_cwd: Option<String>,
    /// CWD from OSC 7 (works across SSH).
    osc_cwd: Option<String>,
    /// Hostname from OSC 7 (null for localhost or if never received).
    osc_hostname: Option<String>,
    foreground: Option<String>,
    activity: String,
    /// Seconds since last OSC 133 marker, or null if never seen.
    osc133_last_marker_secs: Option<f64>,
    /// Seconds since last terminal data received, or null if never seen.
    last_data_secs: Option<f64>,
    /// OSC 133 status for current leaf PID: "confirmed", "failed", or "unknown".
    osc133_status: String,
}

async fn handle_list_panes(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let registry = state.registry.lock().await;
    let caller_window = resolve_caller_window(&registry, params)?;

    // Get local hostname to filter it from osc_hostname
    let local_hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default();

    let mut entries = Vec::new();
    for (_, tp) in registry.iter() {
        if tp.window_id != caller_window {
            continue;
        }

        let (process_cwd, foreground) = if let Some(pid) = tp.pid {
            let info = proc::proc_info(pid);
            (
                info.as_ref().and_then(|i| i.cwd.as_ref().map(|p| p.display().to_string())),
                info.as_ref().and_then(|i| i.foreground.as_ref().map(|f| f.comm.clone())),
            )
        } else {
            (None, None)
        };

        let pane_state = tp.processor.state();
        let osc133_last_marker_secs = pane_state
            .last_osc133_marker
            .map(|t| t.elapsed().as_secs_f64());
        let last_data_secs = pane_state
            .last_data
            .map(|t| t.elapsed().as_secs_f64());

        let leaf_pid = tp.pid.map(get_leaf_pid).unwrap_or(0);
        let osc133_status = match pane_state.osc133_lookup(leaf_pid) {
            Some(true) => "confirmed",
            Some(false) => "failed",
            None => "unknown",
        };

        entries.push(PaneEntry {
            pane_id: tp.pane_id.clone(),
            pid: tp.pid,
            width: tp.processor.columns(),
            height: tp.processor.screen_lines(),
            x: tp.x,
            y: tp.y,
            process_cwd,
            osc_cwd: pane_state.cwd.clone(),
            osc_hostname: pane_state.hostname.as_ref().and_then(|h| {
                if h == &local_hostname { None } else { Some(h.clone()) }
            }),
            foreground,
            activity: format!("{:?}", pane_state.activity),
            osc133_last_marker_secs,
            last_data_secs,
            osc133_status: osc133_status.to_string(),
        });
    }

    entries.sort_by(|a, b| a.pane_id.cmp(&b.pane_id));
    Ok(json!({
        "daemon_uptime_secs": state.started_at.elapsed().as_secs_f64(),
        "panes": serde_json::to_value(&entries).map_err(|e| RpcError::internal(e.to_string()))?,
    }))
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

    // Lint the command for anti-patterns
    if let Err(err) = crate::lint::lint_command_run(command) {
        return Err(RpcError::invalid_params(err.to_string()));
    }

    // Validate window access and get shell PID
    let shell_pid = {
        let registry = state.registry.lock().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry
            .get(pane_id)
            .and_then(|tp| tp.pid)
            .ok_or_else(|| RpcError::internal("Pane has no known PID"))?
    };

    let leaf_pid = get_leaf_pid(shell_pid);

    // --- OSC 133 gating ---
    let cache_status = {
        let registry = state.registry.lock().await;
        registry.get(pane_id).and_then(|tp| tp.processor.state().osc133_lookup(leaf_pid))
    };

    match cache_status {
        Some(false) => {
            // Known failed — instant reject
            let registry = state.registry.lock().await;
            let screen = capture_screen(&registry, pane_id, 20);
            let marker_secs = registry
                .get(pane_id)
                .and_then(|tp| tp.processor.state().last_osc133_marker)
                .map(|t| t.elapsed().as_secs_f64());
            return Err(RpcError::internal(format!(
                "OSC 133 not active for this pane. Use inject_osc133 to enable shell integration.\n\nosc133_last_marker_secs: {:?}\n\nScreen:\n{}",
                marker_secs, screen
            )));
        }
        None => {
            // Unknown — probe with ` :`
            let seq_before = {
                let registry = state.registry.lock().await;
                registry.get(pane_id).map(|tp| tp.processor.state().completion_seq).unwrap_or(0)
            };

            {
                let mut conn = state.conn.lock().await;
                conn.send_command(pane_id, " :")
                    .await
                    .map_err(|e| RpcError::internal(format!("Failed to probe: {}", e)))?;
            }

            // Wait up to 500ms for completion_seq bump
            let probe_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
            let mut probed_ok = false;
            loop {
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                {
                    let registry = state.registry.lock().await;
                    if let Some(tp) = registry.get(pane_id) {
                        if tp.processor.state().completion_seq > seq_before {
                            probed_ok = true;
                            break;
                        }
                    }
                }
                if tokio::time::Instant::now() >= probe_deadline {
                    break;
                }
            }

            {
                let mut registry = state.registry.lock().await;
                if probed_ok {
                    if let Some(tp) = registry.get_mut(pane_id) {
                        tp.processor.state_mut().osc133_confirm(leaf_pid);
                    }
                } else {
                    if let Some(tp) = registry.get_mut(pane_id) {
                        tp.processor.state_mut().osc133_fail(leaf_pid);
                    }
                    let screen = capture_screen(&registry, pane_id, 20);
                    return Err(RpcError::internal(format!(
                        "OSC 133 probe failed — no markers detected. Use inject_osc133 to enable shell integration.\n\nScreen:\n{}",
                        screen
                    )));
                }
            }
        }
        Some(true) => {
            // Confirmed — proceed (will verify C marker after send below)
        }
    }

    // --- Send the command ---
    let seq_before = {
        let registry = state.registry.lock().await;
        registry.get(pane_id).map(|tp| tp.processor.state().completion_seq).unwrap_or(0)
    };
    let marker_before = {
        let registry = state.registry.lock().await;
        registry.get(pane_id).and_then(|tp| tp.processor.state().last_osc133_marker)
    };

    {
        let mut conn = state.conn.lock().await;
        conn.send_command(pane_id, command)
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send command: {}", e)))?;
    }

    // Wait up to 500ms for any marker (C or completion_seq bump) to verify OSC 133 is alive
    let verify_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
    let mut marker_seen = false;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        {
            let registry = state.registry.lock().await;
            if let Some(tp) = registry.get(pane_id) {
                let ps = tp.processor.state();
                if ps.completion_seq > seq_before {
                    marker_seen = true;
                    break;
                }
                if ps.last_osc133_marker != marker_before {
                    marker_seen = true;
                    break;
                }
            }
        }
        if tokio::time::Instant::now() >= verify_deadline {
            break;
        }
    }

    if !marker_seen {
        // OSC 133 stopped working — command was already sent, pane state uncertain
        tracing::warn!("OSC 133 markers not seen after sending command to pane {}", pane_id);
        let mut registry = state.registry.lock().await;
        if let Some(tp) = registry.get_mut(pane_id) {
            tp.processor.state_mut().osc133_fail(leaf_pid);
        }
        let screen = capture_screen(&registry, pane_id, 20);
        return Err(RpcError::internal(format!(
            "Command was sent but no OSC 133 markers detected. Shell integration may have stopped working. \
             Use capture_pane to inspect pane state.\n\nScreen:\n{}",
            screen
        )));
    }

    // --- Poll for completion ---
    let deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        {
            let registry = state.registry.lock().await;
            if let Some(tp) = registry.get(pane_id) {
                let pane_state = tp.processor.state();
                if pane_state.completion_seq > seq_before {
                    if let Some(cmd) = pane_state.commands.front() {
                        if cmd.id > 0 {
                            return Ok(json!({
                                "command": cmd.command,
                                "exit_code": cmd.exit_code,
                                "output": cmd.output,
                            }));
                        }
                    }
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

async fn handle_capture_pane(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let lines = params["lines"].as_u64().unwrap_or(50).min(1000) as usize;

    // Validate window access
    {
        let registry = state.registry.lock().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
    }

    // Read from alacritty screen model
    let registry = state.registry.lock().await;
    let tp = registry
        .get(pane_id)
        .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?;

    let all_lines = tp.processor.screen_text();
    // Take the last N lines, trim trailing empty lines
    let total = all_lines.len();
    let start = total.saturating_sub(lines);
    let selected: Vec<&str> = all_lines[start..]
        .iter()
        .map(|s| s.as_str())
        .collect();

    // Trim trailing empty lines
    let end = selected
        .iter()
        .rposition(|l| !l.is_empty())
        .map(|i| i + 1)
        .unwrap_or(0);

    let text = selected[..end].join("\n");
    Ok(json!({ "text": text }))
}

/// OSC 133 + OSC 7 injection script for bash.
/// Each line has leading space for history suppression.
const OSC133_INJECT_LINES: &[&str] = &[
    " __osc133_exec_ready=true; __osc7_cwd() { printf '\\e]7;file://%s%s\\e\\\\' \"$(hostname)\" \"$PWD\"; }; __osc133_precmd() { local ret=$?; __osc133_exec_ready=true; printf '\\e]133;D;%d\\e\\\\\\e]133;A\\e\\\\' \"$ret\"; __osc7_cwd; return \"$ret\"; }; PROMPT_COMMAND=\"__osc133_precmd;${PROMPT_COMMAND:-}\"",
    " __osc133_debug() { if $__osc133_exec_ready; then __osc133_exec_ready=false; local cmd; cmd=$(HISTTIMEFORMAT= builtin history 1 | sed 's/^ *[0-9]* *//'); [[ -n \"$cmd\" ]] && printf '\\e]133;E;%s\\e\\\\' \"$cmd\"; fi; }; trap '__osc133_debug \"$_\"' DEBUG",
    " PS1=\"${PS1}\"$'\\001\\e]133;B\\e\\\\\\002'; PS0+='\\[\\e]133;C\\e\\\\\\]'",
];

async fn handle_inject_osc133(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;

    // Validate window access and get shell PID
    let shell_pid = {
        let registry = state.registry.lock().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry
            .get(pane_id)
            .and_then(|tp| tp.pid)
            .ok_or_else(|| RpcError::internal("Pane has no known PID"))?
    };

    let leaf_pid = get_leaf_pid(shell_pid);

    // --- Probe first: maybe markers already work ---
    let seq_before = {
        let registry = state.registry.lock().await;
        registry.get(pane_id).map(|tp| tp.processor.state().completion_seq).unwrap_or(0)
    };

    {
        let mut conn = state.conn.lock().await;
        conn.send_command(pane_id, " :")
            .await
            .map_err(|e| RpcError::internal(format!("Failed to probe: {}", e)))?;
    }

    let probe_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
    let mut already_active = false;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        {
            let registry = state.registry.lock().await;
            if let Some(tp) = registry.get(pane_id) {
                if tp.processor.state().completion_seq > seq_before {
                    already_active = true;
                    break;
                }
            }
        }
        if tokio::time::Instant::now() >= probe_deadline {
            break;
        }
    }

    if already_active {
        let mut registry = state.registry.lock().await;
        if let Some(tp) = registry.get_mut(pane_id) {
            tp.processor.state_mut().osc133_confirm(leaf_pid);
        }
        return Ok(json!({ "status": "already_active" }));
    }

    // --- Inject the script ---
    {
        let mut conn = state.conn.lock().await;
        for line in OSC133_INJECT_LINES {
            conn.send_command(pane_id, line)
                .await
                .map_err(|e| RpcError::internal(format!("Failed to send injection: {}", e)))?;
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    }

    // --- Post-inject probe ---
    let seq_after_inject = {
        let registry = state.registry.lock().await;
        registry.get(pane_id).map(|tp| tp.processor.state().completion_seq).unwrap_or(0)
    };

    {
        let mut conn = state.conn.lock().await;
        conn.send_command(pane_id, " :")
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send probe: {}", e)))?;
    }

    let post_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
    let mut inject_ok = false;
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        {
            let registry = state.registry.lock().await;
            if let Some(tp) = registry.get(pane_id) {
                if tp.processor.state().completion_seq > seq_after_inject {
                    inject_ok = true;
                    break;
                }
            }
        }
        if tokio::time::Instant::now() >= post_deadline {
            break;
        }
    }

    let mut registry = state.registry.lock().await;
    if inject_ok {
        if let Some(tp) = registry.get_mut(pane_id) {
            tp.processor.state_mut().osc133_confirm(leaf_pid);
        }
        Ok(json!({ "status": "active" }))
    } else {
        if let Some(tp) = registry.get_mut(pane_id) {
            tp.processor.state_mut().osc133_fail(leaf_pid);
        }
        let screen = capture_screen(&registry, pane_id, 20);
        Ok(json!({
            "status": "failed",
            "message": "Injection sent but no markers detected. Shell may not be bash.",
            "screen": screen,
        }))
    }
}

async fn handle_send_keys(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let keys = params["keys"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("keys is required"))?;

    if keys.len() > 64 {
        return Err(RpcError::invalid_params(
            format!("keys too long: {} chars (max 64)", keys.len()),
        ));
    }

    // Validate window access
    {
        let registry = state.registry.lock().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
    }

    // Send keys
    {
        let mut conn = state.conn.lock().await;
        conn.send_raw_keys(pane_id, keys)
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send keys: {}", e)))?;
    }

    // Brief pause for terminal to process, then return screen
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let registry = state.registry.lock().await;
    let screen = capture_screen(&registry, pane_id, 20);
    Ok(json!({ "screen": screen }))
}
