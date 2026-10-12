//! Which daemon serves a tmux session when two builds of tmux-mcp meet: the
//! newest runs, and an older client refuses it.

use crate::harness::{Decision, Harness};

/// A newer client retires an older daemon and serves from its own; the older
/// server's connection ends with the daemon it had.
#[tokio::test]
async fn a_newer_client_replaces_an_older_daemon() {
    let h = Harness::claiming("0.0.1").await;
    let shell = h.shell().await;

    let (decision, reason) = h.hook(&shell, "ls");
    let older = h.debug_pane(&shell).await;

    // A failed connection denies; any other decision is the new daemon's.
    // Fresh, it has seen no directory for the pane yet, so `ls` may ask.
    assert_ne!(decision, Decision::Deny, "{reason}");
    assert!(older.contains("Daemon closed connection"), "{older}");
}

/// An older client leaves a newer daemon running and says why it stops.
#[tokio::test]
async fn an_older_client_refuses_a_newer_daemon() {
    let h = Harness::claiming("99.0.0").await;
    let shell = h.shell().await;

    let (decision, reason) = h.hook(&shell, "ls");
    let screen = h.debug_pane(&shell).await;

    assert_eq!(decision, Decision::Deny);
    assert!(
        reason.contains("99.0.0") && reason.contains(env!("CARGO_PKG_VERSION")),
        "{reason}"
    );
    assert!(screen.contains('$'), "{screen}");
}
