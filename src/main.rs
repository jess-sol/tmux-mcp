use std::collections::HashMap;

use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use tmux_mcp::pane::processor::PaneProcessor;
use tmux_mcp::parse::escape::unescape_tmux_output;
use tmux_mcp::tmux::connection::RawTmuxConnection;
use tmux_mcp::tmux::notification::Notification;

#[derive(Parser)]
#[command(name = "tmux-mcp")]
#[command(about = "Daemon for tracking tmux pane state")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the daemon (attaches to current tmux session)
    Daemon {
        /// Pane to visualize (e.g., %0). If omitted, shows all panes.
        #[arg(long)]
        watch: Option<String>,
    },
    /// Inject OSC 133 shell integration into a pane
    Inject {
        /// Target pane ID (e.g., %0). Defaults to current pane.
        pane_id: Option<String>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().with_writer(std::io::stderr))
        .init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Daemon { watch } => {
            if let Err(e) = run_daemon(watch).await {
                tracing::error!("Daemon error: {}", e);
                std::process::exit(1);
            }
        }
        Commands::Inject { pane_id } => {
            tracing::info!("Injecting OSC 133 into pane {:?}", pane_id);
            // TODO: wire up injection
        }
    }
}

async fn run_daemon(watch_pane: Option<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = RawTmuxConnection::connect().await?;
    let origin_pane = conn.origin_pane().to_string();

    tracing::info!(
        "Connected to session {} ({}), origin pane {}",
        conn.session_id(),
        conn.session_name(),
        origin_pane,
    );

    // Discover panes in the session
    let session_id = conn.session_id().to_string();
    let pane_list_output = conn.list_panes(&session_id, true).await?;
    tracing::info!("Panes:\n{}", pane_list_output);

    // Parse pane info and create processors
    let mut processors: HashMap<String, PaneProcessor> = HashMap::new();
    for line in pane_list_output.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 9 {
            let pane_id = parts[1].to_string();
            let width: usize = parts[7].parse().unwrap_or(80);
            let height: usize = parts[8].parse().unwrap_or(24);

            // Don't monitor our own pane
            if pane_id == origin_pane {
                tracing::info!("Skipping origin pane {}", pane_id);
                continue;
            }

            tracing::info!(
                "Monitoring pane {} ({}x{}, process: {})",
                pane_id, width, height, parts[5]
            );

            // Enable %output for this pane
            conn.enable_pane_output(&pane_id).await?;

            processors.insert(pane_id, PaneProcessor::new(height, width));
        }
    }

    if processors.is_empty() {
        tracing::warn!("No panes to monitor (only the origin pane exists)");
        tracing::warn!("Open another pane in this tmux session, then restart");
        return Ok(());
    }

    tracing::info!("Monitoring {} panes. Waiting for output...", processors.len());

    // Main loop: receive notifications and process
    loop {
        let Some(notification) = conn.recv_notification().await else {
            tracing::error!("Tmux connection closed");
            break;
        };

        match notification {
            Notification::Output { pane_id, data } => {
                let Some(proc) = processors.get_mut(&pane_id) else {
                    continue;
                };

                // Unescape tmux octal encoding
                let bytes = unescape_tmux_output(&data);

                // Feed through pane processor (OSC splitting + terminal emulation)
                let osc_events = proc.process_chunk(&bytes);

                // Log OSC events
                for event in &osc_events {
                    tracing::debug!("OSC event in {}: {:?}", pane_id, event.event);
                }

                // Visualize the watched pane (or first pane if none specified)
                let should_show = match &watch_pane {
                    Some(wp) => pane_id == *wp,
                    None => true,
                };

                if should_show {
                    print_screen(&pane_id, proc, &osc_events);
                }
            }
            Notification::Exit { reason } => {
                tracing::info!("Tmux exit: {:?}", reason);
                break;
            }
            Notification::SessionClose { session_id } => {
                tracing::info!("Session {} closed", session_id);
                break;
            }
            Notification::WindowClose { window_id } => {
                tracing::info!("Window {} closed", window_id);
            }
            Notification::Other { line } => {
                tracing::trace!("Other notification: {}", line);
            }
        }
    }

    Ok(())
}

fn print_screen(
    pane_id: &str,
    proc: &PaneProcessor,
    osc_events: &[tmux_mcp::pane::osc::OscMatch],
) {
    let (cursor_line, cursor_col) = proc.cursor_position();
    let cols = proc.columns();

    // Clear screen and move to top
    eprint!("\x1b[2J\x1b[H");

    // Header
    eprintln!(
        "\x1b[1m── Pane {} ({}x{}) cursor=({},{}) ──\x1b[0m",
        pane_id,
        cols,
        proc.screen_lines(),
        cursor_line,
        cursor_col,
    );

    // Screen content
    for line_idx in 0..proc.screen_lines() {
        let text = proc.screen_line_text(line_idx);
        if line_idx == cursor_line {
            // Highlight cursor line
            eprintln!("\x1b[7m{:<width$}\x1b[0m", text, width = cols);
        } else if !text.is_empty() {
            eprintln!("{}", text);
        } else {
            eprintln!();
        }
    }

    // Footer with OSC events
    if !osc_events.is_empty() {
        eprintln!(
            "\x1b[2m── OSC: {} ──\x1b[0m",
            osc_events
                .iter()
                .map(|e| format!("{:?}", e.event))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}
