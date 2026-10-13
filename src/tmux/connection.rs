//! Low-level tmux control mode connection.
//!
//! Spawns `tmux -C attach-session`, provides async command execution and
//! notification receiving as two independent types to prevent deadlocks.

use std::process::Stdio;

use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

use crate::client::TmuxSession;
use crate::pane::registry::PaneMode;
use crate::tmux::notification::Notification;
use crate::tmux::reader;

// --- Error Types ---

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to spawn tmux: {source}"))]
    Spawn { source: std::io::Error },

    #[snafu(display("Failed to write to tmux stdin: {source}"))]
    Write { source: std::io::Error },

    #[snafu(display("Tmux connection closed"))]
    ConnectionClosed,

    #[snafu(display("Tmux command failed: {message}"))]
    CommandFailed { message: String },
}

pub type Result<T> = std::result::Result<T, Error>;

/// How long to wait for a control-mode command response before giving up.
///
/// tmux answers every command with a `%begin/%end` block; a wait longer than
/// this means the response was lost or the connection is unhealthy, so we
/// surface an error rather than block the daemon forever. Override with
/// `TMUX_MCP_EXEC_TIMEOUT_MS` (default 15000).
fn exec_timeout() -> std::time::Duration {
    let millis = std::env::var("TMUX_MCP_EXEC_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(15_000);
    std::time::Duration::from_millis(millis)
}

// --- Split connection types ---

/// Command-sending half of the tmux connection.
/// Send commands, keys, enable/disable output. Goes behind `Arc<Mutex<>>`.
pub struct TmuxCommands {
    stdin: ChildStdin,
    cmd_tx: mpsc::Sender<reader::ReaderCommand>,
    _child: Child,
    _reader_handle: tokio::task::JoinHandle<()>,
    session: String,
}

/// Notification-receiving half of the tmux connection.
/// Used directly by the event loop — never behind a mutex.
pub struct TmuxNotifications {
    rx: mpsc::Receiver<Notification>,
}

/// Connect to a tmux session in control mode.
///
/// Returns split halves: `TmuxCommands` for sending commands (goes behind mutex)
/// and `TmuxNotifications` for receiving events (used directly by event loop).
/// This split makes deadlock structurally impossible.
pub async fn connect(session: &TmuxSession) -> Result<(TmuxCommands, TmuxNotifications)> {
    tracing::info!("Connecting to tmux session {}", session);

    let mut cmd = Command::new("tmux");
    cmd.arg("-S").arg(session.server());
    cmd.args(["-C", "attach-session", "-t", &session.target()]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context(SpawnSnafu)?;

    let mut stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");

    let (cmd_tx, cmd_rx) = mpsc::channel::<reader::ReaderCommand>(32);
    let (notification_tx, notification_rx) = mpsc::channel::<Notification>(4096);

    // Priming: drain the unsolicited attach `%begin/%end` block before routing
    // real commands, so the response FIFO starts aligned (see `reader::spawn_reader`).
    let (ready_tx, ready_rx) = oneshot::channel();
    let prime_marker = format!("TMUX_MCP_PRIME_{}", std::process::id());
    let reader_handle = reader::spawn_reader(
        stdout,
        cmd_rx,
        notification_tx,
        ready_tx,
        prime_marker.clone(),
    );

    // Send the marker command directly (unregistered): its response is the
    // boundary between pre-prime noise and aligned command routing. The marker
    // must stay on a single line: control mode treats every newline as a
    // command separator, so a newline inside the quoted argument becomes a
    // stray command that answers with an extra %error block and desyncs the
    // response FIFO for the first registered command.
    stdin
        .write_all(format!("display-message -p '{}'\n", prime_marker).as_bytes())
        .await
        .context(WriteSnafu)?;
    stdin.flush().await.context(WriteSnafu)?;

    match tokio::time::timeout(std::time::Duration::from_secs(5), ready_rx).await {
        Ok(Ok(())) => tracing::debug!("tmux control mode primed"),
        Ok(Err(_)) => tracing::warn!("Reader closed before priming completed"),
        Err(_) => {
            tracing::warn!("Priming timed out; routing responses anyway");
            let _ = cmd_tx.send(reader::ReaderCommand::StopPriming).await;
        }
    }

    let commands = TmuxCommands {
        stdin,
        cmd_tx,
        _child: child,
        _reader_handle: reader_handle,
        session: session.target(),
    };

    let notifications = TmuxNotifications {
        rx: notification_rx,
    };

    Ok((commands, notifications))
}

// --- TmuxCommands ---

impl TmuxCommands {
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Execute a tmux control mode command and wait for the response.
    ///
    /// The wait is bounded by [`exec_timeout`]: if tmux does not answer (a slow
    /// or lost `%begin/%end` block), this returns an error instead of awaiting
    /// forever. Without this bound a single unanswered command could hold the
    /// connection lock (and, in the event loop, the registry lock) for the
    /// lifetime of the daemon, wedging every subsequent RPC.
    pub async fn execute(&mut self, command: &str) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();

        // Register the command with the reader BEFORE writing to stdin.
        self.cmd_tx
            .send(reader::ReaderCommand::Register {
                command: command.to_string(),
                response_tx,
            })
            .await
            .map_err(|_| Error::ConnectionClosed)?;

        // Yield to let the reader task process the registration
        tokio::task::yield_now().await;

        // Write command to tmux stdin
        self.stdin
            .write_all(format!("{}\n", command).as_bytes())
            .await
            .context(WriteSnafu)?;
        self.stdin.flush().await.context(WriteSnafu)?;

        // Wait for response, but never indefinitely. A late response still
        // pairs with this (now abandoned) registration, so the reader's FIFO
        // stays in sync and the next command is routed correctly.
        let timeout = exec_timeout();
        let response = match tokio::time::timeout(timeout, response_rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => return Err(Error::ConnectionClosed),
            Err(_) => {
                return Err(Error::CommandFailed {
                    message: format!(
                        "timed out after {:?} waiting for tmux response to `{}`",
                        timeout, command
                    ),
                });
            }
        };

        if response.is_error {
            Err(Error::CommandFailed {
                message: response.output,
            })
        } else {
            Ok(response.output)
        }
    }

    /// Send keys to a pane without waiting for completion.
    pub async fn send_keys(&mut self, target: &str, keys: &str) -> Result<()> {
        let lines: Vec<&str> = keys.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                self.execute(&format!("send-keys -t {} C-j", target))
                    .await?;
            }
            if !line.is_empty() {
                self.execute(&format!(
                    "send-keys -t {} {}",
                    target,
                    crate::parse::escape::shell_escape(line),
                ))
                .await?;
            }
        }
        Ok(())
    }

    /// Send keys followed by Enter.
    pub async fn send_command(&mut self, target: &str, command: &str) -> Result<()> {
        self.send_keys(target, command).await?;
        self.execute(&format!("send-keys -t {} Enter", target))
            .await?;
        Ok(())
    }

    /// Send raw keys to a pane, passing the keys string directly to tmux
    /// for key name interpretation (C-c, Enter, Escape, etc).
    pub async fn send_raw_keys(&mut self, target: &str, keys: &str) -> Result<()> {
        // Quoted, so `;` or `#` in keys cannot end or comment out the tmux
        // command; tmux strips the quotes and still reads a key name.
        let keys = crate::parse::escape::shell_escape(keys);
        self.execute(&format!("send-keys -t {} {}", target, keys))
            .await?;
        Ok(())
    }

    /// Enable %output notifications for a pane.
    pub async fn enable_pane_output(&mut self, pane_id: &str) -> Result<()> {
        self.execute(&format!("refresh-client -A '{}:on'", pane_id))
            .await
            .map(|_| ())
    }

    /// Disable %output notifications for a pane.
    pub async fn disable_pane_output(&mut self, pane_id: &str) -> Result<()> {
        self.execute(&format!("refresh-client -A '{}:off'", pane_id))
            .await
            .map(|_| ())
    }

    /// Query the mode a pane is in.
    pub async fn query_pane_mode(&mut self, pane_id: &str) -> Result<PaneMode> {
        let output = self
            .execute(&format!("display-message -t {pane_id} -p '#{{pane_mode}}'"))
            .await?;
        Ok(PaneMode::from_format(&output))
    }

    /// Query the shell PID of a pane.
    pub async fn query_pane_pid(&mut self, pane_id: &str) -> Result<u32> {
        let output = self
            .execute(&format!(
                "display-message -t {} -p '#{{pane_pid}}'",
                pane_id
            ))
            .await?;
        output
            .trim()
            .parse::<u32>()
            .map_err(|_| Error::CommandFailed {
                message: format!("invalid pane_pid: {}", output.trim()),
            })
    }

    /// Query the foreground command name of a pane (tmux's own view).
    ///
    /// This is the *effective* shell/command, which can differ from `pane_pid`
    /// when tmux runs a login shell that then spawns another shell (e.g.
    /// `default-shell fish` with `bash` as a child).
    pub async fn query_pane_command(&mut self, pane_id: &str) -> Result<String> {
        let output = self
            .execute(&format!(
                "display-message -t {} -p '#{{pane_current_command}}'",
                pane_id
            ))
            .await?;
        Ok(output.trim().to_string())
    }
}

// --- TmuxNotifications ---

impl TmuxNotifications {
    /// Receive the next notification from tmux.
    /// Returns `None` when the connection closes.
    pub async fn recv(&mut self) -> Option<Notification> {
        self.rx.recv().await
    }
}
