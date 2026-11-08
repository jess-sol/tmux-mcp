//! What holds between the hook's "ask" and the `command_run` it approves.

use crate::harness::Decision::Deny;
use crate::harness::Harness;

/// The hook refuses a pane that cannot take a command, as command_run would,
/// so no approval is taken for a context the person never saw.
#[tokio::test]
async fn the_hook_refuses_a_busy_pane() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "sleep 30").await;

    let (decision, reason) = h.hook(&shell, "seq 1");

    assert_eq!(decision, Deny);
    assert!(reason.contains("busy") && reason.contains("sleep 30"), "{reason}");
}
