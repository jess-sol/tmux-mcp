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

/// A shell that sends no markers takes no command until its integration is
/// injected; the failed probe shows in `list_panes` until then.
#[tokio::test]
async fn a_shell_without_integration_takes_commands_once_injected() {
    let h = Harness::new().await;
    let shell = h.shell_without_integration().await;

    let refused = h.run(&shell, "seq 1").await;
    let row = h.list_panes().await.row(&shell);
    let injected = h.inject_osc133(&shell).await;
    let ran = h.run(&shell, "seq 1").await;

    assert!(refused.contains("inject_osc133"), "{refused}");
    assert!(row.contains("failed"), "{row}");
    assert!(injected.contains("active"), "{injected:?}");
    assert!(ran.ends_with("1\n\nExit code: 0"), "{ran:?}");
}

#[tokio::test]
async fn inject_osc133_leaves_an_integrated_shell_alone() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run(&shell, "true").await;

    let reply = h.inject_osc133(&shell).await;

    assert!(reply.contains("already active"), "{reply:?}");
}

/// Locate a fish binary for the fish-specific regression test, if installed.
fn find_fish() -> Option<String> {
    const CANDIDATES: &[&str] = &[
        "/opt/homebrew/bin/fish",
        "/usr/local/bin/fish",
        "/run/current-system/sw/bin/fish",
        "/usr/bin/fish",
        "fish",
    ];
    CANDIDATES
        .iter()
        .find(|candidate| {
            std::process::Command::new(candidate)
                .arg("--version")
                .output()
                .is_ok_and(|o| o.status.success())
        })
        .map(|candidate| candidate.to_string())
}

/// A fish pane must complete commands even though fish 4.x emits its own OSC
/// 133 markers alongside the injected ones. Regression test: the injected
/// fish_prompt hook emits a second D marker in the Prompt phase, which used
/// to synthesize a spurious record that swallowed the B latch, so the real
/// command never completed and command_run hung until its timeout.
#[tokio::test]
async fn a_fish_shell_completes_commands_with_native_and_injected_markers() {
    let Some(fish) = find_fish() else {
        eprintln!("skipping: fish not installed");
        return;
    };
    let h = Harness::new().await;
    let shell = h.fish(&fish).await;

    let reply = h.inject_osc133(&shell).await;
    assert!(
        reply.contains("active") && !reply.contains("unsupported"),
        "{reply:?}"
    );

    let out = h.run(&shell, "seq 2").await;

    assert!(out.contains("1\n2"), "{out:?}");
    assert!(out.contains("Exit code: 0"), "{out:?}");
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
async fn a_pane_that_shrinks_and_grows_back_shows_what_tmux_shows() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run(&shell, "seq 40").await;

    shell.resize(10);
    shell.resize(25);

    let (daemon, tmux) = h.screens(&shell).await;
    assert_eq!(daemon, tmux);
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
