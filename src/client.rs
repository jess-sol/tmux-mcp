/// Daemon client: connects to a running daemon over a Unix socket,
/// or spawns one if needed. Uses newline-delimited JSON-RPC.

use std::fmt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::Snafu;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

// --- Tmux session ---

/// One tmux session on one tmux server: what one daemon serves. A session id
/// alone names nothing, since every server numbers its sessions from `$0`;
/// the server's pid tells live servers apart.
#[derive(Clone)]
pub struct TmuxSession {
    server: PathBuf,
    pid: u32,
    id: u32,
}

#[derive(Debug, Snafu)]
pub enum TmuxSessionError {
    #[snafu(display("TMUX is not set — tmux-mcp runs inside a tmux pane"))]
    NotInTmux,
    #[snafu(display("TMUX={value:?} names no tmux session"))]
    Malformed { value: String },
}

impl TmuxSession {
    /// The `TmuxSession` named by `$TMUX`: the one the shipped binary serves.
    pub fn from_env() -> Result<TmuxSession, TmuxSessionError> {
        std::env::var("TMUX").map_err(|_| TmuxSessionError::NotInTmux)?.parse()
    }

    /// Where the daemon of this `TmuxSession` listens.
    pub fn socket(&self) -> PathBuf {
        self.file("sock")
    }

    pub(crate) fn lock(&self) -> PathBuf {
        self.file("lock")
    }

    pub(crate) fn log(&self) -> PathBuf {
        self.file("log")
    }

    /// Creates the tmux-mcp directory, private to this user.
    pub(crate) fn create_dir(&self) -> std::io::Result<()> {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir())
    }

    pub(crate) fn server(&self) -> &Path {
        &self.server
    }

    /// The tmux target naming this session on its server.
    pub(crate) fn target(&self) -> String {
        format!("${}", self.id)
    }

    /// `<server socket name>-<server pid>-<session id>.<extension>`; the
    /// socket name only labels the server for a reader.
    fn file(&self, extension: &str) -> PathBuf {
        let name = self.server.file_name().unwrap_or_default().to_string_lossy();
        dir().join(format!("{name}-{}-{}.{extension}", self.pid, self.id))
    }
}

impl FromStr for TmuxSession {
    type Err = TmuxSessionError;

    /// Parses a `$TMUX` value, `<server socket>,<server pid>,<session id>`.
    /// The socket path may itself hold commas, so fields come off the right.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let malformed = || TmuxSessionError::Malformed { value: value.to_string() };
        let mut fields = value.rsplitn(3, ',');
        let id = fields.next().and_then(|id| id.parse().ok()).ok_or_else(malformed)?;
        let pid = fields.next().and_then(|pid| pid.parse().ok()).ok_or_else(malformed)?;
        let server = fields.next().filter(|s| !s.is_empty()).ok_or_else(malformed)?;
        Ok(TmuxSession { server: PathBuf::from(server), pid, id })
    }
}

impl fmt::Display for TmuxSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} on {}", self.target(), self.server.display())
    }
}

/// `$XDG_RUNTIME_DIR/tmux-mcp`, or `/tmp/tmux-mcp-<uid>` without one.
fn dir() -> PathBuf {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(runtime) => PathBuf::from(runtime).join("tmux-mcp"),
        None => {
            let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
            std::env::temp_dir().join(format!("tmux-mcp-{uid}"))
        }
    }
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
        // A model reads this text; the code is transport detail it cannot use.
        f.write_str(&self.message)
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
    pub async fn connect(session: &TmuxSession) -> Result<Self, Box<dyn std::error::Error>> {
        let sock = session.socket();

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
        // The daemon reads the same `$TMUX` this process inherited.
        session.create_dir()?;
        let exe = std::env::current_exe()?;
        let mut child = tokio::process::Command::new(&exe)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr({
                match std::fs::File::create(session.log()) {
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

#[cfg(test)]
mod tests {
    use super::*;

    // --- Tmux session ---

    #[test]
    fn a_tmux_value_names_its_server_and_session() {
        let cases = [
            ("/run/user/1000/tmux-1000/default,563061,0", Some("$0 on /run/user/1000/tmux-1000/default")),
            ("/tmp/odd,name,42,7", Some("$7 on /tmp/odd,name")),
            ("", None),
            ("/tmp/tmux-1000/default", None),
            ("/tmp/tmux-1000/default,42,x", None),
            ("/tmp/tmux-1000/default,x,0", None),
            (",42,0", None),
        ];
        for (value, want) in cases {
            let got = value.parse::<TmuxSession>().ok().map(|s| s.to_string());
            assert_eq!(got.as_deref(), want, "TMUX={value:?}");
        }
    }

    #[test]
    fn each_tmux_server_gets_its_own_daemon_socket() {
        let a: TmuxSession = "/run/user/1000/tmux-1000/default,4242,0".parse().unwrap();
        let b: TmuxSession = "/tmp/elsewhere/default,4343,0".parse().unwrap();

        assert_eq!(a.socket().file_name(), Some(std::ffi::OsStr::new("default-4242-0.sock")));
        assert_ne!(a.socket(), b.socket());
    }
}
