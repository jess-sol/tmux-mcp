//! When `command_run` refuses to touch a pane.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use crate::harness::{Decision, Harness, SETTLE};

#[tokio::test]
async fn a_pane_the_user_is_typing_in_refuses_commands() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    shell.type_text("git sta");

    let reply = h.run(&shell, "ls").await;

    assert!(reply.starts_with("User is typing in pane"), "{reply}");
}

#[tokio::test]
async fn the_typing_refusal_suggests_ctrl_c_only_when_the_cursor_sits_outside_the_text() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    // Background jobs print at the idle prompt: a garbled line, with nothing
    // typed. `xyz` leaves the cursor before it, `abc` ends in a newline that
    // leaves the cursor below it. Octal keeps the text off the command line
    // itself; the sleep keeps `set -b` from redrawing.
    shell.say("(sleep 0.5; printf '\\170\\171\\172\\033[3D'; sleep 30) &");
    shell.wait_until(SETTLE, |s| s.contains("xyz")).unwrap();
    let before = h.run(&shell, "seq 1").await;
    h.press_key(&shell, "C-c").await;
    shell.say("(sleep 0.5; printf '\\141\\142\\143\\n'; sleep 30) &");
    shell
        .wait_until(SETTLE, |s| s.lines().any(|row| row.ends_with("$ abc")))
        .unwrap();
    let above = h.run(&shell, "seq 1").await;
    h.press_key(&shell, "C-c").await;
    shell.type_text("git sta");
    let typing = h.run(&shell, "seq 1").await;

    for garbled in [&before, &above] {
        assert!(
            garbled.starts_with("User is typing") && garbled.contains("press_key C-c"),
            "{garbled}"
        );
    }
    assert!(
        typing.starts_with("User is typing")
            && typing.contains("Read as input: \"git sta\"")
            && !typing.contains("press_key C-c"),
        "{typing}"
    );
}

#[tokio::test]
async fn a_cleared_input_line_takes_commands_again() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    shell.type_text("hi");
    h.press_key(&shell, "C-u").await;

    let reply = h.run(&shell, "seq 1").await;

    assert!(reply.ends_with("1\n\nExit code: 0"), "{reply:?}");
}

#[tokio::test]
async fn two_commands_sent_to_one_pane_at_once_run_one_at_a_time() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let (first, second) = tokio::join!(
        h.run(&shell, "sleep 1; seq 1"),
        h.run(&shell, "sleep 1; seq 2")
    );

    // Whichever reads the pane after the other's send sees it busy; one that
    // read it before finds it changed at its own send.
    let replies = [&first, &second];
    let ran = replies
        .iter()
        .filter(|r| r.contains("Exit code: 0"))
        .count();
    let refused = replies
        .iter()
        .filter(|r| r.contains("busy") || r.contains("changed before the send"))
        .count();
    assert_eq!((ran, refused), (1, 1), "{first:?}\n{second:?}");
}

/// Keys sent to a pane in copy mode drive copy mode, not the shell; tmux
/// announces each change of mode, so the pane takes commands again once the
/// person leaves it.
#[tokio::test]
async fn a_pane_in_copy_mode_says_so_and_refuses_commands_until_it_leaves() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    shell.copy_mode();

    let refused = h.run(&shell, "seq 1").await;
    let row = h.list_panes().await.row(&shell);
    shell.leave_copy_mode();
    let ran = h.run(&shell, "seq 2").await;
    let history = h.history(&shell).await;

    assert!(refused.contains("copy-mode"), "{refused}");
    assert!(row.contains("copy-mode"), "{row}");
    assert!(ran.ends_with("Exit code: 0"), "{ran}");
    assert!(!history.contains("seq 1"), "{history}");
}

/// A command goes out only at the *Pane version* its checks read: keys the
/// person types between the checks and the write stop it.
#[tokio::test]
async fn a_pane_that_changes_before_the_send_runs_nothing() {
    let h = Harness::delaying_sends(Duration::from_millis(1000)).await;
    let shell = h.shell().await;

    let (reply, ()) = tokio::join!(h.run(&shell, "seq 1"), async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        shell.type_text("x");
    });
    let history = h.history(&shell).await;

    assert!(reply.contains("changed before"), "{reply}");
    assert!(!history.contains("seq 1"), "{history}");
}

/// The person's yes comes a while after the ask. A decision for a pane in
/// another project in between changes nothing about this pane, so the
/// command still runs.
#[tokio::test]
async fn a_decision_in_another_project_between_the_ask_and_the_yes_stops_nothing() {
    let h = Harness::delaying_sends(Duration::from_millis(200)).await;
    let shell = h.shell().await;
    let other = h.shell().await;
    std::fs::create_dir_all(h.dir().join("other/.claude")).expect("another project");
    std::fs::write(h.dir().join("other/.claude/tmux-mcp.toml"), "default = \"ask\"\n")
        .expect("its policy");
    h.run(&other, "cd other").await;
    std::fs::write(h.dir().join("hello"), "#!/bin/sh\necho hi\n").expect("a script");
    std::fs::set_permissions(h.dir().join("hello"), Permissions::from_mode(0o755))
        .expect("an executable script");
    assert_eq!(h.decision(&shell, "./hello"), Decision::Ask);

    h.decision(&other, "true");
    let reply = h.run_approved(&shell, "./hello").await;

    assert!(reply.ends_with("Exit code: 0"), "{reply}");
}

#[tokio::test]
async fn a_pane_running_a_command_refuses_another() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "sleep 30").await;

    let reply = h.run(&shell, "seq 1").await;

    assert!(
        reply.contains("busy") && reply.contains("sleep 30"),
        "{reply:?}"
    );
}
