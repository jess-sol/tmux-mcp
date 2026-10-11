//! When `command_run` refuses to touch a pane.

use crate::harness::{Harness, SETTLE};

#[tokio::test]
async fn a_pane_the_user_is_typing_in_refuses_commands() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    shell.type_text("git sta");

    let reply = h.run(&shell, "ls").await;

    assert!(reply.starts_with("User is typing in pane"), "{reply}");
}

#[tokio::test]
async fn the_typing_refusal_suggests_ctrl_c_only_when_nothing_sits_before_the_cursor() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    // A background job prints `xyz` at the idle prompt and moves the cursor
    // back before it: a garbled line, with nothing typed. Octal keeps `xyz`
    // off the command line itself; the sleep keeps `set -b` from redrawing.
    shell.say("(sleep 0.5; printf '\\170\\171\\172\\033[3D'; sleep 30) &");
    shell.wait_until(SETTLE, |s| s.contains("xyz")).unwrap();

    let garbled = h.run(&shell, "seq 1").await;
    h.press_key(&shell, "C-c").await;
    shell.type_text("git sta");
    let typing = h.run(&shell, "seq 1").await;

    assert!(
        garbled.starts_with("User is typing") && garbled.contains("press_key C-c"),
        "{garbled}"
    );
    assert!(
        typing.starts_with("User is typing") && !typing.contains("press_key C-c"),
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

    let replies = [&first, &second];
    let ran = replies
        .iter()
        .filter(|r| r.contains("Exit code: 0"))
        .count();
    let refused = replies.iter().filter(|r| r.contains("busy")).count();
    assert_eq!((ran, refused), (1, 1), "{first:?}\n{second:?}");
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
