/// RPC method dispatch and handlers.

use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};

use crate::pane::osc133::Osc133Phase;
use crate::pane::registry::{PaneHandle, PaneRegistry};
use crate::pane::state::Activity;
use crate::policy;
use crate::policy::approval::ApprovalStore;
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
    pub registry: RwLock<PaneRegistry>,
    pub approvals: Mutex<ApprovalStore>,
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
        "request_approval" => handle_request_approval(params, state).await,
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

/// Read parameters shared between command_run and command_read.
struct ReadParams {
    next: Option<usize>,
    head: Option<usize>,
    tail: Option<usize>,
    search: Option<regex::Regex>,
}

impl ReadParams {
    fn from_json(params: &Value) -> Result<Self, RpcError> {
        let next = params["next"].as_u64().map(|n| n as usize);
        let head = params["head"].as_u64().map(|n| n as usize);
        let tail = params["tail"].as_u64().map(|n| n as usize);

        let exclusive_count = next.is_some() as u8 + head.is_some() as u8 + tail.is_some() as u8;
        if exclusive_count > 1 {
            return Err(RpcError::invalid_params(
                "next, head, and tail are mutually exclusive — use only one",
            ));
        }

        let search = if let Some(pattern) = params["search"].as_str() {
            Some(
                regex::Regex::new(pattern)
                    .map_err(|e| RpcError::invalid_params(format!("invalid search regex: {}", e)))?,
            )
        } else {
            None
        };

        Ok(Self { next, head, tail, search })
    }
}

/// Result of applying read params to command output.
struct ReadResult {
    lines: Vec<String>,
    total_lines: usize,
    skipped: usize,
    matched: Option<usize>,  // number of search matches (if search was used)
}

/// Apply read params (next/head/tail/search) to a command record.
/// Returns the selected lines and updates the cursor if `next` was used.
fn apply_read_params(
    output: &str,
    cursor: &mut usize,
    params: &ReadParams,
) -> ReadResult {
    let all_lines: Vec<&str> = if output.is_empty() {
        Vec::new()
    } else {
        output.lines().collect()
    };
    let total_lines = all_lines.len();

    // Select range
    let (selected, skipped) = if let Some(n) = params.next {
        let start = *cursor;
        let end = total_lines.min(start + n);
        let lines = all_lines.get(start..end).unwrap_or(&[]).to_vec();
        let skipped = start;
        *cursor = end; // advance cursor
        (lines, skipped)
    } else if let Some(n) = params.head {
        let end = total_lines.min(n);
        (all_lines[..end].to_vec(), 0)
    } else if let Some(n) = params.tail {
        let start = total_lines.saturating_sub(n);
        (all_lines[start..].to_vec(), start)
    } else {
        // All output
        (all_lines, 0)
    };

    // Apply search filter
    if let Some(re) = &params.search {
        let matched_lines: Vec<String> = selected
            .iter()
            .filter(|l| re.is_match(l))
            .map(|l| l.to_string())
            .collect();
        let matched = matched_lines.len();
        ReadResult {
            lines: matched_lines,
            total_lines,
            skipped,
            matched: Some(matched),
        }
    } else {
        ReadResult {
            lines: selected.into_iter().map(|s| s.to_string()).collect(),
            total_lines,
            skipped,
            matched: None,
        }
    }
}

/// Get the leaf PID for a pane: the foreground process PID, or the shell PID if idle.
fn get_leaf_pid(shell_pid: u32) -> u32 {
    proc::proc_info(shell_pid)
        .and_then(|info| info.foreground.map(|f| f.pid))
        .unwrap_or(shell_pid)
}

/// Capture the last N lines of visible screen text from locked pane state.
fn capture_screen(ps: &crate::pane::registry::PaneState, lines: usize) -> String {
    let all_lines = ps.processor.screen_text();
    let total = all_lines.len();
    let start = total.saturating_sub(lines);
    let selected: Vec<&str> = all_lines[start..].iter().map(|s| s.as_str()).collect();
    let end = selected.iter().rposition(|l| !l.is_empty()).map(|i| i + 1).unwrap_or(0);
    selected[..end].join("\n")
}

/// Probe OSC 133 by sending ` :` and watching for a completion_seq bump.
/// Returns true if a bump was observed within the deadline.
async fn probe_osc133(
    pane_id: &str,
    pane_handle: &PaneHandle,
    state: &Arc<DaemonState>,
    deadline_ms: u64,
) -> Result<bool, RpcError> {
    let seq_before = {
        let tp = pane_handle.lock().await;
        tp.processor.state().completion_seq
    };

    {
        let mut conn = state.conn.lock().await;
        conn.send_command(pane_id, " :")
            .await
            .map_err(|e| RpcError::internal(format!("Failed to probe: {}", e)))?;
    }

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(deadline_ms);
    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        {
            let tp = pane_handle.lock().await;
            if tp.processor.state().completion_seq > seq_before {
                return Ok(true);
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(false);
        }
    }
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
    /// Username from OSC 7 userinfo (null if never received).
    osc_user: Option<String>,
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
    let handles = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        let all_handles = registry.snapshot_handles();
        // Filter by window — window_id is on TrackedPane (outside mutex)
        let filtered: Vec<PaneHandle> = all_handles
            .into_iter()
            .filter(|h| h.window_id == caller_window)
            .collect();
        filtered
    };

    // Get local hostname to filter it from osc_hostname
    let local_hostname = hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_default();

    let mut entries = Vec::new();
    for handle in &handles {
        let ps = handle.lock().await;

        let (process_cwd, foreground) = if let Some(pid) = ps.pid {
            let info = proc::proc_info(pid);
            (
                info.as_ref().and_then(|i| i.cwd.as_ref().map(|p| p.display().to_string())),
                info.as_ref().and_then(|i| i.foreground.as_ref().map(|f| f.comm.clone())),
            )
        } else {
            (None, None)
        };

        let term_state = ps.processor.state();
        let osc133_last_marker_secs = term_state
            .last_osc133_marker
            .map(|t: std::time::Instant| t.elapsed().as_secs_f64());
        let last_data_secs = term_state
            .last_data
            .map(|t: std::time::Instant| t.elapsed().as_secs_f64());

        let leaf_pid = ps.pid.map(get_leaf_pid).unwrap_or(0);
        let osc133_status = match term_state.osc133_lookup(leaf_pid) {
            Some(true) => "confirmed",
            Some(false) => "failed",
            None => "unknown",
        };

        entries.push(PaneEntry {
            pane_id: handle.pane_id.clone(),
            pid: ps.pid,
            width: ps.processor.columns(),
            height: ps.processor.screen_lines(),
            x: ps.x,
            y: ps.y,
            process_cwd,
            osc_cwd: term_state.cwd.clone(),
            osc_user: term_state.user.clone(),
            osc_hostname: term_state.hostname.as_ref().and_then(|h: &String| {
                if h == &local_hostname { None } else { Some(h.clone()) }
            }),
            foreground,
            activity: format!("{:?}", term_state.activity),
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

    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry.get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    let tp = pane_handle.lock().await;
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
    let command_id = params["command_id"].as_u64();
    let timeout_secs = params["timeout_secs"].as_u64().unwrap_or(5);
    let read_params = ReadParams::from_json(params)?;

    // Validate window access and get pane handle
    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry.get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    // Find the target command
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);

    loop {
        {
            let mut tp = pane_handle.lock().await;
            let pane_state = tp.processor.state_mut();
            let cmd = if let Some(id) = command_id {
                pane_state.command_by_id_mut(id)
            } else {
                // Default: most recent command
                pane_state.commands.front_mut()
            };

            if let Some(cmd) = cmd {
                // For completed commands or non-next reads, return immediately
                let is_next = read_params.next.is_some();
                if cmd.completed || !is_next {
                    let result = apply_read_params(&cmd.output, &mut cmd.read_cursor, &read_params);
                    return Ok(json!({
                        "command_id": cmd.id,
                        "command": cmd.command,
                        "status": if cmd.completed { "completed" } else { "running" },
                        "exit_code": cmd.exit_code,
                        "output": result.lines.join("\n"),
                        "total_lines": result.total_lines,
                        "lines_skipped": result.skipped,
                        "search_matches": result.matched,
                    }));
                }

                // next on active command — check if there's new output
                let cursor = cmd.read_cursor;
                let line_count = if cmd.output.is_empty() { 0 } else { cmd.output.lines().count() };
                if line_count > cursor {
                    let result = apply_read_params(&cmd.output, &mut cmd.read_cursor, &read_params);
                    if !result.lines.is_empty() {
                        return Ok(json!({
                            "command_id": cmd.id,
                            "command": cmd.command,
                            "status": "running",
                            "output": result.lines.join("\n"),
                            "total_lines": result.total_lines,
                        }));
                    }
                }
            } else if command_id.is_some() {
                return Err(RpcError::invalid_params(format!(
                    "Command {} not found", command_id.unwrap()
                )));
            } else {
                return Err(RpcError::invalid_params("No commands in history"));
            }
        }

        if tokio::time::Instant::now() >= deadline {
            // Timeout — return whatever we have
            let mut tp = pane_handle.lock().await;
            let pane_state = tp.processor.state_mut();
            let cmd = if let Some(id) = command_id {
                pane_state.command_by_id_mut(id)
            } else {
                pane_state.commands.front_mut()
            };

            if let Some(cmd) = cmd {
                let result = apply_read_params(&cmd.output, &mut cmd.read_cursor, &read_params);
                return Ok(json!({
                    "command_id": cmd.id,
                    "command": cmd.command,
                    "status": if cmd.completed { "completed" } else { "running" },
                    "exit_code": cmd.exit_code,
                    "output": result.lines.join("\n"),
                    "total_lines": result.total_lines,
                }));
            }
            return Err(RpcError::internal("No command found"));
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
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
    let origin_pane = params["origin_pane"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("origin_pane is required"))?;

    // Lint the command for anti-patterns
    if let Err(err) = crate::lint::lint_command_run(command) {
        return Err(RpcError::invalid_params(err.to_string()));
    }

    // Validate window access, get pane handle and shell PID
    let (pane_handle, shell_pid) = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        let handle = registry.get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?;
        let pid = {
            let tp = handle.lock().await;
            tp.pid.ok_or_else(|| RpcError::internal("Pane has no known PID"))?
        };
        (handle, pid)
    };

    // --- Policy check ---
    {
        let ctx = read_pane_context(state, pane_id).await?;
        let result = policy::evaluate(command, &ctx);
        match result.decision {
            policy::Decision::Allow => { /* proceed */ }
            policy::Decision::Deny => {
                return Err(RpcError {
                    code: -32001,
                    message: format!("Policy denied (rule: {})", result.rule),
                });
            }
            policy::Decision::Ask => {
                let live_ctx = read_pane_context(state, pane_id).await?;
                let verify = state
                    .approvals
                    .lock()
                    .await
                    .verify_and_consume(origin_pane, pane_id, command, &live_ctx);
                if let Err(msg) = verify {
                    return Err(RpcError {
                        code: -32001,
                        message: format!(
                            "Policy: {} (rule: {}). Install the tmux-mcp policy hook \
                             for interactive approval, or update the policy.",
                            msg, result.rule
                        ),
                    });
                }
            }
        }
    }

    let leaf_pid = get_leaf_pid(shell_pid);

    // --- Pre-execution guards ---
    // Check for conditions that would cause the OSC 133 probe to timeout
    // with a misleading error. Catch them early with specific messages.
    {
        let tp = pane_handle.lock().await;

        // Guard 1: Command already running
        if tp.processor.state().active_command().is_some()
            || tp.processor.state().activity == Activity::Busy
            || matches!(tp.processor.osc133_phase(), Osc133Phase::Executing { .. })
        {
            let cmd_text = tp
                .processor
                .osc133_phase()
                .executing_command()
                .or_else(|| {
                    tp.processor
                        .state()
                        .active_command()
                        .map(|c| c.command.as_str())
                })
                .unwrap_or("(unknown command)");
            let screen = capture_screen(&tp, 20);
            return Err(RpcError::internal(format!(
                "Pane {} is busy running '{}'. Use command_read to check status or wait.\n\nScreen:\n{}",
                pane_id, cmd_text, screen
            )));
        }
        // Guard 2: User is typing
        if tp.processor.has_input_content() {
            let screen = capture_screen(&tp, 20);
            return Err(RpcError::internal(format!(
                "User is typing in pane {}. Wait for them to finish or use a different pane.\n\nScreen:\n{}",
                pane_id, screen
            )));
        }
    }

    // --- OSC 133 gating ---
    let cache_status = {
        let tp = pane_handle.lock().await;
        tp.processor.state().osc133_lookup(leaf_pid)
    };

    match cache_status {
        Some(false) => {
            // Known failed — instant reject
            let tp = pane_handle.lock().await;
            let screen = capture_screen(&tp, 20);
            let marker_secs = tp.processor.state().last_osc133_marker
                .map(|t| t.elapsed().as_secs_f64());
            return Err(RpcError::internal(format!(
                "OSC 133 not active for this pane. Use inject_osc133 to enable shell integration.\n\nosc133_last_marker_secs: {:?}\n\nScreen:\n{}",
                marker_secs, screen
            )));
        }
        None => {
            // Unknown — probe with ` :`
            let probed_ok = probe_osc133(pane_id, &pane_handle, state, 500).await?;

            {
                let mut tp = pane_handle.lock().await;
                if probed_ok {
                    tp.processor.state_mut().osc133_confirm(leaf_pid);
                } else {
                    tp.processor.state_mut().osc133_fail(leaf_pid);
                    let screen = capture_screen(&tp, 20);
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

    // Parse read params
    let read_params = ReadParams::from_json(params)?;

    // --- Send the command ---
    // Snapshot the front command ID so we can wait for a *new* record
    let id_before = {
        let tp = pane_handle.lock().await;
        tp.processor.state().commands.front().map(|c| c.id).unwrap_or(0)
    };

    {
        let mut conn = state.conn.lock().await;
        conn.send_command(pane_id, command)
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send command: {}", e)))?;
    }

    // --- Unified poll: wait for new command record, then for completion ---
    // Phase 1 (first 500ms): wait for C marker to push a new record (id > id_before).
    //   If no new record appears, OSC 133 is broken — error out.
    // Phase 2 (remaining timeout): wait for that record to complete.
    let marker_deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(500);
    let completion_deadline =
        tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    let mut command_seen = false;

    loop {
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        {
            let mut tp = pane_handle.lock().await;
            let pane_state = tp.processor.state_mut();
            if let Some(cmd) = pane_state.commands.front_mut() {
                if cmd.id > id_before {
                    command_seen = true;
                    if cmd.completed {
                        let result = apply_read_params(&cmd.output, &mut cmd.read_cursor, &read_params);
                        return Ok(json!({
                            "command_id": cmd.id,
                            "command": cmd.command,
                            "status": "completed",
                            "exit_code": cmd.exit_code,
                            "output": result.lines.join("\n"),
                            "total_lines": result.total_lines,
                            "lines_skipped": result.skipped,
                            "search_matches": result.matched,
                        }));
                    }
                }
            }
        }

        let now = tokio::time::Instant::now();

        // Phase 1 timeout: no new command record appeared
        if !command_seen && now >= marker_deadline {
            tracing::warn!("OSC 133 markers not seen after sending command to pane {}", pane_id);
            let mut tp = pane_handle.lock().await;
            tp.processor.state_mut().osc133_fail(leaf_pid);
            let screen = capture_screen(&tp, 20);
            return Err(RpcError::internal(format!(
                "Command was sent but no OSC 133 markers detected. Shell integration may have stopped working. \
                 Use debug_pane to inspect pane state.\n\nScreen:\n{}",
                screen
            )));
        }

        // Phase 2 timeout: command seen but not completed
        if now >= completion_deadline {
            let mut tp = pane_handle.lock().await;
            let pane_state = tp.processor.state_mut();
            if let Some(cmd) = pane_state.commands.front_mut() {
                if cmd.id > id_before {
                    let result = apply_read_params(&cmd.output, &mut cmd.read_cursor, &read_params);
                    return Ok(json!({
                        "command_id": cmd.id,
                        "command": cmd.command,
                        "status": "running",
                        "output": result.lines.join("\n"),
                        "total_lines": result.total_lines,
                    }));
                }
            }
            return Ok(json!({
                "status": "running",
                "output": "",
                "total_lines": 0,
            }));
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

    // Validate window access and get pane handle
    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry.get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    // Read from alacritty screen model
    let tp = pane_handle.lock().await;
    let text = capture_screen(&tp, lines);

    // Include active command metadata so MCP layer can steer LLMs toward command_read
    let active_cmd = tp.processor.state().active_command().map(|cmd| {
        json!({
            "command_id": cmd.id,
            "command": cmd.command,
        })
    });

    Ok(json!({
        "text": text,
        "active_command": active_cmd,
    }))
}

/// OSC 133 + OSC 7 injection script for bash.
/// Each line has leading space for history suppression.
const OSC133_INJECT_LINES: &[&str] = &[
    " __osc133_exec_ready=true; __osc7_cwd() { printf '\\e]7;file://%s@%s%s\\e\\\\' \"$USER\" \"$(hostname)\" \"$PWD\"; }; __osc133_precmd() { local ret=$?; __osc133_exec_ready=true; printf '\\e]133;D;%d\\e\\\\\\e]133;A\\e\\\\' \"$ret\"; __osc7_cwd; return \"$ret\"; }; PROMPT_COMMAND=\"__osc133_precmd;${PROMPT_COMMAND:-}\"",
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

    // Validate window access and get pane handle + shell PID
    let (pane_handle, shell_pid) = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        let handle = registry.get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?;
        let pid = {
            let tp = handle.lock().await;
            tp.pid.ok_or_else(|| RpcError::internal("Pane has no known PID"))?
        };
        (handle, pid)
    };

    let leaf_pid = get_leaf_pid(shell_pid);

    // --- Probe first: maybe markers already work ---
    let already_active = probe_osc133(pane_id, &pane_handle, state, 500).await?;

    if already_active {
        let mut tp = pane_handle.lock().await;
        tp.processor.state_mut().osc133_confirm(leaf_pid);
        return Ok(json!({ "status": "already_active" }));
    }

    // --- Inject the script ---
    for line in OSC133_INJECT_LINES {
        {
            let mut conn = state.conn.lock().await;
            conn.send_command(pane_id, line)
                .await
                .map_err(|e| RpcError::internal(format!("Failed to send injection: {}", e)))?;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    }

    // --- Post-inject probe ---
    let inject_ok = probe_osc133(pane_id, &pane_handle, state, 500).await?;

    let mut tp = pane_handle.lock().await;
    if inject_ok {
        tp.processor.state_mut().osc133_confirm(leaf_pid);
        Ok(json!({ "status": "active" }))
    } else {
        tp.processor.state_mut().osc133_fail(leaf_pid);
        let screen = capture_screen(&tp, 20);
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

    if keys.len() > 5 {
        return Err(RpcError::invalid_params(
            format!("keys too long: {} chars (max 5). Use command_run for commands.", keys.len()),
        ));
    }

    // Validate window access and get pane handle
    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry.get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    // Send keys
    {
        let mut conn = state.conn.lock().await;
        conn.send_raw_keys(pane_id, keys)
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send keys: {}", e)))?;
    }

    // Brief pause for terminal to process, then return screen
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    let tp = pane_handle.lock().await;
    let screen = capture_screen(&tp, 20);
    Ok(json!({ "screen": screen }))
}

// --- Policy ---

/// Read the current pane context for policy evaluation.
async fn read_pane_context(
    state: &Arc<DaemonState>,
    pane_id: &str,
) -> Result<policy::PaneContext, RpcError> {
    let handle = {
        let registry = state.registry.read().await;
        registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };
    let tp = handle.lock().await;
    let term_state = tp.processor.state();
    Ok(policy::PaneContext {
        hostname: term_state.hostname.clone(),
        cwd: term_state.cwd.clone(),
        foreground: tp.pid.and_then(|pid| {
            proc::proc_info(pid).and_then(|info| info.foreground.map(|f| f.comm.clone()))
        }),
        user: term_state.user.clone(),
    })
}

async fn handle_request_approval(
    params: &Value,
    state: &Arc<DaemonState>,
) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let command = params["command"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("command is required"))?;

    // Window scoping still applies
    let origin_pane = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        params["origin_pane"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("origin_pane is required"))?
            .to_string()
    };

    let ctx = read_pane_context(state, pane_id).await?;
    let result = policy::evaluate(command, &ctx);

    let ctx_json = json!({
        "hostname": ctx.hostname,
        "cwd": ctx.cwd,
        "user": ctx.user,
        "foreground": ctx.foreground,
    });

    match result.decision {
        policy::Decision::Allow => Ok(json!({"result": "allow", "rule": result.rule})),
        policy::Decision::Deny => {
            let mut resp = json!({"result": "deny", "rule": result.rule});
            resp.as_object_mut().unwrap().extend(ctx_json.as_object().unwrap().clone());
            Ok(resp)
        }
        policy::Decision::Ask => {
            state
                .approvals
                .lock()
                .await
                .store(&origin_pane, pane_id, command, &ctx);
            let mut resp = json!({"result": "ask", "rule": result.rule});
            resp.as_object_mut().unwrap().extend(ctx_json.as_object().unwrap().clone());
            Ok(resp)
        }
    }
}
