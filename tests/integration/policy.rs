//! What the policy hook tells Claude Code before a command runs.

use crate::harness::{Decision, Harness};

#[tokio::test]
#[ignore = "harness: red — in_project compares path strings, not path components"]
async fn a_directory_whose_name_extends_the_project_is_outside_it() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let beside = format!("{}-evil/notes", h.dir().display());

    let decision = h.decision(&shell, &format!("rm {beside}"));

    assert!(matches!(decision, Decision::Ask), "{decision:?}");
}

#[tokio::test]
#[ignore = "harness: red — a quoted command name escapes its rule"]
async fn quoting_eval_does_not_escape_its_denial() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let decision = h.decision(&shell, "'eval' true");

    assert!(matches!(decision, Decision::Deny { .. }), "{decision:?}");
}

#[tokio::test]
#[ignore = "harness: red — command substitutions inside assignments go unseen"]
async fn eval_inside_an_assignment_is_seen() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let decision = h.decision(&shell, "FOO=$(eval true) ls");

    assert!(matches!(decision, Decision::Deny { .. }), "{decision:?}");
}
