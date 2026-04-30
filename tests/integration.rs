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

    /// Send an RPC request, returning the error message on failure.
    async fn rpc_err(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let mut p = params.as_object().cloned().unwrap_or_default();
        p.insert("origin_pane".to_string(), json!(self.origin_pane));
        self.client
            .as_mut()
            .unwrap()
            .request(method, Value::Object(p))
            .await
            .map_err(|e| e.to_string())
    }

    /// Run a command and return (output, exit_code).
    /// Calls request_approval first to simulate the hook flow.
    async fn run(&mut self, command: &str) -> (String, Option<i64>) {
        // Simulate hook: request approval so command_run can verify it
        let _ = self
            .rpc(
                "request_approval",
                json!({"pane_id": self.origin_pane.clone(), "command": command}),
            )
            .await;

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

    /// Type literal text into the pane (via tmux send-keys -l).
    fn type_text(&self, text: &str) {
        let status = StdCommand::new("tmux")
            .args(["-L", &self.name, "send-keys", "-l", "-t", &self.origin_pane, text])
            .status()
            .expect("failed to send literal keys");
        assert!(status.success(), "tmux send-keys -l failed");
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
                json!({"pane_id": td.origin_pane.clone()}),
            )
            .await;
        let output = result["output"].as_str().unwrap_or("");
        assert!(output.contains("readtest"), "output should contain 'readtest', got: {:?}", output);
        assert_eq!(result["status"].as_str(), Some("completed"));

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

// --- Read params (next/head/tail/search) ---

#[tokio::test(flavor = "multi_thread")]
async fn test_head_returns_first_n_lines() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "seq 1 10", "timeout_secs": 5, "head": 3}),
            )
            .await;
        let output = result["output"].as_str().unwrap_or("");
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 3, "head=3 should return 3 lines, got: {:?}", lines);
        assert!(output.contains("1") && output.contains("2") && output.contains("3"),
            "should contain first 3 lines, got: {:?}", output);
        assert!(!output.contains("4"), "should not contain line 4, got: {:?}", output);
        assert!(result["total_lines"].as_u64().unwrap_or(0) >= 10,
            "total_lines should reflect full output");
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tail_returns_last_n_lines() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "seq 1 10", "timeout_secs": 5, "tail": 3}),
            )
            .await;
        let output = result["output"].as_str().unwrap_or("");
        assert!(output.contains("8") && output.contains("9") && output.contains("10"),
            "should contain last 3 lines, got: {:?}", output);
        assert!(!output.contains("\n7\n") && !output.starts_with("7\n"),
            "should not contain line 7, got: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_next_advances_cursor_across_calls() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // First call: next=3 → lines 1,2,3
        let r1 = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "seq 1 10", "timeout_secs": 5, "next": 3}),
            )
            .await;
        let o1 = r1["output"].as_str().unwrap_or("");
        assert!(o1.contains("1") && o1.contains("3"), "first window: {:?}", o1);
        assert!(!o1.contains("4"), "first window should not have 4: {:?}", o1);

        // Second call: next=3 → lines 4,5,6
        let r2 = td
            .rpc(
                "command_read",
                json!({"pane_id": td.origin_pane.clone(), "next": 3}),
            )
            .await;
        let o2 = r2["output"].as_str().unwrap_or("");
        assert!(o2.contains("4") && o2.contains("6"), "second window: {:?}", o2);
        assert!(!o2.contains("3") && !o2.contains("7"), "second window bounds: {:?}", o2);

        // Third call: next=100 → remainder (7..10)
        let r3 = td
            .rpc(
                "command_read",
                json!({"pane_id": td.origin_pane.clone(), "next": 100}),
            )
            .await;
        let o3 = r3["output"].as_str().unwrap_or("");
        assert!(o3.contains("7") && o3.contains("10"), "remainder: {:?}", o3);
        assert!(!o3.contains("6"), "remainder should not have 6: {:?}", o3);

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_head_does_not_advance_cursor() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // head=2 returns first 2 lines but should NOT move cursor
        td.rpc(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "seq 1 10", "timeout_secs": 5, "head": 2}),
        )
        .await;

        // next=3 should start from line 1, not line 3
        let r = td
            .rpc(
                "command_read",
                json!({"pane_id": td.origin_pane.clone(), "next": 3}),
            )
            .await;
        let output = r["output"].as_str().unwrap_or("");
        assert!(output.contains("1") && output.contains("3"),
            "next after head should start from 0, got: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_tail_does_not_advance_cursor() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // tail=2 returns last 2 lines but should NOT move cursor
        td.rpc(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "seq 1 10", "timeout_secs": 5, "tail": 2}),
        )
        .await;

        // next=3 should start from line 1, not from the end
        let r = td
            .rpc(
                "command_read",
                json!({"pane_id": td.origin_pane.clone(), "next": 3}),
            )
            .await;
        let output = r["output"].as_str().unwrap_or("");
        assert!(output.contains("1") && output.contains("3"),
            "next after tail should start from 0, got: {:?}", output);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_next_past_end_returns_empty() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Consume all output
        td.rpc(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "seq 1 3", "timeout_secs": 5, "next": 3}),
        )
        .await;

        // Next call should return empty
        let r = td
            .rpc(
                "command_read",
                json!({"pane_id": td.origin_pane.clone(), "next": 10}),
            )
            .await;
        let output = r["output"].as_str().unwrap_or("");
        assert!(output.is_empty(), "next past end should be empty, got: {:?}", output);
        assert_eq!(r["status"].as_str(), Some("completed"));
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_search_filters_matching_lines() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({
                    "pane_id": td.origin_pane.clone(),
                    "command": "printf '1 apple\\n2 banana\\n3 apple pie\\n4 cherry\\n'",
                    "timeout_secs": 5,
                    "search": "apple"
                }),
            )
            .await;
        let output = result["output"].as_str().unwrap_or("");
        assert!(output.contains("apple"), "should contain apple lines: {:?}", output);
        assert!(!output.contains("banana") && !output.contains("cherry"),
            "should not contain non-matching lines: {:?}", output);
        assert_eq!(result["search_matches"].as_u64(), Some(2),
            "search_matches should be 2");
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_search_combined_with_tail() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc(
                "command_run",
                json!({
                    "pane_id": td.origin_pane.clone(),
                    "command": "seq 1 20",
                    "timeout_secs": 5,
                    "tail": 10,
                    "search": "1"
                }),
            )
            .await;
        let output = result["output"].as_str().unwrap_or("");
        // tail=10 selects lines 11-20; search="1" filters to lines containing "1"
        // Lines 11-19 all contain "1", line 20 does not
        assert!(output.contains("11"), "should include 11: {:?}", output);
        assert!(!output.contains("20"), "20 doesn't match '1': {:?}", output);
        // Lines 1-10 are outside the tail window
        assert!(!output.split('\n').any(|l| l.trim() == "1"),
            "line '1' is outside tail window: {:?}", output);
        let matches = result["search_matches"].as_u64().unwrap_or(0);
        assert!(matches >= 9, "should have at least 9 matches (11-19), got: {}", matches);
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_next_and_head_mutually_exclusive() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;
        let result = td
            .rpc_err(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "seq 1 5", "timeout_secs": 5, "next": 2, "head": 2}),
            )
            .await;
        assert!(result.is_err(), "should reject mutually exclusive params, got: {:?}", result);
        let err = result.unwrap_err();
        assert!(err.contains("mutually exclusive"), "error should explain: {:?}", err);
        td.cleanup().await;
    })
    .await;
}

// --- debug_pane ---

#[tokio::test(flavor = "multi_thread")]
async fn test_debug_pane_shows_redirect_when_command_active() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Start a long command that will timeout
        let run_result = td
            .rpc(
                "command_run",
                json!({"pane_id": td.origin_pane.clone(), "command": "sleep 30", "timeout_secs": 1}),
            )
            .await;
        let command_id = run_result["command_id"].as_u64().expect("should have command_id");

        // capture_pane should include active_command metadata
        let cap = td
            .rpc("capture_pane", json!({"pane_id": td.origin_pane.clone(), "lines": 50}))
            .await;
        assert!(cap["active_command"].is_object(),
            "should have active_command when command is running, got: {:?}", cap);
        assert_eq!(cap["active_command"]["command_id"].as_u64(), Some(command_id),
            "active_command should match the running command");

        // Clean up
        td.rpc("send_keys", json!({"pane_id": td.origin_pane.clone(), "keys": "C-c"})).await;
        tokio::time::sleep(Duration::from_millis(500)).await;
        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_debug_pane_no_redirect_when_idle() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Run a command that completes
        td.run("echo done").await;

        let cap = td
            .rpc("capture_pane", json!({"pane_id": td.origin_pane.clone(), "lines": 50}))
            .await;
        assert!(cap["active_command"].is_null(),
            "should not have active_command when idle, got: {:?}", cap["active_command"]);
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

// --- list_panes OSC 7 fields ---

#[tokio::test(flavor = "multi_thread")]
async fn test_list_panes_osc_user() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // After warmup, the shell's precmd has emitted OSC 7 with user@host/path
        let result = td.rpc("list_panes", json!({})).await;
        let pane = result["panes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["pane_id"].as_str() == Some(&td.origin_pane))
            .expect("origin pane should be in list");

        let osc_user = pane["osc_user"].as_str();
        assert!(
            osc_user.is_some(),
            "osc_user should be set after warmup, got: {:?}",
            pane
        );
        assert!(
            !osc_user.unwrap().is_empty(),
            "osc_user should not be empty"
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

// --- press_key ---

#[tokio::test(flavor = "multi_thread")]
async fn test_press_key_ctrl_c() {
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
async fn test_press_key_returns_screen() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Send Enter to get a fresh prompt
        let result = td
            .rpc("send_keys", json!({"pane_id": td.origin_pane.clone(), "keys": "Enter"}))
            .await;

        let screen = result["screen"].as_str().unwrap_or("");
        assert!(!screen.is_empty(), "press_key should return screen capture");

        td.cleanup().await;
    })
    .await;
}

// --- Pre-execution guards ---

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_rejects_when_busy() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Start a long-running command with a short timeout so we get control back
        let _ = td.rpc_err(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "sleep 30", "timeout_secs": 1}),
        ).await;

        // Now try to run another command — should be rejected as busy
        let result = td.rpc_err(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "echo should-fail", "timeout_secs": 5}),
        ).await;

        assert!(result.is_err(), "should reject when busy, got: {:?}", result);
        let err = result.unwrap_err();
        assert!(err.contains("busy"), "error should mention busy: {:?}", err);
        assert!(err.contains("sleep"), "error should mention the running command: {:?}", err);

        // Clean up: cancel the sleep
        td.rpc("send_keys", json!({"pane_id": td.origin_pane.clone(), "keys": "C-c"})).await;
        tokio::time::sleep(Duration::from_millis(500)).await;

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_rejects_when_typing() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Type some text without pressing Enter
        td.type_text("partial command");
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Try to run a command — should be rejected because user is typing
        let result = td.rpc_err(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "echo should-fail", "timeout_secs": 5}),
        ).await;

        assert!(result.is_err(), "should reject when user is typing, got: {:?}", result);
        let err = result.unwrap_err();
        assert!(err.contains("typing"), "error should mention typing: {:?}", err);

        // Clean up: clear the typed text
        td.rpc("send_keys", json!({"pane_id": td.origin_pane.clone(), "keys": "C-c"})).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        td.cleanup().await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn test_command_run_succeeds_after_backspace_clears_input() {
    with_timeout(async {
        let mut td = TestDaemon::start().await;

        // Type some text
        td.type_text("hi");
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Clear the line with Ctrl+U
        td.rpc("send_keys", json!({"pane_id": td.origin_pane.clone(), "keys": "C-u"})).await;
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Now command_run should succeed — no visible input
        let result = td.rpc_err(
            "command_run",
            json!({"pane_id": td.origin_pane.clone(), "command": "echo cleared-ok", "timeout_secs": 5}),
        ).await;

        assert!(result.is_ok(), "should succeed after input cleared: {:?}", result);
        let val = result.unwrap();
        assert!(val["output"].as_str().unwrap_or("").contains("cleared-ok"));

        td.cleanup().await;
    })
    .await;
}
