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

    /// Run a command and return (output, exit_code).
    async fn run(&mut self, command: &str) -> (String, Option<i64>) {
        let result = self
            .rpc(
                "command_run",
                json!({"pane_id": self.origin_pane.clone(), "command": command, "timeout_secs": 5}),
            )
            .await;
        let output = result["output"].as_str().unwrap_or("").to_string();
        let exit_code = result["exit_code"].as_i64();
        (output, exit_code)
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
        let panes = result["panes"].as_array().expect("list_panes should return panes array");
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

// --- Compound commands ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_multi_statement_subshell() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, exit_code) = td.run("(echo aaa; echo bbb; echo ccc)").await;
        assert!(output.contains("aaa"), "missing aaa: {:?}", output);
        assert!(output.contains("bbb"), "missing bbb: {:?}", output);
        assert!(output.contains("ccc"), "missing ccc: {:?}", output);
        assert_eq!(exit_code, Some(0));
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_nested_subshell() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run("(echo outer; (echo inner))").await;
        assert!(output.contains("outer"), "missing outer: {:?}", output);
        assert!(output.contains("inner"), "missing inner: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_brace_group() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run("{ echo aaa; echo bbb; }").await;
        assert!(output.contains("aaa"), "missing aaa: {:?}", output);
        assert!(output.contains("bbb"), "missing bbb: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_for_loop() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run("for i in x y z; do echo $i; done").await;
        assert!(output.contains("x"), "missing x: {:?}", output);
        assert!(output.contains("y"), "missing y: {:?}", output);
        assert!(output.contains("z"), "missing z: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

// --- Pipes and substitution ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_pipe_chain() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run("echo hello | tr a-z A-Z | sed 's/H/h/'").await;
        assert!(output.contains("hELLO"), "expected hELLO: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_command_substitution() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run("echo $(echo inner)").await;
        assert!(output.contains("inner"), "missing inner: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

// --- Heredoc ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_heredoc() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, exit_code) = td.run("cat <<'EOF'\nhello heredoc\nworld\nEOF").await;
        assert!(output.contains("hello heredoc"), "missing 'hello heredoc': {:?}", output);
        assert!(output.contains("world"), "missing 'world': {:?}", output);
        assert_eq!(exit_code, Some(0));
        td.cleanup().await;
    })
    .await;
}

// --- Large output ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_large_output() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run("seq 1 500").await;
        assert!(output.contains("1"), "missing 1");
        assert!(output.contains("250"), "missing 250");
        assert!(output.contains("500"), "missing 500");
        td.cleanup().await;
    })
    .await;
}

// --- Edge cases ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_no_output() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, exit_code) = td.run("true").await;
        // `true` produces no visible output, but capture may contain
        // control characters from prompt wrapping (e.g., \x01/\x02).
        let visible: String = output.chars().filter(|c| !c.is_control()).collect();
        assert!(visible.trim().is_empty(), "expected no visible output: {:?}", output);
        assert_eq!(exit_code, Some(0));
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_stderr_only() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, exit_code) = td.run("echo err >&2").await;
        // Stderr goes to terminal — it should appear in capture since
        // the terminal doesn't distinguish stdout from stderr.
        assert!(output.contains("err"), "stderr should be captured: {:?}", output);
        assert_eq!(exit_code, Some(0));
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_ansi_in_output() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run("printf '\\033[31mred\\033[0m\\n'").await;
        // Output contains the raw escape sequences (capture is raw terminal bytes)
        assert!(output.contains("red"), "missing 'red': {:?}", output);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_special_chars_in_output() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (output, _) = td.run(r#"echo 'it'\''s "quoted"'"#).await;
        assert!(output.contains("it's"), "missing it's: {:?}", output);
        assert!(output.contains("\"quoted\""), "missing quoted: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

// --- Exit codes ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_exit_code_2() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (_, exit_code) = td.run("bash -c 'exit 2'").await;
        assert_eq!(exit_code, Some(2));
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_signal_exit() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let (_, exit_code) = td.run("bash -c 'kill -TERM $$'").await;
        // SIGTERM = 15, exit code = 128 + 15 = 143
        assert_eq!(exit_code, Some(143));
        td.cleanup().await;
    })
    .await;
}

// --- Rapid sequential ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_rapid_sequential() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        let (out1, _) = td.run("echo rapid1").await;
        let (out2, _) = td.run("echo rapid2").await;
        let (out3, _) = td.run("echo rapid3").await;
        let (out4, _) = td.run("echo rapid4").await;
        let (out5, _) = td.run("echo rapid5").await;

        assert!(out1.contains("rapid1"), "missing rapid1: {:?}", out1);
        assert!(out2.contains("rapid2"), "missing rapid2: {:?}", out2);
        assert!(out3.contains("rapid3"), "missing rapid3: {:?}", out3);
        assert!(out4.contains("rapid4"), "missing rapid4: {:?}", out4);
        assert!(out5.contains("rapid5"), "missing rapid5: {:?}", out5);

        td.cleanup().await;
    })
    .await;
}

// --- capture_pane ---

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_pane() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Run a command so there's something on screen
        td.run("echo capture-test-marker").await;

        let result = td
            .rpc("capture_pane", json!({"pane_id": td.origin_pane.clone(), "lines": 50}))
            .await;
        let text = result["text"].as_str().unwrap_or("");
        assert!(
            text.contains("capture-test-marker"),
            "capture should contain marker, got: {:?}",
            text
        );

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_capture_pane_shows_prompt() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // After warm-up, screen should show a prompt with $
        let result = td
            .rpc("capture_pane", json!({"pane_id": td.origin_pane.clone()}))
            .await;
        let text = result["text"].as_str().unwrap_or("");
        assert!(
            text.contains('$'),
            "capture should show prompt, got: {:?}",
            text
        );

        td.cleanup().await;
    })
    .await;
}

// --- list_panes osc133 field ---

#[tokio::test(flavor = "multi_thread")]
async fn test_list_panes_osc133_marker() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // After warm-up, markers should have been seen
        let result = td.rpc("list_panes", json!({})).await;
        let panes = result["panes"].as_array().expect("should be array");
        let our_pane = panes
            .iter()
            .find(|p| p["pane_id"].as_str() == Some(&td.origin_pane))
            .expect("origin pane should be in list");

        let secs = our_pane["osc133_last_marker_secs"].as_f64();
        assert!(secs.is_some(), "osc133_last_marker_secs should be set after warm-up");
        assert!(
            secs.unwrap() < 10.0,
            "marker should be recent, got: {:?}s",
            secs
        );

        td.cleanup().await;
    })
    .await;
}

// --- OSC 133 gating ---

#[tokio::test(flavor = "multi_thread")]
async fn test_list_panes_osc133_status_confirmed() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // After warm-up + command_run, the shell PID should be confirmed
        td.run("echo gating-test").await;

        let result = td.rpc("list_panes", json!({})).await;
        let panes = result["panes"].as_array().expect("should be array");
        let our_pane = panes
            .iter()
            .find(|p| p["pane_id"].as_str() == Some(&td.origin_pane))
            .expect("origin pane should be in list");

        assert_eq!(
            our_pane["osc133_status"].as_str(),
            Some("confirmed"),
            "should be confirmed after successful command_run"
        );

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_inject_skips_if_already_active() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Warm-up confirms the shell. Now inject should detect already_active.
        td.run("echo primed").await;

        let result = td
            .rpc("inject_osc133", json!({"pane_id": td.origin_pane.clone()}))
            .await;

        assert_eq!(
            result["status"].as_str(),
            Some("already_active"),
            "inject should detect existing markers: {:?}",
            result
        );

        td.cleanup().await;
    })
    .await;
}

// --- send_keys ---

#[tokio::test(flavor = "multi_thread")]
async fn test_send_keys_ctrl_c() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Start a long-running command — expect it to timeout/fail
        let _ = td.client.as_mut().unwrap().request(
            "command_run",
            json!({
                "origin_pane": td.origin_pane,
                "pane_id": td.origin_pane.clone(),
                "command": "sleep 100",
                "timeout_secs": 1,
            }),
        ).await;

        // Send Ctrl+C to cancel it
        let result = td
            .rpc("send_keys", json!({"pane_id": td.origin_pane.clone(), "keys": "C-c"}))
            .await;

        let screen = result["screen"].as_str().unwrap_or("");
        assert!(
            screen.contains('$'),
            "screen should show prompt after Ctrl+C: {:?}",
            screen
        );

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_send_keys_returns_screen() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Send Enter to get a fresh prompt
        let result = td
            .rpc("send_keys", json!({"pane_id": td.origin_pane.clone(), "keys": "Enter"}))
            .await;

        let screen = result["screen"].as_str().unwrap_or("");
        assert!(!screen.is_empty(), "send_keys should return screen capture");

        td.cleanup().await;
    })
    .await;
}
