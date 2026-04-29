use std::collections::HashMap;
use std::io::Write;

use clap::{Parser, Subcommand};
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

use tmux_mcp::pane::osc::OscEvent;
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
    /// Run the daemon (attaches to current tmux session, monitors all panes)
    Daemon,
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
        Commands::Daemon => {
            if let Err(e) = run_daemon().await {
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

/// Pane metadata parsed from tmux list-panes output.
struct PaneInfo {
    pane_id: String,
    #[allow(dead_code)]
    window_id: String,
    width: usize,
    height: usize,
    process: String,
}

fn parse_pane_list(output: &str) -> Vec<PaneInfo> {
    output
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            if parts.len() >= 9 {
                Some(PaneInfo {
                    window_id: parts[0].to_string(),
                    pane_id: parts[1].to_string(),
                    width: parts[7].parse().unwrap_or(80),
                    height: parts[8].parse().unwrap_or(24),
                    process: parts[5].to_string(),
                })
            } else {
                None
            }
        })
        .collect()
}

async fn run_daemon() -> Result<(), Box<dyn std::error::Error>> {
    let mut conn = RawTmuxConnection::connect().await?;
    let origin_pane = conn.origin_pane().to_string();
    let session_id = conn.session_id().to_string();

    tracing::info!(
        "Connected to session {} ({}), origin pane {}",
        session_id,
        conn.session_name(),
        origin_pane,
    );

    let mut processors: HashMap<String, PaneProcessor> = HashMap::new();

    // Discover and monitor all existing panes
    sync_all_panes(&mut conn, &mut processors, &origin_pane, &session_id).await?;

    let state_path = "/tmp/tmux-mcp-state.txt";
    tracing::info!(
        "Monitoring {} panes. View state: watch -n0.5 cat {}",
        processors.len(),
        state_path,
    );

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

                let bytes = unescape_tmux_output(&data);
                let osc_events = proc.process_chunk(&bytes);

                for event in &osc_events {
                    if let OscEvent::Osc133 { marker, param } = &event.event {
                        tracing::debug!(
                            "{} OSC 133;{}{} | {:?} | cmds={}",
                            pane_id,
                            *marker as char,
                            param.as_ref().map(|p| format!(";{}", p)).unwrap_or_default(),
                            proc.osc133_phase(),
                            proc.state().commands.len(),
                        );
                    }
                }

                write_state_file(state_path, &processors);
            }

            Notification::WindowClose { window_id } => {
                // Remove all panes belonging to this window
                tracing::info!("Window {} closed, resyncing panes", window_id);
                sync_all_panes(&mut conn, &mut processors, &origin_pane, &session_id).await?;
                write_state_file(state_path, &processors);
            }

            Notification::Other { line }
                if line.starts_with("%layout-change ")
                    || line.starts_with("%window-add ")
                    || line.starts_with("%window-pane-changed ") =>
            {
                // Pane added, removed, or changed — resync
                tracing::debug!("Pane change detected: {}", line);
                sync_all_panes(&mut conn, &mut processors, &origin_pane, &session_id).await?;
                write_state_file(state_path, &processors);
            }

            Notification::Exit { .. } | Notification::SessionClose { .. } => break,
            _ => {}
        }
    }

    Ok(())
}

/// Sync the processor map with the current tmux session state.
/// Adds processors for new panes, removes processors for dead panes,
/// keeps existing processors (preserving their state/history).
async fn sync_all_panes(
    conn: &mut RawTmuxConnection,
    processors: &mut HashMap<String, PaneProcessor>,
    origin_pane: &str,
    session_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let pane_list_output = conn.list_panes(session_id, true).await?;
    let current_panes = parse_pane_list(&pane_list_output);

    let current_ids: std::collections::HashSet<String> = current_panes
        .iter()
        .map(|p| p.pane_id.clone())
        .collect();

    // Remove processors for panes that no longer exist
    let dead: Vec<String> = processors
        .keys()
        .filter(|id| !current_ids.contains(id.as_str()))
        .cloned()
        .collect();
    for id in &dead {
        tracing::info!("Pane {} gone, removing", id);
        processors.remove(id);
    }

    // Add processors for new panes (skip origin)
    for pane in &current_panes {
        if pane.pane_id == origin_pane {
            continue;
        }
        if !processors.contains_key(&pane.pane_id) {
            tracing::info!(
                "New pane {} ({}x{}, process: {})",
                pane.pane_id, pane.width, pane.height, pane.process,
            );
            conn.enable_pane_output(&pane.pane_id).await?;
            processors.insert(
                pane.pane_id.clone(),
                PaneProcessor::new(pane.height, pane.width),
            );
        }
    }

    Ok(())
}

/// Strip ANSI escape sequences from text for clean display.
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if nc.is_ascii_alphabetic() { break; }
                    }
                }
                Some(']') => {
                    chars.next();
                    while let Some(&nc) = chars.peek() {
                        chars.next();
                        if nc == '\x07' { break; }
                        if nc == '\x1b' {
                            if chars.peek() == Some(&'\\') { chars.next(); }
                            break;
                        }
                    }
                }
                Some(_) => { chars.next(); }
                None => {}
            }
        } else if c == '\r' {
            // skip CR
        } else {
            result.push(c);
        }
    }
    result
}

fn write_state_file(path: &str, processors: &HashMap<String, PaneProcessor>) {
    let mut buf = String::new();

    // Sort pane IDs for stable output
    let mut pane_ids: Vec<&String> = processors.keys().collect();
    pane_ids.sort();

    for pane_id in pane_ids {
        let proc = &processors[pane_id];
        let state = proc.state();

        buf.push_str(&format!(
            "=== {} | {:?} | cwd={} | host={} ===\n",
            pane_id,
            state.activity,
            state.cwd.as_deref().unwrap_or("?"),
            state.hostname.as_deref().unwrap_or("local"),
        ));

        let cmds = state.recent_commands(10);
        if cmds.is_empty() {
            buf.push_str("  (no commands yet)\n");
        } else {
            for cmd in cmds.iter().rev() {
                let exit_indicator = match cmd.exit_code {
                    Some(0) => " ok ".to_string(),
                    Some(c) => format!("{:>3} ", c),
                    None => " ?? ".to_string(),
                };
                buf.push_str(&format!("  [{}] {}\n", exit_indicator, cmd.command));

                let output = strip_ansi(&cmd.output);
                let output = output.trim();
                if !output.is_empty() {
                    let lines: Vec<&str> = output.lines().collect();
                    let show = if lines.len() > 3 {
                        format!(
                            "    {}\n    ... ({} more lines)\n    {}",
                            lines[0],
                            lines.len() - 2,
                            lines.last().unwrap(),
                        )
                    } else {
                        lines.iter().map(|l| format!("    {}", l)).collect::<Vec<_>>().join("\n")
                    };
                    buf.push_str(&show);
                    buf.push('\n');
                }
            }
        }
        buf.push('\n');
    }

    if let Ok(mut f) = std::fs::File::create(path) {
        let _ = f.write_all(buf.as_bytes());
    }
}
