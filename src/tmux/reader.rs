//! Reader task: reads lines from tmux control mode stdout, separates
//! command responses (%begin/%end blocks) from notifications (%output, etc.).
//!
//! Uses biased `tokio::select!` to always register pending commands before
//! reading lines. This prevents a race where tmux responds before the
//! reader knows a command was sent.

use std::collections::VecDeque;

use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};

use crate::tmux::notification::{CommandResponse, Notification, ReaderMessage, parse_line};

/// Handle to send commands to the reader for response routing.
///
/// When `RawTmuxConnection::execute()` sends a command to tmux stdin,
/// it also sends a response oneshot through this channel so the reader
/// knows to route the next %begin/%end block back to the caller.
pub(crate) type CommandRegistration = (String, oneshot::Sender<CommandResponse>);

/// Spawn the reader task. Returns immediately; the task runs until the
/// stdout stream closes or the command channel drops.
pub(crate) fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    cmd_rx: mpsc::Receiver<CommandRegistration>,
    notification_tx: mpsc::Sender<Notification>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(reader_loop(stdout, cmd_rx, notification_tx))
}

async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    mut cmd_rx: mpsc::Receiver<CommandRegistration>,
    notification_tx: mpsc::Sender<Notification>,
) {
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut pending_commands: VecDeque<oneshot::Sender<CommandResponse>> = VecDeque::new();
    let mut current_block: Option<Vec<String>> = None;
    let mut line_buf = Vec::new();

    loop {
        tokio::select! {
            // Biased: always check command registrations first.
            // This ensures the response sender is registered before
            // we read the line that might contain the response.
            biased;

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some((cmd_str, response_tx)) => {
                        tracing::debug!(
                            "Reader: registered command (pending={}): {}",
                            pending_commands.len() + 1,
                            cmd_str,
                        );
                        pending_commands.push_back(response_tx);
                    }
                    None => {
                        tracing::info!("Reader: command channel closed");
                        break;
                    }
                }
            }

            result = reader.read_until(b'\n', &mut line_buf) => {
                match result {
                    Ok(0) => {
                        tracing::warn!("Reader: EOF from tmux control mode");
                        break;
                    }
                    Ok(_) => {
                        let line = String::from_utf8_lossy(&line_buf);
                        let line = line.trim_end_matches('\n').trim_end_matches('\r');

                        if let Some(msg) = parse_line(line, &mut current_block) {
                            match msg {
                                ReaderMessage::Response(resp) => {
                                    if let Some(tx) = pending_commands.pop_front() {
                                        let _ = tx.send(resp);
                                    } else {
                                        tracing::warn!(
                                            "Reader: response with no pending command: {:?}",
                                            resp.output.chars().take(100).collect::<String>(),
                                        );
                                    }
                                }
                                ReaderMessage::Notification(notif) => {
                                    tracing::trace!("Reader: notification: {:?}", notif);
                                    if notification_tx.try_send(notif).is_err() {
                                        tracing::warn!("Notification channel full, dropping");
                                    }
                                }
                            }
                        }
                        line_buf.clear();
                    }
                    Err(e) => {
                        tracing::error!("Reader: error reading from tmux: {}", e);
                        break;
                    }
                }
            }
        }
    }
    tracing::info!("Reader task exiting");
}
