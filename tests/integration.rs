//! Integration tests: exercise the real daemon against an isolated tmux server.
//!
//! Each test gets its own tmux server (via `-L`), session, and daemon.
//! Run with: cargo test --test integration -- --test-threads=1

use std::process::Command as StdCommand;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use tmux_mcp::client::{self, DaemonClient};

static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);
static TRACING_INIT: std::sync::Once = std::sync::Once::new();

fn init_tracing() {
    TRACING_INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("debug")),
            )
            .with_test_writer()
            .init();
    });
}

// --- Test harness ---

struct TestDaemon {
    name: String,
    client: Option<DaemonClient>,
    daemon_handle: Option<JoinHandle<()>>,
    origin_pane: String,
}

impl TestDaemon {
    /// Start an isolated tmux server, session, and daemon.
    async fn start() -> Self {
        init_tracing();
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("tmux-mcp-test-{}-{}", std::process::id(), id);

        // Create isolated tmux server + session
        let status = StdCommand::new("tmux")
            .args(["-L", &name, "new-session", "-d", "-s", &name, "-x", "80", "-y", "24"])
            .status()
            .expect("failed to start tmux");
        assert!(status.success(), "tmux new-session failed: {}", status);

        // Get the pane ID
        let output = StdCommand::new("tmux")
            .args(["-L", &name, "display-message", "-t", &name, "-p", "#{pane_id}"])
            .output()
            .expect("failed to query pane_id");
        let origin_pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(origin_pane.starts_with('%'), "unexpected pane_id: {}", origin_pane);

        // Spawn daemon in-process
        let daemon_name = name.clone();
        let daemon_handle = tokio::spawn(async move {
            if let Err(e) = tmux_mcp::daemon::run(&daemon_name, Some(&daemon_name)).await {
                eprintln!("Daemon error: {}", e);
            }
        });

        // Poll-connect to daemon socket
        let sock = client::socket_path(&name);
        let mut client = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Ok(c) = DaemonClient::connect_to_socket(&sock).await {
                    return c;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("timed out waiting for daemon socket");

        // Prime bash-preexec: the first command after shell startup doesn't
        // get preexec hooks (bash-preexec needs one prompt cycle to set
        // __bp_preexec_interactive_mode). Send a throwaway command to prime it,
        // then wait for it to complete via completion_seq.
        {
            let mut p = serde_json::Map::new();
            p.insert("origin_pane".into(), json!(origin_pane));
            p.insert("pane_id".into(), json!(origin_pane));
            p.insert("command".into(), json!(" :"));
            p.insert("timeout_secs".into(), json!(5));
            let _ = client.request("command_run", Value::Object(p)).await;
        }

        TestDaemon {
            name,
            client: Some(client),
            daemon_handle: Some(daemon_handle),
            origin_pane,
        }
    }

    /// Send an RPC request with origin_pane injected.
    async fn rpc(&mut self, method: &str, params: Value) -> Value {
        let mut p = params.as_object().cloned().unwrap_or_default();
        p.insert("origin_pane".to_string(), json!(self.origin_pane));
        self.client
            .as_mut()
            .unwrap()
            .request(method, Value::Object(p))
            .await
            .unwrap_or_else(|e| panic!("RPC {} failed: {}", method, e))
    }

    /// Graceful cleanup: abort daemon, kill tmux server.
    async fn cleanup(mut self) {
        drop(self.client.take());
        if let Some(h) = self.daemon_handle.take() {
            h.abort();
            let _ = h.await;
        }
        self.kill_tmux_server();
    }

    fn kill_tmux_server(&self) {
        let _ = StdCommand::new("tmux")
            .args(["-L", &self.name, "kill-server"])
            .status();
        let _ = std::fs::remove_file(client::socket_path(&self.name));
        let _ = std::fs::remove_file(client::lock_path(&self.name));
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        // Safety net: ensure tmux server is killed even on panic
        self.kill_tmux_server();
    }
}

// --- Helper ---

/// Run a test body with a timeout.
async fn with_timeout<F: std::future::Future<Output = ()>>(f: F) {
    tokio::time::timeout(Duration::from_secs(10), f)
        .await
        .expect("test timed out after 10s");
}

// --- Tests ---

#[tokio::test(flavor = "multi_thread")]
async fn test_daemon_bootstrap() {
    with_timeout(async {
        let td = TestDaemon::start().await;
        // If we got here, daemon bootstrapped and we connected successfully
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_list_panes() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td.rpc("list_panes", json!({})).await;
        let panes = result.as_array().expect("list_panes should return array");
        assert!(!panes.is_empty(), "should have at least one pane");

        // Our origin pane should be in the list
        let has_origin = panes
            .iter()
            .any(|p| p["pane_id"].as_str() == Some(&td.origin_pane));
        assert!(has_origin, "origin pane {} not found in {:?}", td.origin_pane, panes);

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_simple() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "echo hello", "timeout_secs": 5}),
            )
            .await;

        let output = result["output"].as_str().unwrap_or("");
        assert!(output.contains("hello"), "output should contain 'hello', got: {:?}", output);
        assert_eq!(result["exit_code"], json!(0));

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_exit_code() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "false", "timeout_secs": 5}),
            )
            .await;

        assert_eq!(result["exit_code"], json!(1));

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_history() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Run two commands
        td.rpc(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "echo first", "timeout_secs": 5}),
        )
        .await;

        td.rpc(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "echo second", "timeout_secs": 5}),
        )
        .await;

        let result = td
            .rpc(
                "command_history",
                json!({"pane_id": td.origin_pane.clone(), "count": 10}),
            )
            .await;
        let history = result.as_array().expect("history should be array");
        assert!(history.len() >= 2, "should have at least 2 commands, got {}", history.len());

        let newest = history[0]["command"].as_str().unwrap_or("");
        assert!(newest.contains("echo second"), "newest should be 'echo second', got: {:?}", newest);

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_read() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        td.rpc(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "echo readtest", "timeout_secs": 5}),
        )
        .await;

        let result = td
            .rpc(
                "command_read",
                json!({"pane_id": td.origin_pane.clone(), "count": 1}),
            )
            .await;
        let cmds = result.as_array().expect("command_read should return array");
        assert!(!cmds.is_empty());
        let output = cmds[0]["output"].as_str().unwrap_or("");
        assert!(output.contains("readtest"), "output should contain 'readtest', got: {:?}", output);

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_multiline_output() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "seq 5", "timeout_secs": 5}),
            )
            .await;

        let output = result["output"].as_str().unwrap_or("");
        for n in 1..=5 {
            assert!(output.contains(&n.to_string()), "output should contain '{}', got: {:?}", n, output);
        }

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_subshell() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "(echo sub)", "timeout_secs": 5}),
            )
            .await;

        let output = result["output"].as_str().unwrap_or("");
        assert!(output.contains("sub"));

        td.cleanup().await;
    })
    .await;
}
