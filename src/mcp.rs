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
    #[schemars(description = "Show next N lines from cursor (advances cursor). Returns as soon as N lines are available — with `search`, as soon as N matches accumulate — and the command keeps running. Mutually exclusive with head/tail.")]
    pub next: Option<i64>,
    #[schemars(description = "Show first N lines. Returns as soon as the first N lines are available — with `search`, as soon as N matches accumulate — and the command keeps running. Mutually exclusive with next/tail.")]
    pub head: Option<i64>,
    #[schemars(description = "Show last N lines. Waits for the command to complete (or timeout) before returning. Mutually exclusive with next/head.")]
    pub tail: Option<i64>,
    #[schemars(description = "Filter output to lines matching this regex pattern. Applied after next/head/tail windowing. With next, non-matching lines are still consumed from the cursor. Use standard regex with normal JSON string escaping — one backslash in the JSON string reaches the regex engine as-is. Examples: \"error|warn\" (alternation), \"plan\\.metrics\" (literal dot), \"result\\?\" (literal question mark), \"exit code: \\d+\" (digit class).")]
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
    #[schemars(description = "Show next N lines from cursor (advances cursor). Returns as soon as N lines are available — with `search`, as soon as N matches accumulate — and the command keeps running. Mutually exclusive with head/tail.")]
    pub next: Option<i64>,
    #[schemars(description = "Show first N lines. Returns as soon as the first N lines are available — with `search`, as soon as N matches accumulate — and the command keeps running. Mutually exclusive with next/tail.")]
    pub head: Option<i64>,
    #[schemars(description = "Show last N lines. Waits for the command to complete (or timeout) before returning. Mutually exclusive with next/head.")]
    pub tail: Option<i64>,
    #[schemars(description = "Filter output to lines matching this regex pattern. Applied after next/head/tail windowing. With next, non-matching lines are still consumed from the cursor. Use standard regex with normal JSON string escaping — one backslash in the JSON string reaches the regex engine as-is. Examples: \"error|warn\" (alternation), \"plan\\.metrics\" (literal dot), \"result\\?\" (literal question mark), \"exit code: \\d+\" (digit class).")]
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

/// Format list_panes result as clean JSON with relative coordinates and short keys.
fn format_list_panes(result: &serde_json::Value, origin_pane: &str) -> String {
    use serde_json::{Map, Value};

    let panes = match result["panes"].as_array() {
        Some(arr) => arr,
        None => return "(no panes)".to_string(),
    };

    // Origin pane position used as coordinate reference point.
    let (ox, oy) = panes
        .iter()
        .find(|p| p["pane_id"].as_str() == Some(origin_pane))
        .map(|p| (p["x"].as_i64().unwrap_or(0), p["y"].as_i64().unwrap_or(0)))
        .unwrap_or((0, 0));

    let pane_values: Vec<Value> = panes
        .iter()
        .map(|p| {
            let pane_id = p["pane_id"].as_str().unwrap_or("?");

            // Merge cwd: prefer osc_cwd (works across SSH), fall back to process_cwd
            let cwd = p["osc_cwd"]
                .as_str()
                .or_else(|| p["process_cwd"].as_str())
                .map(|path| match (p["osc_user"].as_str(), p["osc_hostname"].as_str()) {
                    (Some(user), Some(host)) => format!("{}@{}:{}", user, host, path),
                    (None, Some(host)) => format!("{}:{}", host, path),
                    _ => path.to_string(),
                });

            let mut obj = Map::new();
            obj.insert("id".into(), json!(pane_id));
            if pane_id == origin_pane {
                obj.insert("origin".into(), json!(true));
            }
            obj.insert("x".into(), json!(p["x"].as_i64().unwrap_or(0) - ox));
            obj.insert("y".into(), json!(p["y"].as_i64().unwrap_or(0) - oy));
            obj.insert("w".into(), json!(p["width"].as_u64().unwrap_or(0)));
            obj.insert("h".into(), json!(p["height"].as_u64().unwrap_or(0)));
            if let Some(cwd) = cwd {
                obj.insert("cwd".into(), json!(cwd));
            }
            if let Some(fg) = p["foreground"].as_str() {
                obj.insert("fg".into(), json!(fg));
            }
            obj.insert(
                "activity".into(),
                json!(p["activity"].as_str().unwrap_or("Unknown")),
            );
            obj.insert(
                "shell_integration".into(),
                json!(p["osc133_status"].as_str().unwrap_or("unknown")),
            );
            if let Some(s) = p["osc133_last_marker_secs"].as_f64() {
                obj.insert("marker_age".into(), json!((s * 10.0).round() / 10.0));
            }
            if let Some(s) = p["last_data_secs"].as_f64() {
                obj.insert("data_age".into(), json!((s * 10.0).round() / 10.0));
            }

            Value::Object(obj)
        })
        .collect();

    let mut top = Map::new();
    if let Some(u) = result["daemon_uptime_secs"].as_f64() {
        top.insert("uptime".into(), json!((u).round()));
    }
    top.insert("panes".into(), Value::Array(pane_values));

    serde_json::to_string_pretty(&Value::Object(top))
        .unwrap_or_else(|_| "(error formatting panes)".into())
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
                 command_run: Combine with next=N + search=\"regex\" to return as soon \
                 as N matching lines appear, leaving the command running. Reach for \
                 this any time you'd otherwise set timeout_secs past ~30s and the \
                 command emits a recognizable progress marker (server ready line, \
                 build artifact written, test summary). In search mode, size \
                 timeout_secs to when the match should appear if things go well, not \
                 to total command runtime — if the regex is wrong you fail fast, then \
                 use command_read to inspect what actually printed and retry with a \
                 better pattern. Example for a long-running server: \
                 command_run(pane_id=%X, command=\"python serve.py\", next=1, \
                 search=\"listen|ready|serving\", timeout_secs=60) returns on the first \
                 match, then run dependent commands while the server keeps going. \
                 head=N + search behaves the same over the first N matches. \
                 Without next/head, the call blocks until the command completes or \
                 timeout_secs elapses; on timeout the command keeps running — continue \
                 with command_read(command_id=N). \
                 Use timeout_secs=0 for commands that change shell state \
                 (sudo -i, ssh host, exit). \
                 If a pane is busy (RPC error), use command_read with timeout_secs \
                 to wait for output, or press_key C-c to cancel. \
                 Build separately from run — build output drowns run output.\n\n\
                 command_read: Stream output from running or completed commands. Use next \
                 to advance through output (advances cursor), head/tail to view ranges, \
                 search to filter by regex with before/after for context. \
                 next/head + search waits up to timeout_secs and returns as soon as N \
                 matches accumulate — don't poll in a loop.\n\n\
                 command_history: Lists recent commands with their command_id, exit code, \
                 and output line count. Essential for finding command IDs to revisit \
                 output from earlier commands via command_read(command_id=N).\n\n\
                 debug_pane: Shows only visible terminal text — no scrollback, no structure. \
                 NOT for reading command output (use command_read instead). Use only to \
                 debug pane state or inspect TUI apps.\n\n\
                 press_key: For control sequences only (C-c, C-d, Enter, Escape). \
                 NOT for running commands — use command_run instead."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that the `search` field description in the JSON schema contains
    /// single-backslash regex examples (e.g. `plan\.metrics`), not double-escaped
    /// ones. The Rust string `"\\."` becomes `\.` in the generated schema JSON,
    /// which after JSON parsing gives the AI the literal `\.` to use.
    #[test]
    fn search_schema_has_single_backslash_examples() {
        let schema = schemars::schema_for!(CommandReadParams);
        let json = serde_json::to_string(&schema).unwrap();

        // In the JSON wire format, a single regex backslash is encoded as \\
        // So `plan\.metrics` in the description appears as `plan\\.metrics` in JSON text
        assert!(
            json.contains(r#"plan\\.metrics"#),
            "expected plan\\.metrics (literal dot) in schema JSON, got: {json}"
        );
        assert!(
            json.contains(r#"result\\?"#),
            "expected result\\? (literal question mark) in schema JSON, got: {json}"
        );
        assert!(
            json.contains(r#"\\d+"#),
            "expected \\d+ (digit class) in schema JSON, got: {json}"
        );

        // Must NOT contain triple or quadruple backslashes for these patterns
        assert!(
            !json.contains(r#"plan\\\\.metrics"#),
            "found over-escaped plan\\\\.metrics in schema JSON"
        );
        assert!(
            !json.contains(r#"result\\\\?"#),
            "found over-escaped result\\\\? in schema JSON"
        );
    }
}
