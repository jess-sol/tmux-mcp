/// MCP server: exposes tmux tools over stdio using rmcp.
/// Forwards tool calls to the daemon over the Unix socket client.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, schemars, tool, tool_handler, tool_router};
use serde_json::json;
use tokio::sync::Mutex;

use crate::client::DaemonClient;

#[derive(Clone)]
pub struct TmuxMcp {
    client: Arc<Mutex<DaemonClient>>,
    origin_pane: String,
    tool_router: ToolRouter<Self>,
}

impl TmuxMcp {
    pub fn new(client: DaemonClient, origin_pane: String) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
            origin_pane,
            tool_router: Self::tool_router(),
        }
    }
}

// --- Tool parameter types ---

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandRunParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(description = "Shell command to execute")]
    pub command: String,
    #[schemars(description = "Timeout in seconds (default 30)")]
    pub timeout_secs: Option<i64>,
    #[schemars(description = "Show next N lines from cursor (advances cursor). Mutually exclusive with head/tail.")]
    pub next: Option<i64>,
    #[schemars(description = "Show first N lines. Mutually exclusive with next/tail.")]
    pub head: Option<i64>,
    #[schemars(description = "Show last N lines. Mutually exclusive with next/head.")]
    pub tail: Option<i64>,
    #[schemars(description = "Filter output to lines matching this regex pattern. Applied after next/head/tail windowing. With next, non-matching lines are still consumed from the cursor.")]
    pub search: Option<String>,
    #[schemars(description = "Lines of context before each search match (like grep -B). Requires search.")]
    pub before: Option<i64>,
    #[schemars(description = "Lines of context after each search match (like grep -A). Requires search.")]
    pub after: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandReadParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(description = "Command ID to read (default: current/last command)")]
    pub command_id: Option<i64>,
    #[schemars(description = "Timeout in seconds — how long to wait for new output (default 5)")]
    pub timeout_secs: Option<i64>,
    #[schemars(description = "Show next N lines from cursor (advances cursor). Mutually exclusive with head/tail.")]
    pub next: Option<i64>,
    #[schemars(description = "Show first N lines. Mutually exclusive with next/tail.")]
    pub head: Option<i64>,
    #[schemars(description = "Show last N lines. Mutually exclusive with next/head.")]
    pub tail: Option<i64>,
    #[schemars(description = "Filter output to lines matching this regex pattern. Applied after next/head/tail windowing. With next, non-matching lines are still consumed from the cursor.")]
    pub search: Option<String>,
    #[schemars(description = "Lines of context before each search match (like grep -B). Requires search.")]
    pub before: Option<i64>,
    #[schemars(description = "Lines of context after each search match (like grep -A). Requires search.")]
    pub after: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandHistoryParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(description = "Number of history entries (default 10)")]
    pub count: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct DebugPaneParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(description = "Number of lines from bottom of screen (default 50, max 1000)")]
    pub lines: Option<i64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct InjectOsc133Params {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct PressKeyParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(
        description = "Key to press. Use tmux key names: Enter, C-c, C-z, C-d, Escape, Tab, Space, Up, Down, Left, Right, BSpace. Max 5 chars."
    )]
    pub keys: String,
}

// --- Response formatting ---

/// Format list_panes result as a compact aligned table.
fn format_list_panes(result: &serde_json::Value, origin_pane: &str) -> String {
    let panes = match result["panes"].as_array() {
        Some(arr) => arr,
        None => return "(no panes)".to_string(),
    };

    // Format seconds compactly: "0.3s", "55.9s", or "-" for null
    fn fmt_secs(v: &serde_json::Value) -> String {
        match v.as_f64() {
            Some(s) => format!("{:.1}s", s),
            None => "-".to_string(),
        }
    }

    // Build row data
    struct Row {
        pane: String,
        pid: String,
        geometry: String,
        cwd: String,
        foreground: String,
        activity: String,
        osc133: String,
        osc133_marker: String,
        last_data: String,
    }
    let rows: Vec<Row> = panes
        .iter()
        .map(|p| {
            let pane_id = p["pane_id"].as_str().unwrap_or("?");
            let pane = if pane_id == origin_pane {
                format!("{} (us)", pane_id)
            } else {
                pane_id.to_string()
            };

            let x = p["x"].as_u64().unwrap_or(0);
            let y = p["y"].as_u64().unwrap_or(0);
            let w = p["width"].as_u64().unwrap_or(0);
            let h = p["height"].as_u64().unwrap_or(0);
            let geometry = format!("{},{} {}x{}", x, y, w, h);

            // Merge cwd: prefer osc_cwd (works across SSH), fall back to process_cwd
            let base_cwd = p["osc_cwd"]
                .as_str()
                .or_else(|| p["process_cwd"].as_str())
                .unwrap_or("-");
            let cwd = match (p["osc_user"].as_str(), p["osc_hostname"].as_str()) {
                (Some(user), Some(host)) => format!("{}@{}:{}", user, host, base_cwd),
                (None, Some(host)) => format!("{}:{}", host, base_cwd),
                _ => base_cwd.to_string(),
            };

            Row {
                pane,
                pid: p["pid"].as_u64().map(|v| v.to_string()).unwrap_or("-".into()),
                geometry,
                cwd,
                foreground: p["foreground"].as_str().unwrap_or("-").to_string(),
                activity: p["activity"].as_str().unwrap_or("?").to_string(),
                osc133: p["osc133_status"].as_str().unwrap_or("?").to_string(),
                osc133_marker: fmt_secs(&p["osc133_last_marker_secs"]),
                last_data: fmt_secs(&p["last_data_secs"]),
            }
        })
        .collect();

    // Compute column widths (all but last column get padded)
    let headers = [
        "Pane", "PID", "Geometry", "CWD", "Foreground", "Activity",
        "OSC133", "LastMarker", "LastData",
    ];
    let mut w: Vec<usize> = headers.iter().map(|h| h.len()).collect();
    for r in &rows {
        w[0] = w[0].max(r.pane.len());
        w[1] = w[1].max(r.pid.len());
        w[2] = w[2].max(r.geometry.len());
        w[3] = w[3].max(r.cwd.len());
        w[4] = w[4].max(r.foreground.len());
        w[5] = w[5].max(r.activity.len());
        w[6] = w[6].max(r.osc133.len());
        w[7] = w[7].max(r.osc133_marker.len());
        w[8] = w[8].max(r.last_data.len());
    }

    let mut out = String::new();
    // Header
    out.push_str(&format!(
        "{:<w0$}  {:>w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}  {:>w7$}  {}\n",
        headers[0], headers[1], headers[2], headers[3], headers[4],
        headers[5], headers[6], headers[7], headers[8],
        w0 = w[0], w1 = w[1], w2 = w[2], w3 = w[3], w4 = w[4],
        w5 = w[5], w6 = w[6], w7 = w[7],
    ));
    // Rows
    for r in &rows {
        out.push_str(&format!(
            "{:<w0$}  {:>w1$}  {:<w2$}  {:<w3$}  {:<w4$}  {:<w5$}  {:<w6$}  {:>w7$}  {}\n",
            r.pane, r.pid, r.geometry, r.cwd, r.foreground,
            r.activity, r.osc133, r.osc133_marker, r.last_data,
            w0 = w[0], w1 = w[1], w2 = w[2], w3 = w[3], w4 = w[4],
            w5 = w[5], w6 = w[6], w7 = w[7],
        ));
    }

    // Footer
    if let Some(uptime) = result["daemon_uptime_secs"].as_f64() {
        out.push_str(&format!("\nDaemon uptime: {:.0}s", uptime));
    }

    out.trim_end().to_string()
}

/// Format command_history result as a compact aligned table.
fn format_command_history(result: &serde_json::Value) -> String {
    let entries = match result.as_array() {
        Some(arr) => arr,
        None => return "(no history)".to_string(),
    };
    if entries.is_empty() {
        return "(no history)".to_string();
    }

    // Collect formatted values to compute widths
    struct Row {
        id: String,
        exit: String,
        lines: String,
        command: String,
    }
    let rows: Vec<Row> = entries
        .iter()
        .map(|e| Row {
            id: e["command_id"].as_u64().map(|v| v.to_string()).unwrap_or("-".into()),
            exit: e["exit_code"]
                .as_i64()
                .map(|v| v.to_string())
                .unwrap_or("-".into()),
            lines: e["output_lines"].as_u64().map(|v| v.to_string()).unwrap_or("-".into()),
            command: e["command"].as_str().unwrap_or("").to_string(),
        })
        .collect();

    let headers = ["ID", "Exit", "Lines", "Command"];
    let mut w = [headers[0].len(), headers[1].len(), headers[2].len()];
    for r in &rows {
        w[0] = w[0].max(r.id.len());
        w[1] = w[1].max(r.exit.len());
        w[2] = w[2].max(r.lines.len());
    }

    let mut out = String::new();
    out.push_str(&format!(
        "{:>w0$}  {:>w1$}  {:>w2$}  {}\n",
        headers[0], headers[1], headers[2], headers[3],
        w0 = w[0], w1 = w[1], w2 = w[2],
    ));
    for r in &rows {
        out.push_str(&format!(
            "{:>w0$}  {:>w1$}  {:>w2$}  {}\n",
            r.id, r.exit, r.lines, r.command,
            w0 = w[0], w1 = w[1], w2 = w[2],
        ));
    }

    out.trim_end().to_string()
}

/// Format a command_run or command_read result with context hints for the LLM.
fn format_command_result(result: &serde_json::Value, is_run: bool) -> String {
    let status = result["status"].as_str().unwrap_or("unknown");
    let output = result["output"].as_str().unwrap_or("");
    let exit_code = result["exit_code"].as_i64();
    let command_id = result["command_id"].as_u64();
    let total_lines = result["total_lines"].as_u64().unwrap_or(0);
    let command = result["command"].as_str().unwrap_or("");

    let lines_skipped = result["lines_skipped"].as_u64().unwrap_or(0);
    let search_matches = result["search_matches"].as_u64();
    let read_cursor = result["read_cursor"].as_u64().unwrap_or(0);

    let mut text = String::new();

    // Command header for command_run
    if is_run && !command.is_empty() {
        text.push_str(&format!("$ {}\n", command));
    }

    // Skipped lines hint — split into already-read vs not-shown portions
    if lines_skipped > 0 {
        let already_read = if read_cursor > 0 { read_cursor.min(lines_skipped) } else { 0 };
        let not_shown = lines_skipped - already_read;
        if already_read > 0 {
            text.push_str(&format!("<{} lines already read>\n", already_read));
        }
        if not_shown > 0 {
            text.push_str(&format!("<{} lines not shown>\n", not_shown));
        }
    }

    // Output
    if !output.is_empty() {
        text.push_str(output);
        if !output.ends_with('\n') {
            text.push('\n');
        }
    }

    // Status footer
    match status {
        "completed" => {
            if let Some(code) = exit_code {
                text.push_str(&format!("\nExit code: {}", code));
            }
            if let Some(matches) = search_matches {
                text.push_str(&format!("\n[{} matches in {} lines.]", matches, total_lines));
            }
            // Nudge toward command_read when command_run output was filtered
            if is_run {
                if let Some(id) = command_id {
                    if search_matches == Some(0) {
                        text.push_str(&format!(
                            "\n[Search had no matches. Use command_read(command_id={}, tail=50) to browse the end, \
                             or command_read(command_id={}, search=\"different pattern\") to try again.]",
                            id, id
                        ));
                    } else if lines_skipped > 0 {
                        text.push_str(&format!(
                            "\n[Output filtered ({} lines total). \
                             Use command_read(command_id={}) with head, tail, or search for different views of the same command output.]",
                            total_lines, id
                        ));
                    }
                }
            }
            if output.is_empty() && !is_run {
                if let Some(id) = command_id {
                    text.push_str(&format!(
                        "\n[Command {} completed. No unread output. Use head, tail, or search to review full output ({} lines).]",
                        id, total_lines
                    ));
                }
            }
        }
        "running" => {
            if let Some(id) = command_id {
                if output.is_empty() {
                    text.push_str(&format!(
                        "\n[No new output. Command still running (id={}). Increase timeout_secs or use press_key C-c to cancel.]",
                        id
                    ));
                } else {
                    let lines_shown = output.lines().count();
                    let mut hint = format!(
                        "\n[Command still running (id={}). {} lines shown, {} total.",
                        id, lines_shown, total_lines
                    );
                    if let Some(matches) = search_matches {
                        hint.push_str(&format!(" {} search matches.", matches));
                    }
                    hint.push_str(&format!(
                        " Use command_read(command_id={}, next=200) to continue reading, or press_key C-c to cancel.]",
                        id
                    ));
                    text.push_str(&hint);
                }
            }
        }
        _ => {}
    }

    text.trim_end().to_string()
}

// --- Tool implementations ---

#[tool_router]
impl TmuxMcp {
    #[tool(description = "List tmux panes with status, working directory, and running process")]
    async fn list_panes(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request("list_panes", json!({"origin_pane": self.origin_pane}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let text = format_list_panes(&result, &self.origin_pane);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Run a shell command in a pane and wait for completion")]
    async fn command_run(
        &self,
        Parameters(params): Parameters<CommandRunParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut rpc_params = json!({
            "origin_pane": self.origin_pane,
            "pane_id": params.pane_id,
            "command": params.command,
            "timeout_secs": params.timeout_secs.unwrap_or(30),
        });
        if let Some(n) = params.next { rpc_params["next"] = json!(n); }
        if let Some(n) = params.head { rpc_params["head"] = json!(n); }
        if let Some(n) = params.tail { rpc_params["tail"] = json!(n); }
        if let Some(ref s) = params.search { rpc_params["search"] = json!(s); }
        if let Some(n) = params.before { rpc_params["before"] = json!(n); }
        if let Some(n) = params.after { rpc_params["after"] = json!(n); }

        let mut client = self.client.lock().await;
        let result = client
            .request("command_run", rpc_params)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let text = format_command_result(&result, true);
        let is_error = result["exit_code"].as_i64().is_some_and(|c| c != 0);
        let mut call_result = CallToolResult::success(vec![Content::text(text)]);
        if is_error {
            call_result.is_error = Some(true);
        }
        Ok(call_result)
    }

    #[tool(description = "Read or stream output from a running or completed command")]
    async fn command_read(
        &self,
        Parameters(params): Parameters<CommandReadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut rpc_params = json!({
            "origin_pane": self.origin_pane,
            "pane_id": params.pane_id,
            "timeout_secs": params.timeout_secs.unwrap_or(5),
        });
        if let Some(id) = params.command_id { rpc_params["command_id"] = json!(id); }
        if let Some(n) = params.next { rpc_params["next"] = json!(n); }
        if let Some(n) = params.head { rpc_params["head"] = json!(n); }
        if let Some(n) = params.tail { rpc_params["tail"] = json!(n); }
        if let Some(ref s) = params.search { rpc_params["search"] = json!(s); }
        if let Some(n) = params.before { rpc_params["before"] = json!(n); }
        if let Some(n) = params.after { rpc_params["after"] = json!(n); }

        let mut client = self.client.lock().await;
        let result = client
            .request("command_read", rpc_params)
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let text = format_command_result(&result, false);
        let is_error = result["exit_code"].as_i64().is_some_and(|c| c != 0);
        let mut call_result = CallToolResult::success(vec![Content::text(text)]);
        if is_error {
            call_result.is_error = Some(true);
        }
        Ok(call_result)
    }

    #[tool(description = "Capture visible terminal screen for debugging pane state")]
    async fn debug_pane(
        &self,
        Parameters(params): Parameters<DebugPaneParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request(
                "capture_pane",
                json!({
                    "origin_pane": self.origin_pane,
                    "pane_id": params.pane_id,
                    "lines": params.lines.unwrap_or(50),
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let mut text = String::new();

        // Steer LLMs toward command_read when a tracked command is active
        if let Some(active) = result["active_command"].as_object() {
            let cmd_id = active["command_id"].as_u64().unwrap_or(0);
            let cmd_text = active["command"].as_str().unwrap_or("");
            text.push_str(&format!(
                "[Command {} ({:?}) is running. Use command_read(command_id={}) \
                 for structured output with cursor tracking.]\n\n",
                cmd_id, cmd_text, cmd_id
            ));
        }

        text.push_str(result["text"].as_str().unwrap_or(""));
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Inject shell integration into a bash pane for command tracking")]
    async fn inject_osc133(
        &self,
        Parameters(params): Parameters<InjectOsc133Params>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request(
                "inject_osc133",
                json!({
                    "origin_pane": self.origin_pane,
                    "pane_id": params.pane_id,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let status = result["status"].as_str().unwrap_or("unknown");
        let text = match status {
            "active" => "OSC 133 injection successful — shell integration active.".to_string(),
            "already_active" => "OSC 133 already active — no injection needed. command_run should work.".to_string(),
            _ => {
                let msg = result["message"].as_str().unwrap_or("Injection may have failed");
                let screen = result["screen"].as_str().unwrap_or("");
                if screen.is_empty() {
                    format!("OSC 133 injection status: {} — {}", status, msg)
                } else {
                    format!("OSC 133 injection status: {} — {}\n\nScreen:\n{}", status, msg, screen)
                }
            }
        };

        let is_error = status == "failed";
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        if is_error {
            result.is_error = Some(true);
        }
        Ok(result)
    }

    #[tool(description = "Send a control key to a pane (C-c, C-d, Enter, Escape, etc.)")]
    async fn press_key(
        &self,
        Parameters(params): Parameters<PressKeyParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request(
                "send_keys",
                json!({
                    "origin_pane": self.origin_pane,
                    "pane_id": params.pane_id,
                    "keys": params.keys,
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let screen = result["screen"].as_str().unwrap_or("").to_string();
        Ok(CallToolResult::success(vec![Content::text(screen)]))
    }

    #[tool(description = "List recent commands and their exit codes for a pane")]
    async fn command_history(
        &self,
        Parameters(params): Parameters<CommandHistoryParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request(
                "command_history",
                json!({
                    "origin_pane": self.origin_pane,
                    "pane_id": params.pane_id,
                    "count": params.count.unwrap_or(10),
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let text = format_command_history(&result);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TmuxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "MCP server for interacting with tmux panes. Use list_panes to discover \
                 available panes, then run commands and read their output.\n\n\
                 command_run: Prefer tail=30 for commands where you only need the result \
                 (builds, tests, installs). Use search to filter verbose output \
                 (e.g. search=\"error|fail|warn\" on test runs); add before/after for \
                 context lines around matches (like grep -B/-A). Use head or next when \
                 exploring unknown output. On timeout, returns partial output — use \
                 command_read(command_id=N) to continue reading. For long-running \
                 servers, use command_read(command_id=N, next=1, \
                 search=\"listen|ready|serving\", timeout_secs=30) to wait for the \
                 ready signal before running dependent commands — never sleep. \
                 Use timeout_secs=0 for commands that change shell state \
                 (sudo -i, ssh host, exit).\n\n\
                 command_read: Use next to stream new output from a running command, \
                 head/tail to view ranges, search to filter by regex. Use before/after \
                 with search for context lines around matches (like grep -B/-A). \
                 For active commands, waits up to timeout_secs for new output.\n\n\
                 command_history: Lists recent commands with their command_id, exit code, \
                 and output line count. Use command_id with command_read to revisit \
                 output from earlier commands.\n\n\
                 debug_pane: Shows only visible terminal text — no scrollback, no structure. \
                 NOT for reading command output (use command_read instead). Use only to \
                 debug pane state or inspect TUI apps.\n\n\
                 press_key: For control sequences only. NOT for running commands — use \
                 command_run instead."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
