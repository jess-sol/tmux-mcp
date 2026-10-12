/// Daemon client: connects to a running daemon over a Unix socket,
/// or spawns one if needed. Uses newline-delimited JSON-RPC.
use std::collections::HashMap;
use std::fmt;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use snafu::Snafu;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::oneshot;

use crate::version::Version;

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
        std::env::var("TMUX")
            .map_err(|_| TmuxSessionError::NotInTmux)?
            .parse()
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
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir())
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
        let name = self
            .server
            .file_name()
            .unwrap_or_default()
            .to_string_lossy();
        dir().join(format!("{name}-{}-{}.{extension}", self.pid, self.id))
    }
}

impl FromStr for TmuxSession {
    type Err = TmuxSessionError;

    /// Parses a `$TMUX` value, `<server socket>,<server pid>,<session id>`.
    /// The socket path may itself hold commas, so fields come off the right.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let malformed = || TmuxSessionError::Malformed {
            value: value.to_string(),
        };
        let mut fields = value.rsplitn(3, ',');
        let id = fields
            .next()
            .and_then(|id| id.parse().ok())
            .ok_or_else(malformed)?;
        let pid = fields
            .next()
            .and_then(|pid| pid.parse().ok())
            .ok_or_else(malformed)?;
        let server = fields
            .next()
            .filter(|s| !s.is_empty())
            .ok_or_else(malformed)?;
        Ok(TmuxSession {
            server: PathBuf::from(server),
            pid,
            id,
        })
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
            let uid = std::fs::metadata("/proc/self")
                .map(|m| m.uid())
                .unwrap_or(0);
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
    /// The request this answers; null on a reply to a line the daemon could not parse.
    id: Option<u64>,
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

/// One connection to the daemon, shared by every caller: each request waits
/// for the reply that carries its own id, so requests overlap.
#[derive(Clone)]
pub struct DaemonClient {
    shared: Arc<Shared>,
}

struct Shared {
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    /// Requests awaiting their reply, by id; `None` once the daemon has closed
    /// the connection.
    pending: Mutex<Option<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    next_id: AtomicU64,
}

impl DaemonClient {
    /// Connects to the session's daemon when it runs this version, retires an
    /// older one and spawns this build's, and refuses a newer one. Whatever
    /// answers after a spawn is checked again, since another build may race.
    pub async fn connect(session: &TmuxSession) -> Result<Self, Box<dyn std::error::Error>> {
        let sock = session.socket();
        let mine = Version::current();
        for _ in 0..3 {
            let Ok(stream) = UnixStream::connect(&sock).await else {
                spawn_daemon(session).await?;
                continue;
            };
            let pid = stream.peer_cred().ok().and_then(|cred| cred.pid());
            let client = Self::from_stream(stream);
            let theirs = client.daemon_version().await;
            if theirs == mine {
                return Ok(client);
            }
            if theirs > mine {
                return Err(format!(
                    "The tmux-mcp daemon for this session runs {theirs}, newer than this \
                     {mine}. Update tmux-mcp and restart its MCP server."
                )
                .into());
            }
            tracing::info!("Retiring the daemon running {theirs} for {mine}");
            drop(client);
            retire(pid, &sock).await?;
        }
        Err("The tmux-mcp daemon kept changing; another build may be replacing it".into())
    }

    /// The daemon's version; one that can't say is older than any that can.
    async fn daemon_version(&self) -> Version {
        match self.request("version", Value::Null).await {
            Ok(reply) => Version::parse(reply["version"].as_str().unwrap_or("")),
            Err(_) => Version::default(),
        }
    }

    fn from_stream(stream: UnixStream) -> Self {
        let (read, write) = stream.into_split();
        let shared = Arc::new(Shared {
            writer: tokio::sync::Mutex::new(write),
            pending: Mutex::new(Some(HashMap::new())),
            next_id: AtomicU64::new(1),
        });
        tokio::spawn(route_replies(BufReader::new(read), Arc::downgrade(&shared)));
        Self { shared }
    }

    /// Send a JSON-RPC request and wait for the response.
    pub async fn request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, Box<dyn std::error::Error>> {
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        match self.shared.pending.lock().expect("pending lock").as_mut() {
            Some(pending) => pending.insert(id, reply_tx),
            None => return Err("Daemon closed connection".into()),
        };

        let req = JsonRpcRequest {
            jsonrpc: "2.0",
            id,
            method: method.to_string(),
            params,
        };
        let mut line = serde_json::to_string(&req)?;
        line.push('\n');
        {
            let mut writer = self.shared.writer.lock().await;
            writer.write_all(line.as_bytes()).await?;
            writer.flush().await?;
        }

        let resp = reply_rx.await.map_err(|_| "Daemon closed connection")?;
        if let Some(error) = resp.error {
            return Err(error.into());
        }
        Ok(resp.result.unwrap_or(Value::Null))
    }
}

/// Starts a detached daemon for `session` and returns once its socket answers.
/// Of two daemons spawned at once, the second fails to take the lock file and
/// exits; the daemon reads the same `$TMUX` this process inherited.
async fn spawn_daemon(session: &TmuxSession) -> Result<(), Box<dyn std::error::Error>> {
    session.create_dir()?;
    let log = std::fs::File::create(session.log()).map_or(Stdio::null(), Stdio::from);
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .kill_on_drop(false)
        .spawn()?;
    tracing::info!("Spawned daemon (pid={:?}), waiting for socket", child.id());

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        tokio::time::sleep(Duration::from_millis(100)).await;
        if UnixStream::connect(session.socket()).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            if let Ok(Some(status)) = child.try_wait() {
                return Err(format!("Daemon exited with {status}").into());
            }
            return Err("Timed out waiting for daemon socket".into());
        }
    }
}

/// Stops the older daemon at `sock`, process `pid`, with SIGTERM, and returns
/// once its socket stops answering.
async fn retire(pid: Option<i32>, sock: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let pid = pid.ok_or("The older tmux-mcp daemon's process is unknown")?;
    tokio::process::Command::new("kill")
        .arg(pid.to_string())
        .status()
        .await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while UnixStream::connect(sock).await.is_ok() {
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("The older tmux-mcp daemon (pid {pid}) did not stop").into());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}

/// Hands each reply to the request waiting for its id. When the daemon closes
/// the connection, every waiting request fails, and so does every later one.
async fn route_replies(mut reader: BufReader<OwnedReadHalf>, shared: Weak<Shared>) {
    let mut line = String::new();
    loop {
        line.clear();
        if !matches!(reader.read_line(&mut line).await, Ok(n) if n > 0) {
            break;
        }
        let Ok(resp) = serde_json::from_str::<JsonRpcResponse>(&line) else {
            continue;
        };
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let waiting = resp.id.and_then(|id| {
            shared
                .pending
                .lock()
                .expect("pending lock")
                .as_mut()
                .and_then(|p| p.remove(&id))
        });
        if let Some(waiting) = waiting {
            let _ = waiting.send(resp);
        }
    }
    if let Some(shared) = shared.upgrade() {
        // Dropping the senders fails every waiting request.
        shared.pending.lock().expect("pending lock").take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Tmux session ---

    #[test]
    fn a_tmux_value_names_its_server_and_session() {
        let cases = [
            (
                "/run/user/1000/tmux-1000/default,563061,0",
                Some("$0 on /run/user/1000/tmux-1000/default"),
            ),
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

        assert_eq!(
            a.socket().file_name(),
            Some(std::ffi::OsStr::new("default-4242-0.sock"))
        );
        assert_ne!(a.socket(), b.socket());
    }
}
