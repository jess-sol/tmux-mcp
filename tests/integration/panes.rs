//! What `list_panes` and `debug_pane` say about the panes beside the caller.

use std::time::{Duration, Instant};

use crate::harness::{Harness, SETTLE};

#[tokio::test]
async fn a_shell_is_confirmed_once_a_command_runs_in_it() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    h.run(&shell, "true").await;

    let row = h.list_panes().await.row(&shell);
    assert!(row.contains("confirmed"), "{row}");
}

#[tokio::test]
async fn inject_osc133_leaves_an_integrated_shell_alone() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run(&shell, "true").await;

    let reply = h.inject_osc133(&shell).await;

    assert!(reply.contains("already active"), "{reply:?}");
}

#[tokio::test]
async fn debug_pane_shows_the_screen_of_an_idle_pane() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run(&shell, "seq 3 | sed s/3/marker/").await;

    let screen = h.debug_pane(&shell).await;

    assert!(
        screen.lines().any(|row| row == "marker") && !screen.starts_with("[Command"),
        "{screen:?}"
    );
}

#[tokio::test]
async fn debug_pane_points_at_command_read_while_a_command_runs() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "sleep 30").await;

    let screen = h.debug_pane(&shell).await;

    assert!(
        screen.starts_with("[Command") && screen.contains("\"sleep 30\""),
        "{screen:?}"
    );
    assert!(screen.contains("command_read"), "{screen:?}");
}

#[tokio::test]
async fn shell_integration_outlives_a_background_job() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    shell.say("sleep 1 &");
    h.run(&shell, "true").await;
    shell.wait_until(SETTLE, |s| s.contains("Done")).unwrap();

    assert!(h.list_panes().await.row(&shell).contains("confirmed"));
}

#[tokio::test]
async fn list_panes_answers_while_a_command_runs() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let running = h.run(&shell, "sleep 3");
    let looking = async {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let asked = Instant::now();
        h.list_panes().await;
        asked.elapsed()
    };
    let (_, waited) = tokio::join!(running, looking);

    assert!(
        waited < Duration::from_secs(1),
        "list_panes waited {waited:?} behind command_run"
    );
}
