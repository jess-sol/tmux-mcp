use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Parser)]
#[command(name = "tmux-mcp")]
#[command(about = "MCP server for interacting with tmux")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run as background daemon for a tmux session (internal, spawned automatically)
    Daemon {
        /// Session target (name or ID, e.g. "main" or "$0")
        session: String,
    },
    /// Policy hook for Claude Code (PreToolUse hook for command_run)
    PolicyCheck,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::PolicyCheck) => {
            if let Err(e) = run_policy_check().await {
                eprintln!("{}", e);
                std::process::exit(2);
            }
        }
        Some(Commands::Daemon { session }) => {
            // Daemon mode: logs to stderr
            tracing_subscriber::registry()
                .with(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .init();

            if let Err(e) = tmux_mcp::daemon::run(&session, None).await {
                tracing::error!("Daemon error: {}", e);
                std::process::exit(1);
            }
        }
        None => {
            // MCP server mode: logs to stderr, MCP protocol on stdio
            tracing_subscriber::registry()
                .with(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("warn")),
                )
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .init();

            if let Err(e) = run_mcp().await {
                tracing::error!("MCP server error: {}", e);
                std::process::exit(1);
            }
        }
    }
}

async fn run_policy_check() -> Result<(), Box<dyn std::error::Error>> {
    let input: serde_json::Value = serde_json::from_reader(std::io::stdin())?;
    let tool_input = &input["tool_input"];

    let pane_id = tool_input["pane_id"].as_str().unwrap_or("");
    let command = tool_input["command"].as_str().unwrap_or("");

    if pane_id.is_empty() || command.is_empty() {
        print_hook_response("allow", None); // let daemon handle validation
        return Ok(());
    }

    let session = tmux_mcp::client::discover_session()?;
    let origin_pane = std::env::var("TMUX_PANE").unwrap_or_default();
    let mut client = tmux_mcp::client::DaemonClient::connect(&session).await?;

    let result = client
        .request(
            "request_approval",
            serde_json::json!({
                "origin_pane": origin_pane,
                "pane_id": pane_id,
                "command": command,
            }),
        )
        .await?;

    let decision = result["result"].as_str().unwrap_or("ask");
    let rule = result["rule"].as_str().unwrap_or("");

    let hostname = result["hostname"].as_str().unwrap_or("local");
    let cwd = result["cwd"].as_str().unwrap_or("?");
    let user = result["user"].as_str().unwrap_or("?");
    let fg = result["foreground"].as_str().unwrap_or("?");

    match decision {
        "allow" => print_hook_response("allow", None),
        "lint" => {
            // JSON deny with the lint message — Claude sees the reason and can
            // self-correct, but the user is never prompted for approval.
            let msg = result["message"].as_str().unwrap_or("lint error");
            print_hook_response("deny", Some(msg));
        }
        "deny" => {
            eprintln!(
                "Policy denied: {}\n  pane {}  host: {}  cwd: {}  rule: {}",
                command, pane_id, hostname, cwd, rule
            );
            std::process::exit(2);
        }
        _ => {
            let reason = format!(
                "{}\n  pane {}  user: {}  host: {}  cwd: {}  shell: {}  rule: {}",
                command, pane_id, user, hostname, cwd, fg, rule
            );
            print_hook_response("ask", Some(&reason));
        }
    }
    Ok(())
}

fn print_hook_response(decision: &str, reason: Option<&str>) {
    let mut hook = serde_json::json!({
        "hookEventName": "PreToolUse",
        "permissionDecision": decision,
    });
    if let Some(r) = reason {
        hook["permissionDecisionReason"] = serde_json::json!(r);
    }
    println!("{}", serde_json::json!({"hookSpecificOutput": hook}));
}

async fn run_mcp() -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::ServiceExt;
    use rmcp::transport::stdio;

    let session = tmux_mcp::client::discover_session()
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    let origin_pane = std::env::var("TMUX_PANE")
        .map_err(|_| -> Box<dyn std::error::Error> {
            "TMUX_PANE not set — are you inside a tmux session?".into()
        })?;

    tracing::info!("Connecting to daemon for session {}, origin pane {}", session, origin_pane);
    let client = tmux_mcp::client::DaemonClient::connect(&session).await?;

    let server = tmux_mcp::mcp::TmuxMcp::new(client, origin_pane);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
