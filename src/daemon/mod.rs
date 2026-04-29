/// Daemon: connects to a tmux session, discovers all panes, and continuously
/// processes notifications — dispatching output to per-pane VTE processors
/// and reacting to topology changes via layout-change events.
///
/// Also runs a Unix socket server for RPC clients (MCP servers).

pub mod rpc;
pub mod server;

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::pane::registry::{PaneRegistry, SyncAction};
use crate::parse::escape::unescape_tmux_output;
use crate::parse::layout::parse_layout;
use crate::tmux::connection::{self, TmuxCommands, TmuxNotifications};
use crate::tmux::notification::Notification;

use rpc::DaemonState;

/// Connect to a tmux session, bootstrap pane state, start the socket server,
/// and run the event loop. Returns when the session closes or the daemon is
/// cancelled (idle timeout).
pub async fn run(session: &str, server: Option<&str>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (mut commands, notifications) = connection::connect(session, server).await?;
    let mut registry = PaneRegistry::new();

    // Bootstrap: discover all existing panes in one query
    let output = commands
        .execute(
            "list-panes -s -F '#{window_id}\t#{pane_id}\t#{pane_pid}\t#{window_layout}'",
        )
        .await?;
    let (windows, panes) = parse_bootstrap(&output);

    tracing::info!(
        "Bootstrap: {} windows, {} panes",
        windows.len(),
        panes.len(),
    );

    // Seed registry from layouts
    for win in &windows {
        let layout_panes = parse_layout(&win.layout);
        let actions = registry.apply_layout(&win.window_id, &layout_panes);
        execute_actions(&mut commands, &mut registry, &actions).await;
    }

    // Set PIDs from the same query
    for bp in &panes {
        registry.set_pid(&bp.pane_id, bp.pid);
    }

    tracing::info!("Monitoring {} panes", registry.len());

    // Shared state for RPC handlers (commands + registry behind mutexes)
    let state = Arc::new(DaemonState {
        conn: Mutex::new(commands),
        registry: Mutex::new(registry),
        started_at: std::time::Instant::now(),
    });

    let cancel = CancellationToken::new();

    // Spawn socket server
    let server_state = state.clone();
    let server_cancel = cancel.clone();
    let session_owned = session.to_string();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server::serve(&session_owned, server_state, server_cancel).await {
            tracing::error!("Server error: {}", e);
        }
    });

    // Event loop — notifications come through their own channel, no mutex needed
    let event_result = event_loop(notifications, state.clone(), cancel.clone()).await;

    // If event loop exited, cancel the server too
    cancel.cancel();
    let _ = server_handle.await;

    event_result
}

async fn event_loop(
    mut notifications: TmuxNotifications,
    state: Arc<DaemonState>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    loop {
        let notification = tokio::select! {
            notif = notifications.recv() => notif,
            _ = cancel.cancelled() => {
                tracing::info!("Daemon cancelled");
                break;
            }
        };

        let Some(notification) = notification else {
            tracing::error!("Tmux connection closed");
            break;
        };

        match notification {
            Notification::LayoutChange { window_id, layout } => {
                let layout_panes = parse_layout(&layout);
                let mut conn = state.conn.lock().await;
                let mut registry = state.registry.lock().await;
                let actions = registry.apply_layout(&window_id, &layout_panes);
                execute_actions(&mut conn, &mut registry, &actions).await;
            }

            Notification::WindowClose { window_id } => {
                tracing::info!("Window {} closed", window_id);
                let mut conn = state.conn.lock().await;
                let mut registry = state.registry.lock().await;
                let actions = registry.remove_window(&window_id);
                execute_actions(&mut conn, &mut registry, &actions).await;
            }

            Notification::Output { pane_id, data } => {
                let mut registry = state.registry.lock().await;
                let Some(proc) = registry.get_processor_mut(&pane_id) else {
                    continue;
                };
                let bytes = unescape_tmux_output(&data);
                proc.process_chunk(&bytes);
            }

            Notification::Exit { .. } | Notification::SessionClose { .. } => {
                tracing::info!("Session ended");
                break;
            }

            _ => {}
        }
    }

    Ok(())
}

/// Execute sync actions (used during bootstrap and topology changes).
async fn execute_actions(
    conn: &mut TmuxCommands,
    registry: &mut PaneRegistry,
    actions: &[SyncAction],
) {
    for action in actions {
        match action {
            SyncAction::PaneAdded { pane_id } => {
                tracing::info!("Pane {} added", pane_id);
                if let Err(e) = conn.enable_pane_output(pane_id).await {
                    tracing::warn!("Failed to enable output for {}: {}", pane_id, e);
                    continue;
                }
                match conn.query_pane_pid(pane_id).await {
                    Ok(pid) => {
                        tracing::debug!("Pane {} pid={}", pane_id, pid);
                        registry.set_pid(pane_id, pid);
                    }
                    Err(e) => {
                        tracing::warn!("Failed to query pid for {}: {}", pane_id, e);
                    }
                }
            }
            SyncAction::PaneRemoved { pane_id } => {
                tracing::info!("Pane {} removed", pane_id);
                if let Err(e) = conn.disable_pane_output(pane_id).await {
                    tracing::warn!("Failed to disable output for {}: {}", pane_id, e);
                }
            }
            SyncAction::Resized {
                pane_id,
                width,
                height,
            } => {
                tracing::debug!("Pane {} resized to {}x{}", pane_id, width, height);
            }
        }
    }
}

// --- Bootstrap parsing ---

struct BootstrapWindow {
    window_id: String,
    layout: String,
}

struct BootstrapPane {
    pane_id: String,
    pid: u32,
}

/// Parse the bootstrap `list-panes` output.
///
/// Each line: `window_id \t pane_id \t pane_pid \t window_layout`
///
/// Returns deduplicated windows (by window_id) and all pane PIDs.
fn parse_bootstrap(output: &str) -> (Vec<BootstrapWindow>, Vec<BootstrapPane>) {
    let mut windows = Vec::new();
    let mut panes = Vec::new();
    let mut seen_windows = std::collections::HashSet::new();

    for line in output.lines() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() < 4 {
            continue;
        }

        let window_id = parts[0];
        let pane_id = parts[1];
        let pid_str = parts[2];
        let layout = parts[3];

        if let Ok(pid) = pid_str.parse::<u32>() {
            panes.push(BootstrapPane {
                pane_id: pane_id.to_string(),
                pid,
            });
        }

        if seen_windows.insert(window_id.to_string()) {
            windows.push(BootstrapWindow {
                window_id: window_id.to_string(),
                layout: layout.to_string(),
            });
        }
    }

    (windows, panes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bootstrap_single_window() {
        let output = "@0\t%0\t1234\taaaa,80x24,0,0,0\n\
                       @0\t%1\t1235\taaaa,80x24,0,0,0";
        let (windows, panes) = parse_bootstrap(output);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].window_id, "@0");
        assert_eq!(windows[0].layout, "aaaa,80x24,0,0,0");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0].pane_id, "%0");
        assert_eq!(panes[0].pid, 1234);
        assert_eq!(panes[1].pane_id, "%1");
        assert_eq!(panes[1].pid, 1235);
    }

    #[test]
    fn parse_bootstrap_multi_window() {
        let output = "@0\t%0\t100\taaaa,80x24,0,0,0\n\
                       @1\t%1\t200\tbbbb,120x40,0,0,1";
        let (windows, panes) = parse_bootstrap(output);
        assert_eq!(windows.len(), 2);
        assert_eq!(panes.len(), 2);
        assert_eq!(windows[0].window_id, "@0");
        assert_eq!(windows[1].window_id, "@1");
    }

    #[test]
    fn parse_bootstrap_deduplicates_windows() {
        let output = "@0\t%0\t100\taaaa,200x50,0,0{100x50,0,0,0,99x50,101,0,1}\n\
                       @0\t%1\t200\taaaa,200x50,0,0{100x50,0,0,0,99x50,101,0,1}";
        let (windows, panes) = parse_bootstrap(output);
        assert_eq!(windows.len(), 1);
        assert_eq!(panes.len(), 2);
    }

    #[test]
    fn parse_bootstrap_empty() {
        let (windows, panes) = parse_bootstrap("");
        assert!(windows.is_empty());
        assert!(panes.is_empty());
    }

    #[test]
    fn parse_bootstrap_malformed_line() {
        let output = "not\tenough\tfields";
        let (windows, panes) = parse_bootstrap(output);
        assert!(windows.is_empty());
        assert!(panes.is_empty());
    }

    #[test]
    fn parse_bootstrap_invalid_pid() {
        let output = "@0\t%0\tnotanumber\taaaa,80x24,0,0,0";
        let (windows, panes) = parse_bootstrap(output);
        assert_eq!(windows.len(), 1);
        assert!(panes.is_empty());
    }
}
