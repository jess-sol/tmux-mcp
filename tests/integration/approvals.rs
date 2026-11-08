//! What holds between the hook's "ask" and the `command_run` it approves.

use crate::harness::Decision::{Ask, Deny};
use crate::harness::Harness;

/// Each command the hook asks about in a pane keeps its own approval: two
/// asks in flight for one pane both run.
#[tokio::test]
async fn each_command_asked_for_a_pane_keeps_its_own_approval() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    std::fs::create_dir_all(h.dir().join("a")).expect("dir a");
    std::fs::create_dir_all(h.dir().join("b")).expect("dir b");
    assert_eq!(h.hook(&shell, "rm -r a").0, Ask);
    assert_eq!(h.hook(&shell, "rm -r b").0, Ask);

    let first = h.run_approved(&shell, "rm -r a").await;
    let second = h.run_approved(&shell, "rm -r b").await;

    assert!(first.ends_with("Exit code: 0"), "{first}");
    assert!(second.ends_with("Exit code: 0"), "{second}");
}

/// A command that needs an approval and has none says so, and names the
/// hook as the way to get one.
#[tokio::test]
async fn a_command_without_an_approval_says_none_is_pending() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    std::fs::create_dir_all(h.dir().join("c")).expect("dir c");

    let reply = h.run_approved(&shell, "rm -r c").await;

    assert!(
        reply.contains("no approval is pending") && reply.contains("hook"),
        "{reply}"
    );
}

/// A command other than the one the person approved names the approved
/// one, and points at no hook: the hook is there.
#[tokio::test]
async fn a_command_other_than_the_approved_one_names_it() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    std::fs::create_dir_all(h.dir().join("e")).expect("dir e");
    assert_eq!(h.hook(&shell, "rm -r e").0, Ask);

    let reply = h.run_approved(&shell, "rm -rf e").await;

    assert!(
        reply.contains("approved a different command") && reply.contains("\"rm -r e\""),
        "{reply}"
    );
    assert!(!reply.contains("install"), "{reply}");
    assert!(h.dir().join("e").is_dir(), "nothing ran");
}

/// An approval is for the pane as the person saw it: a cwd that changed
/// since is named, and no command runs.
#[tokio::test]
async fn an_approval_for_another_cwd_names_the_change() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    std::fs::create_dir_all(h.dir().join("sub")).expect("dir sub");
    std::fs::create_dir_all(h.dir().join("d")).expect("dir d");
    assert_eq!(h.hook(&shell, "rm -r d").0, Ask);
    h.run(&shell, "cd sub").await;

    let reply = h.run_approved(&shell, "rm -r d").await;

    assert!(
        reply.contains("cwd") && reply.contains("changed since the approval"),
        "{reply}"
    );
    assert!(h.dir().join("d").is_dir(), "nothing ran");
}

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
