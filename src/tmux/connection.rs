//! Low-level tmux control mode connection.
//!
//! Spawns `tmux -C attach-session`, provides async command execution and
//! notification receiving as two independent types to prevent deadlocks.

use std::process::Stdio;

use snafu::{ResultExt, Snafu};
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot};

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

// --- Split connection types ---

/// Command-sending half of the tmux connection.
/// Send commands, keys, enable/disable output. Goes behind `Arc<Mutex<>>`.
pub struct TmuxCommands {
    stdin: ChildStdin,
    cmd_tx: mpsc::Sender<reader::CommandRegistration>,
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
pub async fn connect(session: &str) -> Result<(TmuxCommands, TmuxNotifications)> {
    tracing::info!("Connecting to tmux session {}", session);

    let mut cmd = Command::new("tmux");
    cmd.args(["-C", "attach-session", "-t", session]);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::inherit());
    cmd.kill_on_drop(true);

    let mut child = cmd.spawn().context(SpawnSnafu)?;

    let stdin = child.stdin.take().expect("stdin was piped");
    let stdout = child.stdout.take().expect("stdout was piped");

    let (cmd_tx, cmd_rx) = mpsc::channel::<reader::CommandRegistration>(32);
    let (notification_tx, notification_rx) = mpsc::channel::<Notification>(4096);

    let reader_handle = reader::spawn_reader(stdout, cmd_rx, notification_tx);

    let mut commands = TmuxCommands {
        stdin,
        cmd_tx,
        _child: child,
        _reader_handle: reader_handle,
        session: session.to_string(),
    };

    let notifications = TmuxNotifications {
        rx: notification_rx,
    };

    // Consume initial attach response. tmux -C implicitly runs attach,
    // which sends a response block we need to drain.
    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
    let init = commands.execute("display-message").await;
    tracing::debug!("Initial display-message result: {:?}", init);

    Ok((commands, notifications))
}

// --- TmuxCommands ---

impl TmuxCommands {
    pub fn session(&self) -> &str {
        &self.session
    }

    pub fn child_pid(&self) -> Option<u32> {
        self._child.id()
    }

    /// Execute a tmux control mode command and wait for the response.
    pub async fn execute(&mut self, command: &str) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();

        // Register the command with the reader BEFORE writing to stdin.
        self.cmd_tx
            .send((command.to_string(), response_tx))
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

        // Wait for response
        let response = response_rx.await.map_err(|_| Error::ConnectionClosed)?;

        if response.is_error {
            Err(Error::CommandFailed { message: response.output })
        } else {
            Ok(response.output)
        }
    }

    /// Send keys to a pane without waiting for completion.
    pub async fn send_keys(&mut self, target: &str, keys: &str) -> Result<()> {
        let lines: Vec<&str> = keys.split('\n').collect();
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                self.execute(&format!("send-keys -t {} C-j", target)).await?;
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
        self.execute(&format!("send-keys -t {} Enter", target)).await?;
        Ok(())
    }

    /// List panes in a window or session, returning raw tmux output.
    pub async fn list_panes(&mut self, target: &str, session_wide: bool) -> Result<String> {
        let flag = if session_wide { "-s" } else { "" };
        self.execute(&format!(
            "list-panes {} -t {} -F \"#{{window_id}}\t#{{pane_id}}\t#{{pane_index}}\t#{{pane_active}}\t#{{pane_title}}\t#{{pane_current_command}}\t#{{pane_current_path}}\t#{{pane_width}}\t#{{pane_height}}\"",
            flag, target,
        )).await
    }

    /// Capture raw pane content (for raw_read / screen snapshots).
    pub async fn capture_pane(&mut self, target: &str, scrollback: i32) -> Result<String> {
        self.execute(&format!("capture-pane -J -p -t {} -S -{}", target, scrollback))
            .await
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

    /// Query the shell PID of a pane.
    pub async fn query_pane_pid(&mut self, pane_id: &str) -> Result<u32> {
        let output = self
            .execute(&format!("display-message -t {} -p '#{{pane_pid}}'", pane_id))
            .await?;
        output.trim().parse::<u32>().map_err(|_| Error::CommandFailed {
            message: format!("invalid pane_pid: {}", output.trim()),
        })
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
