use clap::{Parser, Subcommand};
use tmux_mcp::client::TmuxSession;
use tmux_mcp::policy;
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
    /// Run as background daemon for the tmux session in $TMUX (internal, spawned automatically)
    Daemon,
    /// Policy hook for Claude Code (PreToolUse hook for Bash and command_run)
    PolicyCheck,
    /// Show the decision and reasons the policy gives each line, as from a pane
    /// and from Claude Code's Bash tool in `--cwd`. Needs no daemon, and stores
    /// no approval
    PolicyEval {
        /// Judge only this channel
        #[arg(long, value_parser = ["tmux", "bash"])]
        via: Option<String>,
        /// The directory the lines run in (default: the current one)
        #[arg(long)]
        cwd: Option<String>,
        /// Print one JSON object per line and channel: line, via, decision,
        /// and reasons, each starting with its rule's id
        #[arg(long)]
        json: bool,
        /// The command lines to judge
        #[arg(required = true)]
        lines: Vec<String>,
    },
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
        Some(Commands::PolicyEval {
            via,
            cwd,
            json,
            lines,
        }) => {
            if let Err(e) = run_policy_eval(&lines, via.as_deref(), cwd, json) {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
        Some(Commands::Daemon) => {
            // Daemon mode: logs to stderr
            tracing_subscriber::registry()
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .init();

            if let Err(e) = run_daemon().await {
                tracing::error!("Daemon error: {}", e);
                std::process::exit(1);
            }
        }
        None => {
            // MCP server mode: logs to stderr, MCP protocol on stdio
            tracing_subscriber::registry()
                .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .init();

            if let Err(e) = run_mcp().await {
                tracing::error!("MCP server error: {}", e);
                std::process::exit(1);
            }
        }
    }
}

/// The `PreToolUse` hook. A `command_run` call is judged by the daemon; a
/// `Bash` call here, so it needs neither tmux nor the daemon. Any other tool,
/// and a *Passthrough*, get no output: Claude Code's own permissions decide.
async fn run_policy_check() -> Result<(), Box<dyn std::error::Error>> {
    let input: serde_json::Value = serde_json::from_reader(std::io::stdin())?;
    let tool_name = input["tool_name"].as_str().unwrap_or("");
    if tool_name == "Bash" {
        check_bash(&input);
        Ok(())
    } else if tool_name.ends_with("__command_run") {
        check_command_run(&input["tool_input"]).await
    } else {
        Ok(())
    }
}

/// A Bash call starts from the payload's `cwd`.
fn check_bash(input: &serde_json::Value) {
    let command = input["tool_input"]["command"].as_str().unwrap_or("");
    let cwd = input["cwd"].as_str().map(str::to_string);
    let engine = policy::PolicyEngine::new(cwd.as_deref());
    let result = policy::evaluate(command, &local_context(cwd), policy::Via::Bash, &engine);
    match result.decision {
        policy::Decision::Passthrough => {}
        policy::Decision::Allow => print_hook_response("allow", None),
        policy::Decision::Ask => print_hook_response("ask", Some(&result.reason())),
        policy::Decision::Deny => print_hook_response("deny", Some(&result.reason())),
    }
}

/// A shell on this machine, in `cwd`, as the shell's own user.
fn local_context(cwd: Option<String>) -> policy::PaneContext {
    policy::PaneContext {
        hostname: None,
        cwd,
        foreground: None,
        user: std::env::var("USER").ok(),
    }
}

/// Prints each line, then one row per channel: the decision, then its
/// reasons, one per line. With `json`, one object per line and channel.
fn run_policy_eval(
    lines: &[String],
    via: Option<&str>,
    cwd: Option<String>,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?.to_string_lossy().into_owned(),
    };
    let engine = policy::PolicyEngine::new(Some(&cwd));
    let ctx = local_context(Some(cwd));
    let channels = [("tmux", policy::Via::Tmux), ("bash", policy::Via::Bash)];
    for line in lines {
        if !json {
            println!("{line}");
        }
        for (name, channel) in channels.iter().filter(|(n, _)| via.is_none_or(|v| v == *n)) {
            let result = policy::evaluate(line, &ctx, *channel, &engine);
            let decision = match result.decision {
                policy::Decision::Allow => "allow",
                policy::Decision::Passthrough => "passthrough",
                policy::Decision::Ask => "ask",
                policy::Decision::Deny => "deny",
            };
            if json {
                let row = serde_json::json!({
                    "line": line,
                    "via": name,
                    "decision": decision,
                    "reasons": result.reasons,
                });
                println!("{row}");
                continue;
            }
            let reason = result.reason();
            let mut reasons = reason.lines();
            let row = format!(
                "  {name:<5} {decision:<12} {}",
                reasons.next().unwrap_or("")
            );
            println!("{}", row.trim_end());
            for reason in reasons {
                println!("{}", format!("{:21}{reason}", "").trim_end());
            }
        }
    }
    Ok(())
}

async fn check_command_run(
    tool_input: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let pane_id = tool_input["pane_id"].as_str().unwrap_or("");
    let command = tool_input["command"].as_str().unwrap_or("");

    let session = TmuxSession::from_env()?;
    let origin_pane = std::env::var("TMUX_PANE").unwrap_or_default();
    let client = tmux_mcp::client::DaemonClient::connect(&session).await?;

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
        // Claude reads the reason, which says how to fix the command; the
        // user is not asked.
        "deny" => print_hook_response("deny", Some(rule)),
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

async fn run_daemon() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let session = TmuxSession::from_env()?;
    tmux_mcp::daemon::run(&session).await
}

async fn run_mcp() -> Result<(), Box<dyn std::error::Error>> {
    use rmcp::ServiceExt;
    use rmcp::transport::stdio;

    let session = TmuxSession::from_env()?;

    let origin_pane = std::env::var("TMUX_PANE").map_err(|_| -> Box<dyn std::error::Error> {
        "TMUX_PANE not set — are you inside a tmux session?".into()
    })?;

    tracing::info!(
        "Connecting to daemon for session {}, origin pane {}",
        session,
        origin_pane
    );
    let client = tmux_mcp::client::DaemonClient::connect(&session).await?;

    let server = tmux_mcp::mcp::TmuxMcp::new(client, origin_pane);
    let service = server.serve(stdio()).await?;
    service.waiting().await?;

    Ok(())
}
