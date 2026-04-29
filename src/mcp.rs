/// MCP server: exposes tmux tools over stdio using rmcp.
/// Forwards tool calls to the daemon over the Unix socket client.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{ServerHandler, schemars, tool, tool_router};
use serde_json::json;
use tokio::sync::Mutex;

use crate::client::DaemonClient;

#[derive(Clone)]
pub struct TmuxMcp {
    client: Arc<Mutex<DaemonClient>>,
    tool_router: ToolRouter<Self>,
}

impl TmuxMcp {
    pub fn new(client: DaemonClient) -> Self {
        Self {
            client: Arc::new(Mutex::new(client)),
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

// --- Tool implementations ---

#[tool_router]
impl TmuxMcp {
    #[tool(description = "List all tmux panes with their status, working directory, and running process")]
    async fn list_panes(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        let mut client = self.client.lock().await;
        let result = client
            .request("list_panes", json!({}))
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
                    "pane_id": params.pane_id,
                    "command": params.command,
                    "timeout_secs": params.timeout_secs.unwrap_or(30),
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
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
                    "pane_id": params.pane_id,
                    "count": params.count.unwrap_or(1),
                }),
            )
            .await
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;

        let text = serde_json::to_string_pretty(&result)
            .map_err(|e| rmcp::ErrorData::internal_error(e.to_string(), None))?;
        Ok(CallToolResult::success(vec![Content::text(text)]))
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
