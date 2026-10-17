//! The Pi contract: `policy-check` speaks the neutral decision object, and
//! `ensure-target` names a safe pane.

use serde_json::{Value, json};

use crate::harness::Harness;

/// With `TMUX_MCP_HARNESS=pi`, the hook answers a bare `tool_input` payload
/// with `{"decision", "rule", "context"}` — no Claude envelope.
#[tokio::test]
async fn the_pi_hook_answers_with_the_neutral_decision_object() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let pi = [("TMUX_MCP_HARNESS", "pi".to_string())];

    let input = json!({"tool_input": {"pane_id": shell.id(), "command": "ls"}});
    let out = h.hook_raw(&pi, input);
    let answer: Value = serde_json::from_str(&out).expect("the hook's JSON");

    assert_eq!(answer["decision"], json!("allow"), "{answer}");
    assert!(answer.get("hookSpecificOutput").is_none(), "{answer}");
    assert!(answer["context"].is_object(), "{answer}");

    // A deny names its rule in the same object shape.
    let input = json!({"tool_input": {"pane_id": shell.id(), "command": "eval x"}});
    let out = h.hook_raw(&pi, input);
    let answer: Value = serde_json::from_str(&out).expect("the hook's JSON");

    assert_eq!(answer["decision"], json!("deny"), "{answer}");
    assert!(
        answer["rule"].as_str().unwrap_or("").contains("eval"),
        "{answer}"
    );
}

/// Without an override, `PI_*` variables in the environment select the Pi
/// contract; `--claude` forces the Claude envelope.
#[tokio::test]
async fn the_hook_detects_pi_from_the_environment() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let input = json!({"tool_input": {"pane_id": shell.id(), "command": "ls"}});
    let out = h.hook_raw(&[("PI_SESSION_ID", "test".to_string())], input.clone());
    assert!(out.contains("\"decision\""), "{out}");

    let out = h.hook_raw(
        &[("PI_SESSION_ID", "test".to_string())],
        json!({"tool_name": "mcp__tmux-mcp__command_run", "tool_input": input["tool_input"]}),
    );
    // Pi detection wins over the Claude-shaped payload: still no envelope.
    assert!(out.contains("\"decision\""), "{out}");
}

/// `ensure-target` reports the existing non-origin pane of the window.
#[tokio::test]
async fn ensure_target_names_an_existing_pane() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let target = h.ensure_target(&["--json"]).expect("a target");

    assert_eq!(target["pane_id"], json!(shell.id()), "{target}");
    assert_eq!(target["created"], json!(false), "{target}");
}

/// With `--no-create` and only the origin pane in the window, `ensure-target`
/// refuses rather than naming the origin pane.
#[tokio::test]
async fn ensure_target_never_names_the_origin_pane() {
    let h = Harness::new().await;

    let err = h
        .ensure_target(&["--json", "--no-create"])
        .expect_err("no non-origin pane exists");

    assert!(err.contains("No pane other than the origin pane"), "{err}");
}
