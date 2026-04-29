//! Low-level tmux control mode connection.
//!
//! Spawns `tmux -C attach-session`, provides async `execute()` for sending
//! commands and receiving responses, and exposes a notification channel for
//! %output and other tmux events.

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

// --- RawTmuxConnection ---

pub struct RawTmuxConnection {
    stdin: ChildStdin,
    cmd_tx: mpsc::Sender<reader::CommandRegistration>,
    notification_rx: mpsc::Receiver<Notification>,
    _child: Child,
    _reader_handle: tokio::task::JoinHandle<()>,
    origin_pane: String,
    session_id: String,
    session_name: String,
}

impl RawTmuxConnection {
    /// Attach to the current tmux session in control mode.
    ///
    /// Discovers the origin pane and session, spawns `tmux -C attach-session`,
    /// and starts the reader task for response/notification separation.
    pub async fn connect() -> Result<Self> {
        let origin_pane = query_tmux(&["display-message", "-p", "#{pane_id}"]).await?;
        let session_id = discover_session_id().await?;
        let session_name = query_tmux(&["display-message", "-p", "#{session_name}"]).await?;

        tracing::info!(
            "Connecting to tmux session {} ({}) from pane {}",
            session_id,
            session_name,
            origin_pane,
        );

        let mut cmd = Command::new("tmux");
        cmd.args(["-C", "attach-session", "-t", &origin_pane]);
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

        let mut conn = Self {
            stdin,
            cmd_tx,
            notification_rx,
            _child: child,
            _reader_handle: reader_handle,
            origin_pane,
            session_id,
            session_name,
        };

        // Consume initial attach response. tmux -C implicitly runs attach,
        // which sends a response block we need to drain.
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        let init = conn.execute("display-message").await;
        tracing::debug!("Initial display-message result: {:?}", init);

        Ok(conn)
    }

    pub fn origin_pane(&self) -> &str {
        &self.origin_pane
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn session_name(&self) -> &str {
        &self.session_name
    }

    pub fn child_pid(&self) -> Option<u32> {
        self._child.id()
    }

    /// Execute a tmux control mode command and wait for the response.
    pub async fn execute(&mut self, command: &str) -> Result<String> {
        let (response_tx, response_rx) = oneshot::channel();

        // Register the command with the reader BEFORE writing to stdin.
        // This prevents a race where tmux responds before the reader
        // knows to expect it.
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

    /// Receive the next notification from tmux.
    pub async fn recv_notification(&mut self) -> Option<Notification> {
        self.notification_rx.recv().await
    }

    /// Send keys to a pane without waiting for completion.
    ///
    /// Handles multiline input by splitting on newlines and sending
    /// C-j between parts (tmux control mode uses newlines as command
    /// separators, so we can't send literal newlines).
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
}

// --- Helper Functions ---

/// Run a one-shot tmux command (outside control mode) and return trimmed output.
async fn query_tmux(args: &[&str]) -> Result<String> {
    let output = Command::new("tmux")
        .args(args)
        .output()
        .await
        .context(SpawnSnafu)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Discover the session ID from the TMUX env var or by querying tmux.
async fn discover_session_id() -> Result<String> {
    // TMUX env var format: /tmp/tmux-1000/default,12345,$0
    if let Ok(tmux_env) = std::env::var("TMUX") {
        if let Some(session_id) = tmux_env.rsplit(',').next() {
            if session_id.starts_with('$') {
                return Ok(session_id.to_string());
            }
        }
    }
    let id = query_tmux(&["display-message", "-p", "#{session_id}"]).await?;
    if id.starts_with('$') { Ok(id) } else { Ok(format!("${}", id)) }
}
