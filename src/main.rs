use clap::{Parser, Subcommand};
use tmux_mcp::client::TmuxSession;
use tmux_mcp::harness::{self, Harness};
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
    /// and the Pi permission extension.
    ///
    /// The host is inferred from the environment; use `--claude` or `--pi` to
    /// force a specific output contract.
    PolicyCheck {
        /// Force the neutral Pi JSON decision object.
        #[arg(long, conflicts_with = "claude")]
        pi: bool,
        /// Force the Claude Code `PreToolUse` hook JSON envelope.
        #[arg(long, conflicts_with = "pi")]
        claude: bool,
    },
    /// Print a safe pane to target with command_run.
    ///
    /// Returns an existing pane other than the origin pane, or creates a scratch
    /// pane (armed with shell integration) when the origin pane is the only one.
    EnsureTarget {
        /// Only report an existing pane; never create one.
        #[arg(long)]
        no_create: bool,
        /// Emit a JSON object instead of just the pane id.
        #[arg(long)]
        json: bool,
    },
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
        Some(Commands::PolicyCheck { pi, claude }) => {
            let override_harness = match (pi, claude) {
                (true, _) => Some(Harness::Pi),
                (_, true) => Some(Harness::Claude),
                _ => None,
            };
            let harness = harness::detect_with(override_harness);
            if let Err(e) = run_policy_check(harness).await {
                eprintln!("{}", e);
                std::process::exit(2);
            }
        }
        Some(Commands::EnsureTarget { no_create, json }) => {
            if let Err(e) = run_ensure_target(no_create, json).await {
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

/// The permission hook. Under Claude Code (the `PreToolUse` contract) a
/// `command_run` call is judged by the daemon and a `Bash` call here, so it
/// needs neither tmux nor the daemon; any other tool, and a *Passthrough*, get
/// no output: Claude Code's own permissions decide. Under Pi, a bare
/// `tool_input` remains a `command_run` check for backwards compatibility;
/// `tool_name: "bash"` selects local Bash evaluation and neutral JSON.
async fn run_policy_check(harness: Harness) -> Result<(), Box<dyn std::error::Error>> {
    let input: serde_json::Value = serde_json::from_reader(std::io::stdin())?;
    if harness == Harness::Pi {
        if matches!(input["tool_name"].as_str(), Some("bash" | "Bash")) {
            check_bash(&input, harness);
            return Ok(());
        }
        return check_command_run_pi(&input["tool_input"]).await;
    }
    let tool_name = input["tool_name"].as_str().unwrap_or("");
    if tool_name == "Bash" {
        check_bash(&input, harness);
        Ok(())
    } else if tool_name.ends_with("__command_run") {
        check_command_run(&input["tool_input"]).await
    } else {
        Ok(())
    }
}

/// A Bash call starts from the payload's `cwd`.
fn check_bash(input: &serde_json::Value, harness: Harness) {
    let command = input["tool_input"]["command"].as_str().unwrap_or("");
    let cwd = input["cwd"].as_str().map(str::to_string).or_else(|| {
        (harness == Harness::Pi)
            .then(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().into_owned())
            })
            .flatten()
    });
    let engine = policy::PolicyEngine::new(cwd.as_deref());
    let ctx = local_context(cwd);
    let result = policy::evaluate(command, &ctx, policy::Via::Bash, &engine);
    if harness == Harness::Pi {
        let decision = match result.decision {
            policy::Decision::Allow => "allow",
            policy::Decision::Passthrough => "passthrough",
            policy::Decision::Ask => "ask",
            policy::Decision::Deny => "deny",
        };
        println!(
            "{}",
            serde_json::json!({
                "decision": decision,
                "rule": result.reason(),
                "reasons": result.reasons,
                "context": { "cwd": ctx.cwd, "user": ctx.user, "hostname": ctx.hostname,
                             "foreground": null },
            })
        );
        return;
    }
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

/// The Pi contract: judge the extension's bare `tool_input` and answer with
/// the neutral decision object.
async fn check_command_run_pi(
    tool_input: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error>> {
    let pane_id = tool_input["pane_id"].as_str().unwrap_or("");
    let command = tool_input["command"].as_str().unwrap_or("");

    if pane_id.is_empty() || command.is_empty() {
        // Let the daemon handle validation.
        print_pi_response("allow", "", None, None);
        return Ok(());
    }

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
    let message = result["message"].as_str();

    let context = serde_json::json!({
        "hostname": result["hostname"].as_str().unwrap_or("local"),
        "cwd": result["cwd"].as_str().unwrap_or("?"),
        "user": result["user"].as_str().unwrap_or("?"),
        "foreground": result["foreground"].as_str().unwrap_or("?"),
    });
    print_pi_response(decision, rule, message, Some(context));
    Ok(())
}

/// Emit a neutral decision object for the Pi extension and other non-Claude callers.
fn print_pi_response(
    decision: &str,
    rule: &str,
    message: Option<&str>,
    context: Option<serde_json::Value>,
) {
    let mut resp = serde_json::json!({
        "decision": decision,
        "rule": rule,
    });
    if let Some(msg) = message {
        resp["message"] = serde_json::json!(msg);
    }
    if let Some(ctx) = context {
        resp["context"] = ctx;
    }
    println!("{}", resp);
}

async fn run_ensure_target(
    no_create: bool,
    as_json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let session = TmuxSession::from_env()?;
    let origin_pane = std::env::var("TMUX_PANE").map_err(|_| -> Box<dyn std::error::Error> {
        "TMUX_PANE not set — are you inside a tmux session?".into()
    })?;

    let client = tmux_mcp::client::DaemonClient::connect(&session).await?;
    let target = tmux_mcp::target::ensure_target(&client, &origin_pane, !no_create)
        .await
        .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;

    if as_json {
        println!("{}", target.to_json());
    } else {
        println!("{}", target.pane_id);
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
