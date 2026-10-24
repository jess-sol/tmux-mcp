/// RPC method dispatch and handlers.
use std::sync::Arc;

use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Duration, Instant};

use crate::pane::osc133::Osc133Phase;
use crate::pane::registry::{PaneHandle, PaneMode, PaneRegistry, Stale};
use crate::pane::snapshot::{Fact, PaneSnapshot};
use crate::pane::state::CommandRecord;
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
        Self {
            code: -32601,
            message: format!("Unknown method: {}", method),
        }
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
        }
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
    pub policy: std::sync::Arc<crate::policy::PolicyEngine>,
    pub started_at: std::time::Instant,
    /// `$TMUX_MCP_SEND_DELAY_MS`: a wait before each write to a pane, so a
    /// test can change the pane between a decision and its write.
    pub send_delay: Duration,
}

impl DaemonState {
    /// Types `line` and Enter into the pane while it is still at `version`.
    /// The phase is Unknown from then until a marker answers. The tmux
    /// connection stays locked from the check through the write, so no other
    /// write comes between them.
    pub async fn send_command(
        &self,
        pane_id: &str,
        version: u64,
        line: &str,
    ) -> Result<(), RpcError> {
        let pane = self.pane_to_write(pane_id).await?;
        let mut conn = self.conn.lock().await;
        pane.sending_command(version)
            .await
            .map_err(|Stale| stale(pane_id))?;
        conn.send_command(pane_id, line)
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send to {pane_id}: {e}")))
    }

    /// Sends tmux key names to the pane while it is still at `version`.
    pub async fn send_keys(&self, pane_id: &str, version: u64, keys: &str) -> Result<(), RpcError> {
        let pane = self.pane_to_write(pane_id).await?;
        let mut conn = self.conn.lock().await;
        if pane.snapshot().version != version {
            return Err(stale(pane_id));
        }
        conn.send_raw_keys(pane_id, keys)
            .await
            .map_err(|e| RpcError::internal(format!("Failed to send to {pane_id}: {e}")))
    }

    /// The pane a write goes to, after the test delay before each write.
    async fn pane_to_write(&self, pane_id: &str) -> Result<PaneHandle, RpcError> {
        tokio::time::sleep(self.send_delay).await;
        self.registry
            .read()
            .await
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {pane_id}")))
    }
}

/// A write refused because the pane changed after the decision to write.
fn stale(pane_id: &str) -> RpcError {
    RpcError::internal(format!(
        "Pane {pane_id} changed before the send; nothing was sent. Try again."
    ))
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
        "version" => Ok(json!({ "version": crate::version::Version::current().to_string() })),
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
        .ok_or_else(|| {
            RpcError::invalid_params(format!("Origin pane {} not tracked by daemon", origin_pane))
        })
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
    before: usize,
    after: usize,
    /// The reader's place: a line of the command it names.
    offset: Option<(u64, usize)>,
}

impl ReadParams {
    /// The line `next` starts at in `command_id`: the offset's line when the
    /// offset names that command, else 0.
    fn start(&self, command_id: u64) -> usize {
        match self.offset {
            Some((id, line)) if id == command_id => line,
            _ => 0,
        }
    }
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

        let search =
            if let Some(pattern) = params["search"].as_str() {
                Some(regex::Regex::new(pattern).map_err(|e| {
                    RpcError::invalid_params(format!("invalid search regex: {}", e))
                })?)
            } else {
                None
            };

        let before = params["before"].as_u64().unwrap_or(0) as usize;
        let after = params["after"].as_u64().unwrap_or(0) as usize;

        if (before > 0 || after > 0) && search.is_none() {
            return Err(RpcError::invalid_params("before and after require search"));
        }

        let offset = params["offset"]["command_id"]
            .as_u64()
            .zip(params["offset"]["line"].as_u64())
            .map(|(id, line)| (id, line as usize));

        Ok(Self {
            next,
            head,
            tail,
            search,
            before,
            after,
            offset,
        })
    }
}

/// Result of applying read params to command output.
#[derive(Default)]
struct ReadResult {
    lines: Vec<String>,
    total_lines: usize,
    skipped: usize,
    matched: Option<usize>, // number of search matches (if search was used)
    /// The last selected match's after-context runs past the whole lines so far.
    context_pending: bool,
}

/// Expand match positions into merged (start, end) ranges with before/after context.
/// Returns inclusive ranges sorted by position.
fn merge_context_ranges(
    match_indices: &[usize],
    before: usize,
    after: usize,
    total_lines: usize,
) -> Vec<(usize, usize)> {
    if match_indices.is_empty() || total_lines == 0 {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    for &m in match_indices {
        let start = m.saturating_sub(before);
        let end = (m + after).min(total_lines - 1);
        if let Some((_, prev_end)) = ranges.last_mut()
            && start <= *prev_end + 1
        {
            // Merge: extend previous range
            *prev_end = end.max(*prev_end);
            continue;
        }
        ranges.push((start, end));
    }
    ranges
}

/// Collect lines from merged ranges, inserting "--" separators between groups.
fn collect_context_lines(all_lines: &[String], ranges: &[(usize, usize)]) -> Vec<String> {
    let mut result = Vec::new();
    for (i, &(start, end)) in ranges.iter().enumerate() {
        if i > 0 {
            result.push("--".to_string());
        }
        result.extend(all_lines[start..=end].iter().cloned());
    }
    result
}

/// Apply read params (next/head/tail/search) to a command record.
/// Advances cursor for `next` and bare reads. Returns selected lines.
///
/// When `search` is present, windowing (next/head/tail) limits the number
/// of *matches*, not raw lines. `next+search` scans forward from cursor,
/// consuming non-matching lines along the way.
///
/// A line without its newline shows in reads, but no read consumes it until
/// `output_done`, so a later `next` still finds it once it ends.
fn apply_read_params(
    output: &str,
    output_done: bool,
    cursor: &mut usize,
    params: &ReadParams,
) -> ReadResult {
    // The terminal adds `\r` to each `\n`, and shells injected with an older
    // PS0 line send readline's `\x01`/`\x02` markers; neither is the command's.
    // A `\r` inside a line redraws it, as a progress bar does, so a line is
    // what follows its last one.
    let text = output.replace(['\x01', '\x02'], "");
    let all_lines: Vec<String> = text
        .split_terminator('\n')
        .map(|line| {
            let line = line.trim_end_matches('\r');
            line.rsplit('\r').next().unwrap_or(line).to_string()
        })
        .collect();
    let total_lines = all_lines.len();
    let consumable = if output_done || text.ends_with('\n') {
        total_lines
    } else {
        total_lines.saturating_sub(1)
    };
    let context_pending = |selected: &[usize]| {
        selected
            .last()
            .is_some_and(|&m| params.after > 0 && m + params.after >= consumable)
    };

    if let Some(re) = &params.search {
        let has_context = params.before > 0 || params.after > 0;

        if let Some(n) = params.next {
            // Scan from cursor, collect up to N matches, advance cursor past all scanned.
            let start = (*cursor).min(consumable);
            let mut match_indices = Vec::new();
            let mut scanned = 0;
            for (i, line) in all_lines[start..consumable].iter().enumerate() {
                scanned = i + 1;
                if re.is_match(line) {
                    match_indices.push(start + i);
                    if match_indices.len() >= n {
                        break;
                    }
                }
            }
            let matched = match_indices.len();

            let lines = if has_context {
                let ranges =
                    merge_context_ranges(&match_indices, params.before, params.after, total_lines);
                let display_end = ranges.last().map(|(_, e)| e + 1).unwrap_or(start + scanned);
                *cursor = (start + scanned).max(display_end).min(consumable);
                collect_context_lines(&all_lines, &ranges)
            } else {
                *cursor = start + scanned;
                match_indices
                    .iter()
                    .map(|&i| all_lines[i].clone())
                    .collect()
            };

            let context_pending = context_pending(&match_indices);
            ReadResult {
                lines,
                total_lines,
                skipped: start,
                matched: Some(matched),
                context_pending,
            }
        } else {
            // head/tail/bare: collect all match indices, then window.
            let all_match_indices: Vec<usize> = all_lines
                .iter()
                .enumerate()
                .filter(|(_, l)| re.is_match(l))
                .map(|(i, _)| i)
                .collect();
            let total_matches = all_match_indices.len();

            let (selected_indices, skipped) = if let Some(n) = params.head {
                let end = total_matches.min(n);
                (&all_match_indices[..end], 0)
            } else if let Some(n) = params.tail {
                // Exclude matches on lines already consumed by next (unless cursor at end)
                let effective_start = if *cursor > 0 && *cursor < total_lines {
                    all_match_indices.partition_point(|&i| i < *cursor)
                } else {
                    0
                };
                let effective_count = all_match_indices.len() - effective_start;
                let tail_skip = effective_count.saturating_sub(n);
                let final_start = effective_start + tail_skip;
                (&all_match_indices[final_start..], final_start)
            } else {
                // bare + search: all matches, advance cursor
                *cursor = consumable;
                (&all_match_indices[..], 0)
            };

            let lines = if has_context {
                let ranges = merge_context_ranges(
                    selected_indices,
                    params.before,
                    params.after,
                    total_lines,
                );
                collect_context_lines(&all_lines, &ranges)
            } else {
                selected_indices
                    .iter()
                    .map(|&i| all_lines[i].clone())
                    .collect()
            };

            let context_pending = context_pending(selected_indices);
            ReadResult {
                lines,
                total_lines,
                skipped,
                matched: Some(total_matches),
                context_pending,
            }
        }
    } else {
        // No search: window raw lines.
        let (selected, skipped) = if let Some(n) = params.next {
            let start = (*cursor).min(consumable);
            let end = consumable.min(start + n);
            let lines = all_lines[start..end].to_vec();
            *cursor = end;
            (lines, start)
        } else if let Some(n) = params.head {
            let end = total_lines.min(n);
            (all_lines[..end].to_vec(), 0)
        } else if let Some(n) = params.tail {
            let mut start = total_lines.saturating_sub(n);
            // Don't re-show lines already consumed by next (unless cursor at end = re-read)
            if *cursor > 0 && *cursor < total_lines {
                start = start.max(*cursor);
            }
            (all_lines[start..].to_vec(), start)
        } else {
            // bare: all output, advance cursor
            *cursor = consumable;
            (all_lines, 0)
        };
        ReadResult {
            lines: selected,
            total_lines,
            skipped,
            matched: None,
            context_pending: false,
        }
    }
}

/// How to treat a "bare" read — no `next`/`head`/`tail`/`search` — on a still-running
/// command. `command_read` returns whatever output exists immediately; `command_run`
/// blocks until the command completes or the deadline elapses.
#[derive(Copy, Clone)]
enum BareReadPolicy {
    ReturnNow,
    WaitForCompletion,
}

/// Wait for output from a command and return a result JSON.
///
/// Shared between `command_read` and `command_run`'s post-marker phase.
/// `command_id = None` reads the front (most recent) command — `command_read`'s
/// default. `command_run` passes the id of the record it pushed.
///
/// Returns as soon as one of:
///   * the command has finished — its D marker, or the B marker of the next
///     prompt (status: "completed"),
///   * `next`/`head` is satisfied — N lines (or, with `search`, N matches with
///     their after-context) are available — and the command has not finished
///     within `WINDOW_SETTLE` after that (status: "running"),
///   * a bare read (no next/head/tail/search) with `bare_policy = ReturnNow`
///     observes the new record (status: "running"),
///   * the deadline elapses (status: "running", with whatever output exists).
///
/// `tail`, bare+search, and bare reads with `bare_policy = WaitForCompletion`
/// wait for completion or the deadline; they don't early-return on partial output.
async fn wait_and_read(
    pane_handle: &PaneHandle,
    command_id: Option<u64>,
    read_params: &ReadParams,
    deadline: tokio::time::Instant,
    bare_policy: BareReadPolicy,
) -> Result<Value, RpcError> {
    // Determine how many result lines constitute "enough" to return early.
    let required_lines = read_params.next.or(read_params.head);
    let is_tail = read_params.tail.is_some();
    let mut rx = pane_handle.subscribe_state();
    // Set once a next/head window fills: a command that ends at once still
    // reports its exit code rather than "running".
    let mut settle_until: Option<tokio::time::Instant> = None;
    // D can arrive before prompt markers and a late explicit command-text E.
    // Give those markers a short window to finalize the record before replying.
    let mut complete_settle: Option<tokio::time::Instant> = None;

    loop {
        {
            let snapshot = pane_handle.snapshot();
            let cmd = match command_id {
                Some(id) => snapshot.commands.iter().find(|c| c.id == id),
                None => snapshot.commands.first(),
            };
            let Some(cmd) = cmd else {
                return Err(match command_id {
                    Some(id) => RpcError::invalid_params(format!("Command {id} not found")),
                    None => RpcError::invalid_params("No commands in history"),
                });
            };
            // Read after the snapshot, so a command it shows done has all its
            // output here.
            let output = pane_handle.output(cmd.id).await.unwrap_or_default();
            let start = read_params.start(cmd.id);
            let reply = |status, result: &ReadResult, offset| {
                read_reply(cmd, snapshot.version, status, result, offset)
            };

            // D ends the output and carries the exit code; B and sometimes E
            // follow while the next prompt is drawn.
            if cmd.completed || cmd.output_done {
                let now = tokio::time::Instant::now();
                if cmd.completed
                    || now
                        >= *complete_settle.get_or_insert((now + COMPLETION_SETTLE).min(deadline))
                {
                    let mut offset = start;
                    let result = apply_read_params(&output, true, &mut offset, read_params);
                    return Ok(reply("completed", &result, offset));
                }
            }

            // Do not report the command as running while its prompt markers
            // are still settling.
            if cmd.output_done {
                let wake = complete_settle.unwrap_or(deadline).min(deadline);
                tokio::select! {
                    _ = rx.changed() => {}
                    _ = tokio::time::sleep_until(wake) => {}
                }
                continue;
            }

            let mut offset = start;
            let peeked = apply_read_params(&output, false, &mut offset, read_params);

            let has_enough = if let Some(n) = required_lines {
                // When search is active, count matches (not output lines
                // which may be inflated by before/after context).
                peeked.matched.unwrap_or(peeked.lines.len()) >= n && !peeked.context_pending
            } else if is_tail || peeked.matched.is_some() {
                false // tail / bare+search: wait for completion or timeout
            } else {
                // Bare read (no next/head/tail/search): policy decides.
                matches!(bare_policy, BareReadPolicy::ReturnNow)
            };

            // A filled next/head window first waits out the settle; a bare
            // read returns at once.
            let now = tokio::time::Instant::now();
            if has_enough
                && (required_lines.is_none()
                    || now >= *settle_until.get_or_insert((now + WINDOW_SETTLE).min(deadline)))
            {
                return Ok(reply("running", &peeked, offset));
            }

            // Timeout: tail returns nothing, the others what there is.
            if now >= deadline {
                if is_tail {
                    return Ok(reply("running", &ReadResult::default(), start));
                }
                return Ok(reply("running", &peeked, offset));
            }
        }

        let wake = [settle_until, complete_settle]
            .into_iter()
            .flatten()
            .fold(deadline, |wake, settle| wake.min(settle));
        tokio::select! {
            _ = rx.changed() => {}
            _ = tokio::time::sleep_until(wake) => {}
        }
    }
}

/// The reply to a read of `cmd` at `version`: its window of output, and the
/// `offset` the reader's next read of `cmd` starts from. The reader keeps the
/// offset; the daemon keeps nothing.
fn read_reply(
    cmd: &CommandRecord,
    version: u64,
    status: &str,
    result: &ReadResult,
    offset: usize,
) -> Value {
    json!({
        "command_id": cmd.id,
        "command": cmd.command,
        "version": version,
        "status": status,
        "exit_code": cmd.exit_code,
        "output": result.lines.join("\n"),
        "total_lines": result.total_lines,
        "lines_skipped": result.skipped,
        "search_matches": result.matched,
        "offset": { "command_id": cmd.id, "line": offset },
    })
}

/// How long a filled `next`/`head` window waits for the command's D marker, so a
/// command that ends at once reports its exit code instead of "running".
const WINDOW_SETTLE: tokio::time::Duration = tokio::time::Duration::from_millis(100);

/// Delay after D for prompt markers (B, and a late E) to finalize the reply.
const COMPLETION_SETTLE: tokio::time::Duration = tokio::time::Duration::from_millis(150);

/// The pane's last 20 rows, headed by the *Pane version* they show, which a
/// `press_key` after them takes.
async fn shown_screen(pane_handle: &PaneHandle) -> String {
    let screen = pane_handle.screen().await;
    format!(
        "Screen at version {}:\n{}",
        screen.snapshot.version,
        screen.last_rows(20)
    )
}

/// How long a command line of the daemon's waits for the shell's markers: a
/// shell with integration answers at once.
const MARKER_TIMEOUT: Duration = Duration::from_secs(1);

/// Probes OSC 133 by sending ` :` at the snapshot's *Pane version*. Returns
/// the snapshot that answers it: at `Input` when the shell sends markers,
/// with integration false when it sends none.
async fn probe_osc133(
    pane_id: &str,
    pane_handle: &PaneHandle,
    state: &Arc<DaemonState>,
    snapshot: &PaneSnapshot,
) -> Result<Arc<PaneSnapshot>, RpcError> {
    let rx = pane_handle.subscribe_state();
    state.send_command(pane_id, snapshot.version, " :").await?;
    Ok(at_prompt(pane_handle, rx).await)
}

/// The first snapshot at `Input`. A shell that brings none in time sends no
/// markers, so the snapshot after the wait says integration is false.
async fn at_prompt(
    pane_handle: &PaneHandle,
    mut rx: tokio::sync::watch::Receiver<u64>,
) -> Arc<PaneSnapshot> {
    let deadline = Instant::now() + MARKER_TIMEOUT;
    loop {
        let snapshot = pane_handle.snapshot();
        if snapshot.phase == Fact::Known(Osc133Phase::Input) {
            return snapshot;
        }
        if Instant::now() >= deadline {
            pane_handle.set_integration(Fact::Known(false)).await;
            return pane_handle.snapshot();
        }
        tokio::select! {
            _ = rx.changed() => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

// --- Handlers ---

#[derive(Serialize)]
struct PaneEntry {
    pane_id: String,
    /// The *Pane version* a `press_key` takes.
    version: u64,
    pid: Option<u32>,
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    /// CWD from /proc (local process).
    process_cwd: Option<String>,
    /// CWD from OSC 7 (works across SSH), while it says where the next
    /// command runs.
    osc_cwd: Option<String>,
    /// Username from OSC 7 userinfo (null if never received).
    osc_user: Option<String>,
    /// Hostname from OSC 7 (null for localhost or if never received).
    osc_hostname: Option<String>,
    foreground: Option<String>,
    activity: String,
    /// The tmux mode the pane is in (`copy-mode`), or null in none.
    mode: Option<String>,
    /// Seconds since last terminal data received, or null if never seen.
    last_data_secs: Option<f64>,
    /// "confirmed" once the shell sent a marker, "failed" when a command line
    /// got none, else "unknown".
    osc133_status: String,
}

/// What the OSC 133 phase says the pane is doing: "Busy", "Idle" or "Unknown".
fn activity(phase: &Fact<Osc133Phase>) -> &'static str {
    match phase {
        Fact::Known(Osc133Phase::Executing { .. }) => "Busy",
        Fact::Known(Osc133Phase::Input) => "Idle",
        _ => "Unknown",
    }
}

/// Whether the shell sends OSC 133 markers.
fn osc133_status(integration: &Fact<bool>) -> &'static str {
    match integration {
        Fact::Known(true) => "confirmed",
        Fact::Known(false) => "failed",
        Fact::Unknown => "unknown",
    }
}

async fn handle_list_panes(params: &Value, state: &Arc<DaemonState>) -> Result<Value, RpcError> {
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
        let screen = handle.screen().await;
        let snapshot = &screen.snapshot;

        let (process_cwd, foreground) = if let Some(pid) = snapshot.pid {
            let info = proc::proc_info(pid);
            (
                info.as_ref()
                    .and_then(|i| i.cwd.as_ref().map(|p| p.display().to_string())),
                info.as_ref()
                    .and_then(|i| i.foreground.as_ref().map(|f| f.comm.clone())),
            )
        } else {
            (None, None)
        };

        entries.push(PaneEntry {
            pane_id: handle.pane_id.clone(),
            version: snapshot.version,
            pid: snapshot.pid,
            width: screen.width,
            height: screen.height,
            x: screen.x,
            y: screen.y,
            process_cwd,
            osc_cwd: snapshot.cwd.clone(),
            osc_user: snapshot.user.clone(),
            osc_hostname: snapshot.host.clone().filter(|host| *host != local_hostname),
            foreground,
            activity: activity(&snapshot.phase).to_string(),
            mode: match &snapshot.mode {
                Fact::Known(PaneMode::On(mode)) => Some(mode.clone()),
                _ => None,
            },
            last_data_secs: screen.last_output.map(|t| t.elapsed().as_secs_f64()),
            osc133_status: osc133_status(&snapshot.integration).to_string(),
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
        registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    let snapshot = pane_handle.snapshot();
    let mut entries = Vec::new();
    for cmd in snapshot.commands.iter().take(count) {
        let output = pane_handle.output(cmd.id).await.unwrap_or_default();
        let line_count = output.strip_suffix('\n').unwrap_or(&output).lines().count();
        entries.push(json!({
            "command_id": cmd.id,
            "command": cmd.command,
            "exit_code": cmd.exit_code,
            "output_lines": line_count,
        }));
    }

    Ok(json!(entries))
}

async fn handle_command_read(params: &Value, state: &Arc<DaemonState>) -> Result<Value, RpcError> {
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
        registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(timeout_secs);
    wait_and_read(
        &pane_handle,
        command_id,
        &read_params,
        deadline,
        BareReadPolicy::ReturnNow,
    )
    .await
}

async fn handle_command_run(params: &Value, state: &Arc<DaemonState>) -> Result<Value, RpcError> {
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

    // Validate window access and get the pane handle
    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    if pane_id == origin_pane && !origin_target_allowed() {
        return Err(RpcError::invalid_params(format!(
            "Refusing to run commands in pane {pane_id}: that pane hosts the agent itself, so the \
             command would be typed into the agent's terminal. Target another pane or explicitly \
             allow this with TMUX_MCP_ALLOW_ORIGIN=1."
        )));
    }

    let read_params = ReadParams::from_json(params)?;

    // The probe types into the pane, so the pane's checks guard it too.
    let mut snapshot = pane_handle.snapshot();
    check_pane(pane_id, &pane_handle, &snapshot).await?;
    if snapshot.phase == Fact::Unknown {
        snapshot = probe_osc133(pane_id, &pane_handle, state, &snapshot).await?;
        check_pane(pane_id, &pane_handle, &snapshot).await?;
    }
    let approved = check_policy(state, origin_pane, pane_id, command, &snapshot).await?;

    let newest = snapshot.commands.first().map_or(0, |c| c.id);
    let rx = pane_handle.subscribe_state();
    state
        .send_command(pane_id, snapshot.version, command)
        .await?;
    if approved {
        state.approvals.lock().await.consume(origin_pane, pane_id);
    }
    let completion_deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let new_command_id = started_command(pane_id, &pane_handle, newest, rx).await?;

    // --- Wait and read ---
    // Returns when the command completes, when next/head (± search) is satisfied,
    // or when the completion deadline elapses (status: "running").
    // Bare reads wait for completion (unlike command_read, which returns immediately).
    wait_and_read(
        &pane_handle,
        Some(new_command_id),
        &read_params,
        completion_deadline,
        BareReadPolicy::WaitForCompletion,
    )
    .await
}

/// Refuses a pane where a command line would not reach a shell at its
/// prompt. An Unknown phase passes only while integration is Unknown too:
/// the probe makes both known.
async fn check_pane(
    pane_id: &str,
    pane_handle: &PaneHandle,
    snapshot: &PaneSnapshot,
) -> Result<(), RpcError> {
    check_mode(pane_id, snapshot)?;
    let input = &snapshot.input;
    let refusal = match (&snapshot.phase, &snapshot.integration) {
        (Fact::Known(Osc133Phase::Executing { command }), _) => format!(
            "Pane {pane_id} is busy running '{command}'. Use command_read to check status or wait."
        ),
        (Fact::Unknown, Fact::Known(true)) => {
            format!("Pane {pane_id} is busy: a command line is on its way to its shell. Try again.")
        }
        (Fact::Unknown, Fact::Known(false)) => format!(
            "Pane {pane_id} sent no OSC 133 marker for the last command line. Its shell may \
             have no integration (inject_osc133 adds it), or the line may be unfinished, such \
             as an open quote (press_key C-c clears it)."
        ),
        (Fact::Unknown, Fact::Unknown) => return Ok(()),
        _ if !input.is_empty() => {
            let garbled = if snapshot.input_garbled {
                "\nThe cursor sits outside that text, so it may be a garbled line rather \
                 than typing. If the screen shows no command being typed, press_key C-c \
                 clears the line.\n"
            } else {
                ""
            };
            format!(
                "User is typing in pane {pane_id}. Wait for them to finish or use a different \
                 pane.\nRead as input: {input:?}\n{garbled}"
            )
        }
        _ => return Ok(()),
    };
    let screen = shown_screen(pane_handle).await;
    Err(RpcError::internal(format!("{refusal}\n{screen}")))
}

/// Refuses a pane in a mode: a mode takes typed keys, so they would never
/// reach the shell.
fn check_mode(pane_id: &str, snapshot: &PaneSnapshot) -> Result<(), RpcError> {
    let refusal = match &snapshot.mode {
        Fact::Known(PaneMode::Off) => return Ok(()),
        Fact::Known(PaneMode::On(mode)) => format!(
            "Pane {pane_id} is in {mode}, which takes typed keys instead of the shell: the \
             person may be scrolling back or selecting. Wait for them to leave it, ask them, \
             or use a different pane."
        ),
        Fact::Unknown => {
            format!("Pane {pane_id} changed mode and tmux has not said to which; try again.")
        }
    };
    Err(RpcError::internal(refusal))
}

/// Judges `command` where the snapshot says it runs. True when it runs on
/// the person's approval, which the caller consumes once the command is sent.
async fn check_policy(
    state: &Arc<DaemonState>,
    origin_pane: &str,
    pane_id: &str,
    command: &str,
    snapshot: &PaneSnapshot,
) -> Result<bool, RpcError> {
    let ctx = policy::PaneContext::from(snapshot);
    let result = policy::evaluate(command, &ctx, policy::Via::Tmux, &state.policy);
    match result.decision {
        policy::Decision::Allow => Ok(false),
        policy::Decision::Deny => Err(RpcError {
            code: -32001,
            message: result.reason(),
        }),
        // *Passthrough* comes only from the Bash *Channel*.
        policy::Decision::Ask | policy::Decision::Passthrough => {
            let verified = state
                .approvals
                .lock()
                .await
                .verify(origin_pane, pane_id, command, &ctx);
            verified.map(|()| true).map_err(|msg| RpcError {
                code: -32001,
                message: format!(
                    "Policy: {} (rule: {}). Install the tmux-mcp policy hook for interactive \
                     approval, or update the policy.",
                    msg,
                    result.reason()
                ),
            })
        }
    }
}

/// The id of the command the pane started after `newest`: its C marker
/// pushes a record. A shell that sends none in time sends no markers, so
/// integration becomes false.
async fn started_command(
    pane_id: &str,
    pane_handle: &PaneHandle,
    newest: u64,
    mut rx: tokio::sync::watch::Receiver<u64>,
) -> Result<u64, RpcError> {
    let deadline = Instant::now() + MARKER_TIMEOUT;
    loop {
        let snapshot = pane_handle.snapshot();
        if let Some(command) = snapshot.commands.first()
            && command.id > newest
        {
            return Ok(command.id);
        }
        if Instant::now() >= deadline {
            pane_handle.set_integration(Fact::Known(false)).await;
            let screen = shown_screen(pane_handle).await;
            return Err(RpcError::internal(format!(
                "Command was sent to pane {pane_id} but no OSC 133 markers came back. Shell \
                 integration may have stopped working. Use debug_pane to inspect pane \
                 state.\n\n{screen}"
            )));
        }
        tokio::select! {
            _ = rx.changed() => {}
            _ = tokio::time::sleep_until(deadline) => {}
        }
    }
}

async fn handle_capture_pane(params: &Value, state: &Arc<DaemonState>) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let lines = params["lines"].as_u64().unwrap_or(50).min(1000) as usize;

    // Validate window access and get pane handle
    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    let screen = pane_handle.screen().await;

    // Include active command metadata so MCP layer can steer LLMs toward command_read
    let running = screen
        .snapshot
        .commands
        .first()
        .filter(|cmd| !cmd.completed && !cmd.output_done);
    let active_cmd = running.map(|cmd| {
        json!({
            "command_id": cmd.id,
            "command": cmd.command,
        })
    });

    Ok(json!({
        "text": screen.last_rows(lines),
        "version": screen.snapshot.version,
        "active_command": active_cmd,
    }))
}

/// Permit self-targeting for headless callers that intentionally map their
/// origin pane to a pane they want to drive.
fn origin_target_allowed() -> bool {
    matches!(
        std::env::var("TMUX_MCP_ALLOW_ORIGIN").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

/// OSC 133 + OSC 7 injection script for bash.
/// Each line has leading space for history suppression.
pub const OSC133_INJECT_LINES: &[&str] = &[
    " __osc133_exec_ready=true; __osc7_cwd() { printf '\\e]7;file://%s@%s%s\\e\\\\' \"$USER\" \"$(hostname)\" \"$PWD\"; }; __osc133_precmd() { local ret=$?; __osc133_exec_ready=true; __osc7_cwd; printf '\\e]133;D;%d\\e\\\\\\e]133;A\\e\\\\' \"$ret\"; return \"$ret\"; }; PROMPT_COMMAND=\"__osc133_precmd;${PROMPT_COMMAND:-}\"",
    " __osc133_debug() { if $__osc133_exec_ready; then __osc133_exec_ready=false; local cmd; cmd=$(HISTTIMEFORMAT= builtin history 1 | sed 's/^ *[0-9]* *//'); [[ -n \"$cmd\" ]] && printf '\\e]133;E;%s\\e\\\\' \"$cmd\"; fi; }; trap '__osc133_debug \"$_\"' DEBUG",
    " PS1=\"${PS1}\"$'\\001\\e]133;B\\e\\\\\\002'; PS0+='\\e]133;C\\e\\\\'",
];

/// OSC 133 integration for fish. Prompt markers bracket input; preexec and
/// postexec mark commands and preserve their exit status.
const OSC133_INJECT_LINES_FISH: &[&str] = &[
    " functions -q __tmux_mcp_osc133_saved_prompt; or functions -c fish_prompt __tmux_mcp_osc133_saved_prompt",
    " function fish_prompt; if set -q __tmux_mcp_osc133_status; printf '\\e]133;D;%s\\e\\\\' $__tmux_mcp_osc133_status; set -e __tmux_mcp_osc133_status; end; printf '\\e]133;A\\e\\\\'; if functions -q __tmux_mcp_osc133_saved_prompt; __tmux_mcp_osc133_saved_prompt; end; printf '\\e]133;B\\e\\\\'; end",
    " function __tmux_mcp_osc133_preexec --on-event fish_preexec; printf '\\e]133;C\\e\\\\'; end",
    " function __tmux_mcp_osc133_postexec --on-event fish_postexec; set -g __tmux_mcp_osc133_status $status; end",
];

fn osc133_injection_for(shell: &str) -> Option<&'static [&'static str]> {
    match shell.to_ascii_lowercase().trim_end_matches(".exe") {
        "bash" => Some(OSC133_INJECT_LINES),
        "fish" => Some(OSC133_INJECT_LINES_FISH),
        _ => None,
    }
}

async fn handle_inject_osc133(params: &Value, state: &Arc<DaemonState>) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;

    // Validate window access and get the pane handle
    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    // tmux's pane_current_command follows the foreground process; fall back
    // to /proc when tmux cannot answer.
    let shell = {
        let mut conn = state.conn.lock().await;
        conn.query_pane_command(pane_id)
            .await
            .ok()
            .filter(|name| !name.is_empty())
            .or_else(|| {
                pane_handle
                    .snapshot()
                    .pid
                    .and_then(crate::proc::process_name)
            })
            .unwrap_or_default()
    };
    let is_fish =
        shell.eq_ignore_ascii_case("fish") || shell.to_ascii_lowercase().ends_with("/fish");

    let mut snapshot = pane_handle.snapshot();
    check_mode(pane_id, &snapshot)?;
    if snapshot.phase == Fact::Known(Osc133Phase::Input) {
        return Ok(json!({ "status": "already_active", "shell": shell }));
    }
    if snapshot.phase == Fact::Unknown
        && snapshot.integration == Fact::Known(true)
        && (!is_fish || snapshot.commands.first().is_some_and(|cmd| !cmd.completed))
    {
        return Err(RpcError::internal(format!(
            "Pane {pane_id} is busy: a command line is on its way to its shell. Try again."
        )));
    }
    // Fish 4.x emits native OSC 133, but omits the B marker needed to mark
    // input-ready. Install our prompt wrapper directly instead of probing it.
    // Other shells are probed first, including a shell reached over SSH.
    if !is_fish && snapshot.integration != Fact::Known(false) {
        snapshot = probe_osc133(pane_id, &pane_handle, state, &snapshot).await?;
        if snapshot.phase == Fact::Known(Osc133Phase::Input) {
            return Ok(json!({ "status": "already_active", "shell": shell }));
        }
    }

    let inject_lines = match osc133_injection_for(&shell) {
        Some(lines) => lines,
        None if shell.is_empty() => OSC133_INJECT_LINES,
        None => {
            return Ok(json!({
                "status": "unsupported_shell",
                "shell": shell,
                "message": format!(
                    "Pane {pane_id} runs `{shell}`, which has no automatic shell integration (supported: bash, fish)."
                ),
            }));
        }
    };

    // One line: each line of its own would bring markers, and so a new *Pane
    // version*, before the next.
    let rx = pane_handle.subscribe_state();
    state
        .send_command(pane_id, snapshot.version, &inject_lines.join(";"))
        .await?;

    let answered = at_prompt(&pane_handle, rx).await;
    if answered.phase == Fact::Known(Osc133Phase::Input) {
        Ok(json!({ "status": "active", "shell": shell }))
    } else {
        Ok(json!({
            "status": "failed",
            "shell": shell,
            "message": format!("Injection sent but no markers detected for shell `{shell}`."),
            "screen": shown_screen(&pane_handle).await,
        }))
    }
}

/// The key names press_key's schema documents; they skip its length cap.
const KEY_NAMES: &[&str] = &[
    "Enter", "Escape", "BSpace", "Tab", "Space", "Up", "Down", "Left", "Right", "C-c", "C-d", "C-z",
];

async fn handle_send_keys(params: &Value, state: &Arc<DaemonState>) -> Result<Value, RpcError> {
    let pane_id = params["pane_id"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("pane_id is required"))?;
    let keys = params["keys"]
        .as_str()
        .ok_or_else(|| RpcError::invalid_params("keys is required"))?;
    let version = params["version"].as_u64().ok_or_else(|| {
        RpcError::invalid_params(
            "version is required: the pane version that list_panes, debug_pane or a command \
             result shows",
        )
    })?;

    // An escape hatch: key names pass, and other text stays short enough that
    // commands go through command_run.
    if keys.chars().any(char::is_control) {
        return Err(RpcError::invalid_params(format!(
            "keys {keys:?} holds a control character. Send a key name such as Enter instead."
        )));
    }
    if !KEY_NAMES.contains(&keys) && keys.chars().count() > 3 {
        return Err(RpcError::invalid_params(format!(
            "keys too long: {} chars (max 3, or a key name: {}). Use command_run for commands.",
            keys.chars().count(),
            KEY_NAMES.join(", "),
        )));
    }

    // Validate window access and get pane handle
    let pane_handle = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?
    };

    // Subscribed before the screen, so no change between them goes unseen.
    let mut rx = pane_handle.subscribe_state();
    let before = pane_handle.screen().await;

    state.send_keys(pane_id, version, keys).await?;

    // The screen is returned once the pane responds to the key, or after a
    // second for a key that changes nothing.
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        let after = pane_handle.screen().await;
        if (after.cursor, &after.rows) != (before.cursor, &before.rows) {
            break;
        }
        if !matches!(
            tokio::time::timeout_at(deadline, rx.changed()).await,
            Ok(Ok(()))
        ) {
            break;
        }
    }

    Ok(json!({ "screen": shown_screen(&pane_handle).await }))
}

// --- Policy ---

/// The name of the pane's foreground process, from `/proc`, for display.
async fn foreground_process(state: &Arc<DaemonState>, pane_id: &str) -> Option<String> {
    let handle = state.registry.read().await.get_handle(pane_id)?;
    let pid = handle.snapshot().pid?;
    proc::proc_info(pid).and_then(|info| info.foreground.map(|f| f.comm))
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
    let (origin_pane, pane_handle) = {
        let registry = state.registry.read().await;
        let caller_window = resolve_caller_window(&registry, params)?;
        validate_pane_access(&registry, pane_id, &caller_window)?;
        let origin_pane = params["origin_pane"]
            .as_str()
            .ok_or_else(|| RpcError::invalid_params("origin_pane is required"))?
            .to_string();
        let pane_handle = registry
            .get_handle(pane_id)
            .ok_or_else(|| RpcError::invalid_params(format!("Unknown pane: {}", pane_id)))?;
        (origin_pane, pane_handle)
    };

    let ctx = policy::PaneContext::from(&*pane_handle.snapshot());
    let result = policy::evaluate(command, &ctx, policy::Via::Tmux, &state.policy);

    let ctx_json = json!({
        "hostname": ctx.hostname,
        "cwd": ctx.cwd,
        "user": ctx.user,
        // For the person reading the prompt; no decision reads it.
        "foreground": foreground_process(state, pane_id).await,
    });

    match result.decision {
        policy::Decision::Allow => Ok(json!({"result": "allow", "rule": result.reason()})),
        policy::Decision::Deny => {
            let mut resp = json!({"result": "deny", "rule": result.reason()});
            resp.as_object_mut()
                .unwrap()
                .extend(ctx_json.as_object().unwrap().clone());
            Ok(resp)
        }
        // *Passthrough* comes only from the Bash *Channel*.
        policy::Decision::Ask | policy::Decision::Passthrough => {
            state
                .approvals
                .lock()
                .await
                .store(&origin_pane, pane_id, command, &ctx);
            let mut resp = json!({"result": "ask", "rule": result.reason()});
            resp.as_object_mut()
                .unwrap()
                .extend(ctx_json.as_object().unwrap().clone());
            Ok(resp)
        }
    }
}
