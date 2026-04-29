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
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandReadParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(description = "Number of recent commands to read (default 1)")]
    pub count: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommandHistoryParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(description = "Number of history entries (default 10)")]
    pub count: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CapturePaneParams {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
    #[schemars(description = "Number of lines from bottom of screen (default 50, max 1000)")]
    pub lines: Option<u64>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct InjectOsc133Params {
    #[schemars(description = "Target pane ID (e.g. \"%0\")")]
    pub pane_id: String,
}

// --- Tool implementations ---

#[tool_router]
impl TmuxMcp {
    #[tool(description = "List all tmux panes with their status, working directory, and running process")]
    async fn list_panes(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request("list_panes", json!({"origin_pane": self.origin_pane}))
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Run a shell command in a pane and wait for it to complete. Returns the command output and exit code.")]
    async fn command_run(
        &self,
        Parameters(params): Parameters<CommandRunParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request(
                "command_run",
                json!({
                    "origin_pane": self.origin_pane,
                    "pane_id": params.pane_id,
                    "command": params.command,
                    "timeout_secs": params.timeout_secs.unwrap_or(30),
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let output = result["output"].as_str().unwrap_or("");
        let exit_code = result["exit_code"].as_i64();

        let text = match exit_code {
            Some(0) | None => output.to_string(),
            Some(code) => format!("{}\n\nExit code: {}", output, code),
        };

        let is_error = exit_code.is_some_and(|c| c != 0);
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        if is_error {
            result.is_error = Some(true);
        }
        Ok(result)
    }

    #[tool(description = "Read the output of recent commands in a pane")]
    async fn command_read(
        &self,
        Parameters(params): Parameters<CommandReadParams>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request(
                "command_read",
                json!({
                    "origin_pane": self.origin_pane,
                    "pane_id": params.pane_id,
                    "count": params.count.unwrap_or(1),
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        // Format each command's output as plain text
        let mut text = String::new();
        if let Some(cmds) = result.as_array() {
            for cmd in cmds {
                let command = cmd["command"].as_str().unwrap_or("");
                let output = cmd["output"].as_str().unwrap_or("");
                let exit_code = cmd["exit_code"].as_i64();

                text.push_str(&format!("$ {}\n", command));
                if !output.is_empty() {
                    text.push_str(output);
                    if !output.ends_with('\n') {
                        text.push('\n');
                    }
                }
                if let Some(code) = exit_code {
                    if code != 0 {
                        text.push_str(&format!("Exit code: {}\n", code));
                    }
                }
            }
        }

        Ok(CallToolResult::success(vec![Content::text(text.trim_end().to_string())]))
    }

    #[tool(description = "Capture visible text from a pane's screen buffer. Works regardless of what's running in the pane.")]
    async fn capture_pane(
        &self,
        Parameters(params): Parameters<CapturePaneParams>,
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

        let text = result["text"].as_str().unwrap_or("").to_string();
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    #[tool(description = "Inject OSC 133 shell integration into a bash pane. Use after SSH or when markers aren't present. Only works with bash.")]
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
        let text = if status == "active" {
            "OSC 133 injection successful — shell integration active.".to_string()
        } else {
            let msg = result["message"].as_str().unwrap_or("Injection may have failed");
            format!("OSC 133 injection status: {} — {}", status, msg)
        };

        let is_error = status != "active";
        let mut result = CallToolResult::success(vec![Content::text(text)]);
        if is_error {
            result.is_error = Some(true);
        }
        Ok(result)
    }

    #[tool(description = "List command history for a pane, showing commands and their exit codes")]
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

        let text = serde_json::to_string_pretty(&result)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for TmuxMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "MCP server for interacting with tmux panes. \
                 Use list_panes to discover available panes, then run \
                 commands and read their output."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}
