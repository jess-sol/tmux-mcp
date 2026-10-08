//! When `command_run` refuses to touch a pane.

use crate::harness::Harness;

#[tokio::test]
async fn a_pane_the_user_is_typing_in_refuses_commands() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    shell.type_text("git sta");

    let reply = h.run(&shell, "ls").await;

    assert!(reply.starts_with("User is typing in pane"), "{reply}");
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
async fn a_pane_running_a_command_refuses_another() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "sleep 30").await;

    let reply = h.run(&shell, "seq 1").await;

    assert!(reply.contains("busy") && reply.contains("sleep 30"), "{reply:?}");
}
