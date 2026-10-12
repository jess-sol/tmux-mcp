//! What a `Harness` promises: tmux-mcp serves only the harness's own tmux
//! server, and the `Shell` lives in the harness's own `HOME`, so nothing of
//! the developer's setup or history is in reach.

use crate::harness::Decision::{Allow, Ask, Deny, Pass};
use crate::harness::{Harness, decides};

#[tokio::test]
async fn tmux_mcp_serves_only_the_harness_and_its_home() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let dir = h.dir().display().to_string();

    let reply = h.run(&shell, "echo $HOME").await;
    let row = h.list_panes().await.row(&shell);

    assert!(reply.contains(&format!("{dir}/home")), "{reply}");
    assert!(row.contains(&dir), "{row}");
}

/// `decides!` takes 1 to 4 terms per command, `_` anywhere, and fails once
/// with every command that missed.
#[tokio::test]
async fn decides_matches_each_form_and_lists_every_miss() {
    let h = Harness::new().await;

    decides!(h, {
        "ls" => (Allow, Pass),
        "eval x" => (Deny, "eval"),
        "frobnicate" => (Ask, Pass),
        "kill -9 1" => (Ask, Ask, _),
        "cargo build" => (Allow, "cargo", Deny, "prefer-tmux"),
        "git push --force origin main" => (_, "git-push-force"),
    });
    let missed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        decides!(h, {
            "ls" => Deny,
            "eval x" => Deny,
            "frobnicate" => (Ask, "nope"),
        });
    }))
    .expect_err("two rows miss");

    let message = missed.downcast_ref::<String>().expect("a message");
    assert!(message.contains("\"ls\" =>"), "{message}");
    assert!(message.contains("\"frobnicate\" =>"), "{message}");
    assert!(!message.contains("\"eval x\" =>"), "{message}");
}
