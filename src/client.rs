/// Daemon client: connects to a running daemon over a Unix socket,
/// or spawns one if needed. Uses newline-delimited JSON-RPC.

use std::path::PathBuf;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// --- Socket paths ---

pub fn socket_path(session: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/tmux-mcp-{}.sock", sanitize(session)))
}

pub fn lock_path(session: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/tmux-mcp-{}.lock", sanitize(session)))
}

/// Sanitize session name for use in file paths.
fn sanitize(s: &str) -> String {
    s.replace(['/', '\\', '\0', ' ', '$'], "")
}

/// Discover the tmux session from the TMUX environment variable.
/// Returns the session ID (e.g., "$0") or the session name.
pub fn discover_session() -> Result<String, String> {
    let tmux_env = std::env::var("TMUX")
        .map_err(|_| "TMUX environment variable not set — are you inside a tmux session?")?;

    // TMUX format: /path/to/socket,pid,session_number
    if let Some(session_num) = tmux_env.rsplit(',').next() {
        if !session_num.is_empty() {
            return Ok(format!("${}", session_num));
        }
    }

    Err(format!("Could not parse session from TMUX={}", tmux_env))
}

// --- JSON-RPC types ---

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: String,
    params: Value,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    #[allow(dead_code)]
    jsonrpc: String,
    #[allow(dead_code)]
    id: u64,
    result: Option<Value>,
    error: Option<JsonRpcError>,
}

#[derive(Deserialize, Debug)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

// --- Daemon Client ---

pub struct DaemonClient {
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: tokio::net::unix::OwnedWriteHalf,
    next_id: u64,
}

impl DaemonClient {
    /// Connect to an existing daemon, or spawn one and connect.
    pub async fn connect(session: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let sock = socket_path(session);

        // Try connecting to existing daemon
        match UnixStream::connect(&sock).await {
            Ok(stream) => {
                tracing::info!("Connected to existing daemon at {:?}", sock);
                return Ok(Self::from_stream(stream));
            }
            Err(_) => {
                tracing::info!("No daemon found, spawning one");
            }
        }

        // Spawn daemon as detached process.
        // If two clients race here, both spawn daemons — the second daemon
        // will fail to acquire the lock file and exit. Both clients poll
        // for the socket and connect to whichever daemon wins.
        let exe = std::env::current_exe()?;
        let mut child = tokio::process::Command::new(&exe)
            .args(["daemon", session])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr({
                let log_path = format!("/tmp/tmux-mcp-{}.log", sanitize(session));
                match std::fs::File::create(&log_path) {
                    Ok(f) => Stdio::from(f),
                    Err(_) => Stdio::null(),
                }
            })
            .kill_on_drop(false)
            .spawn()?;

        tracing::info!("Spawned daemon (pid={:?}), waiting for socket", child.id());

        // Poll for socket
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(5);
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

            if let Ok(stream) = UnixStream::connect(&sock).await {
                tracing::info!("Connected to new daemon");
                return Ok(Self::from_stream(stream));
            }

            if tokio::time::Instant::now() >= deadline {
                // Check if daemon died
                if let Ok(Some(status)) = child.try_wait() {
                    return Err(format!("Daemon exited with {}", status).into());
                }
                return Err("Timed out waiting for daemon socket".into());
            }
        }
    }

    fn from_stream(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        Self {
            reader: BufReader::new(read),
            writer: write,
            next_id: 1,
        }
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn request(
        &mut self,
        method: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let id = self.next_id;
        self.next_id += 1;

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };

        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        self.writer.write_all(line.as_bytes()).await?;
        self.writer.flush().await?;

        let mut response_line = String::new();
        self.reader.read_line(&mut response_line).await?;

        if response_line.is_empty() {
            return Err("Daemon closed connection".into());
        }

        let resp: JsonRpcResponse = serde_json::from_str(&response_line)?;

        if let Some(error) = resp.error {
            return Err(error.into());
        }

        Ok(resp.result.unwrap_or(Value::Null))
    }
}
