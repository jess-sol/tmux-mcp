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
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Daemon { session }) => {
            // Daemon mode: logs to stderr
            tracing_subscriber::registry()
                .with(
                    EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| EnvFilter::new("info")),
                )
                .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
                .init();

            if let Err(e) = tmux_mcp::daemon::run(&session).await {
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
