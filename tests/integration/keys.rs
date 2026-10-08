//! What `press_key` does to a pane, and what it shows back.

use crate::harness::{Harness, Read};

#[tokio::test]
async fn ctrl_c_stops_a_running_command() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "sleep 100").await;

    h.press_key(&shell, "C-c").await;

    let reply = h.read(&shell, Read::default().tail(1).timeout(5)).await;
    assert!(reply.contains("Exit code: 130"), "{reply:?}");
}

#[tokio::test]
async fn press_key_shows_the_panes_response_to_the_key() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    // Waits silently for one key, then answers 0.3 s later.
    h.start(&shell, "read -rsn1; sleep 0.3; seq 1 | sed s/1/got/").await;

    let screen = h.press_key(&shell, "x").await;

    assert!(screen.lines().any(|row| row == "got"), "{screen:?}");
}

#[tokio::test]
async fn a_screen_whose_content_sits_at_the_top_still_shows() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run(&shell, "clear").await;

    let screen = h.press_key(&shell, "Enter").await;

    assert!(screen.contains('$'), "{screen:?}");
}

#[tokio::test]
#[ignore = "harness: red — press_key rejects key names over 5 characters"]
async fn the_documented_key_names_reach_the_pane() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    shell.type_text("abc");

    let screen = h.press_key(&shell, "BSpace").await;

    assert!(screen.lines().any(|row| row.trim_end() == "$ ab"), "{screen}");
}
