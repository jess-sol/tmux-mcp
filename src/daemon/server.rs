/// Unix socket server: accepts daemon clients, handles ndjson JSON-RPC.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;

use crate::client;
use crate::daemon::rpc::{self, DaemonState};

/// Run the Unix socket server. Blocks until cancelled.
pub async fn serve(
    session: &str,
    state: Arc<DaemonState>,
    cancel: CancellationToken,
) -> Result<(), Box<dyn std::error::Error>> {
    let sock_path = client::socket_path(session);

    // Acquire lock file to prevent duplicate daemons
    let lock_path = client::lock_path(session);
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
    let (read, mut write) = stream.into_split();
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
                let err_resp = json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": { "code": -32700, "message": format!("Parse error: {}", e) }
                });
                let mut resp_line = serde_json::to_string(&err_resp)?;
                resp_line.push('\n');
                write.write_all(resp_line.as_bytes()).await?;
                continue;
            }
        };

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let method = request["method"].as_str().unwrap_or("");
        let params = request.get("params").cloned().unwrap_or(json!({}));

        let response = match rpc::dispatch(method, &params, &state).await {
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
        };

        let mut resp_line = serde_json::to_string(&response)?;
        resp_line.push('\n');
        write.write_all(resp_line.as_bytes()).await?;
        write.flush().await?;
    }

    Ok(())
}
