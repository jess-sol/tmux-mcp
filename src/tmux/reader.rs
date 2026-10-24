//! Reader task: reads lines from tmux control mode stdout, separates
//! command responses (%begin/%end blocks) from notifications (%output, etc.).
//!
//! Uses biased `tokio::select!` to always register pending commands before
//! reading lines. This prevents a race where tmux responds before the
//! reader knows a command was sent.

use std::collections::VecDeque;

use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, oneshot};

use crate::tmux::notification::{
    CommandResponse, Notification, ReaderMessage, parse_line, parse_output,
};

/// A message from the command half to the reader.
pub(crate) enum ReaderCommand {
    /// Route the next `%begin/%end` block to this sender.
    Register {
        command: String,
        response_tx: oneshot::Sender<CommandResponse>,
    },
    /// Leave the priming phase early (e.g. the prime command timed out).
    StopPriming,
}

/// Spawn the reader task. Returns immediately; the task runs until the
/// stdout stream closes or the command channel drops.
///
/// The reader starts in a *priming* phase: `tmux -C attach-session` emits an
/// unsolicited `%begin/%end` block for the attach itself. Until we have drained
/// it, every subsequent response would be shifted by one — pairing the attach
/// block with our first command and leaving the real response unowned. To
/// align the queue deterministically we send a marker command
/// (`display-message -p '<prime_marker>'`) before any real command and discard
/// blocks until the one containing the marker arrives; then `ready_tx` fires
/// and normal FIFO routing begins.
pub(crate) fn spawn_reader(
    stdout: tokio::process::ChildStdout,
    cmd_rx: mpsc::Receiver<ReaderCommand>,
    notification_tx: mpsc::Sender<Notification>,
    ready_tx: oneshot::Sender<()>,
    prime_marker: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(reader_loop(
        stdout,
        cmd_rx,
        notification_tx,
        ready_tx,
        prime_marker,
    ))
}

async fn reader_loop(
    stdout: tokio::process::ChildStdout,
    mut cmd_rx: mpsc::Receiver<ReaderCommand>,
    notification_tx: mpsc::Sender<Notification>,
    ready_tx: oneshot::Sender<()>,
    prime_marker: String,
) {
    let mut reader = tokio::io::BufReader::new(stdout);
    let mut pending_commands: VecDeque<oneshot::Sender<CommandResponse>> = VecDeque::new();
    let mut current_block: Option<Vec<String>> = None;
    let mut line_buf = Vec::new();
    let mut priming = true;
    let mut ready_tx = Some(ready_tx);

    loop {
        tokio::select! {
            // Biased: always check command registrations first.
            // This ensures the response sender is registered before
            // we read the line that might contain the response.
            biased;

            cmd = cmd_rx.recv() => {
                match cmd {
                    Some(ReaderCommand::Register { command: cmd_str, response_tx }) => {
                        tracing::debug!(
                            "Reader: registered command (pending={}): {}",
                            pending_commands.len() + 1,
                            cmd_str,
                        );
                        pending_commands.push_back(response_tx);
                    }
                    Some(ReaderCommand::StopPriming) => {
                        if priming {
                            tracing::warn!("Reader: priming stopped early; routing responses now");
                            priming = false;
                            if let Some(tx) = ready_tx.take() {
                                let _ = tx.send(());
                            }
                        }
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
                        let raw = line_buf.strip_suffix(b"\n").unwrap_or(&line_buf);
                        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
                        if let Some(notif) = parse_output(raw) {
                            if notification_tx.try_send(notif).is_err() {
                                tracing::warn!("Notification channel full, dropping");
                            }
                            line_buf.clear();
                            continue;
                        }
                        let line = String::from_utf8_lossy(&line_buf);
                        let line = line.trim_end_matches('\n').trim_end_matches('\r');

                        if let Some(msg) = parse_line(line, &mut current_block) {
                            match msg {
                                ReaderMessage::Response(resp) => {
                                    if priming {
                                        // Drain the attach block(s) and any other
                                        // pre-prime output until the marker response
                                        // arrives, so FIFO routing starts aligned.
                                        if resp.output.contains(&prime_marker) {
                                            priming = false;
                                            tracing::debug!(
                                                "Reader: primed; response routing active"
                                            );
                                            if let Some(tx) = ready_tx.take() {
                                                let _ = tx.send(());
                                            }
                                        } else {
                                            tracing::trace!(
                                                "Reader: discarding pre-prime response block"
                                            );
                                        }
                                    } else if let Some(tx) = pending_commands.pop_front() {
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
