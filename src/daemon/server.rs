/// Unix socket server: accepts daemon clients, handles ndjson JSON-RPC.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client::TmuxSession;
use crate::daemon::rpc::{self, DaemonState};

/// Run the Unix socket server. Blocks until cancelled.
pub async fn serve(
    session: &TmuxSession,
    state: Arc<DaemonState>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    session.create_dir()?;
    let sock_path = session.socket();

    // Acquire lock file to prevent duplicate daemons
    let lock_path = session.lock();
    let lock_file = std::fs::File::create(&lock_path)?;
    use fs2::FileExt;
    lock_file.try_lock_exclusive().map_err(|_| {
        format!("Another daemon is already running for session {}", session)
    })?;

    // Remove stale socket if present
    let _ = std::fs::remove_file(&sock_path);

    let listener = UnixListener::bind(&sock_path)?;
    tracing::info!("Listening on {:?}", sock_path);

    let client_count = Arc::new(AtomicUsize::new(0));

    // Idle timeout task
    let idle_cancel = cancel.clone();
    let idle_count = client_count.clone();
    tokio::spawn(async move {
        // Give initial grace period for first client
        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
        let timeout = tokio::time::Duration::from_secs(300); // 5 minutes
        let mut idle_since = tokio::time::Instant::now();

        loop {
            tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

            if idle_count.load(Ordering::Relaxed) > 0 {
                idle_since = tokio::time::Instant::now();
            } else if idle_since.elapsed() >= timeout {
                tracing::info!("No clients for {:?}, shutting down", timeout);
                idle_cancel.cancel();
                return;
            }
        }
    });

    loop {
        tokio::select! {
            accept = listener.accept() => {
                let (stream, _addr) = accept?;
                let state = state.clone();
                let count = client_count.clone();

                count.fetch_add(1, Ordering::Relaxed);
                let n = count.load(Ordering::Relaxed);
                tracing::info!("Client connected ({} active)", n);

                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, state).await {
                        tracing::warn!("Client error: {}", e);
                    }
                    let n = count.fetch_sub(1, Ordering::Relaxed) - 1;
                    tracing::info!("Client disconnected ({} active)", n);
                });
            }
            _ = cancel.cancelled() => {
                tracing::info!("Server shutting down");
                break;
            }
        }
    }

    // Cleanup
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(&lock_path);
    Ok(())
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    state: Arc<DaemonState>,
) -> Result<(), Box<dyn std::error::Error>> {
    let (read, write) = stream.into_split();
    let (reply_tx, reply_rx) = mpsc::unbounded_channel();
    let writer = tokio::spawn(write_replies(write, reply_rx));
    let mut reader = BufReader::new(read);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            break; // client disconnected
        }

        let request: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(e) => {
                let _ = reply_tx.send(json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("Parse error: {}", e) }
                }));
                continue;
            }
        };

        // A task per request, so a long command_run holds up no other call.
        // The task runs to its end even when the client leaves: cancelled
        // midway, a tmux command would put the control-mode replies out of step.
        let state = state.clone();
        let reply_tx = reply_tx.clone();
        tokio::spawn(async move {
            let _ = reply_tx.send(answer(&request, &state).await);
        });
    }

    // The writer ends once every request of this client has its reply.
    drop(reply_tx);
    writer.await??;
    Ok(())
}

async fn answer(request: &Value, state: &Arc<DaemonState>) -> Value {
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request["method"].as_str().unwrap_or("");
    let params = request.get("params").cloned().unwrap_or(json!({}));

    match rpc::dispatch(method, &params, state).await {
        Ok(result) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        }),
        Err(err) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": err.to_json(),
        }),
    }
}

/// Writes each reply as it comes, in the order the requests finish; the id
/// in each reply tells the client which request it answers.
async fn write_replies(mut write: OwnedWriteHalf, mut replies: mpsc::UnboundedReceiver<Value>) -> std::io::Result<()> {
    while let Some(reply) = replies.recv().await {
        let mut line = reply.to_string();
        line.push('\n');
        write.write_all(line.as_bytes()).await?;
        write.flush().await?;
    }
    Ok(())
}
