//! How read parameters window a command's output, on `command_run` and on
//! `command_read` after it.

use std::time::{Duration, Instant};

use crate::harness::{Harness, Read};

#[tokio::test]
async fn read_parameters_shape_a_finished_commands_output() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let corpus = [
        // The first command in a Shell is id 2; the integration probe is id 1.
        (
            "seq 10",
            Read::default().tail(3),
            "$ seq 10\n<7 lines not shown>\n8\n9\n10\n\nExit code: 0\n\
             [Output filtered (10 lines total). Use command_read(command_id=2) with head, tail, \
             or search for different views of the same command output.]",
        ),
        // Search filters first, so the window counts matches: the last ten
        // matches, not the matches among the last ten lines.
        (
            "seq 20",
            Read::default().tail(10).search("1"),
            "$ seq 20\n<1 lines not shown>\n10\n11\n12\n13\n14\n15\n16\n17\n18\n19\n\nExit code: 0\n\
             [11 matches in 20 lines.]\n[Output filtered (20 lines total). Use command_read(command_id=3) \
             with head, tail, or search for different views of the same command output.]",
        ),
        (
            "seq 6 | sed 's/4/MATCH/'",
            Read::default().search("MATCH").before(2),
            "$ seq 6 | sed 's/4/MATCH/'\n2\n3\nMATCH\n\nExit code: 0\n[1 matches in 6 lines.]",
        ),
        (
            "seq 6 | sed 's/3/MATCH/'",
            Read::default().search("MATCH").after(2),
            "$ seq 6 | sed 's/3/MATCH/'\nMATCH\n4\n5\n\nExit code: 0\n[1 matches in 6 lines.]",
        ),
        (
            "seq 2 13",
            Read::default().search("1"),
            "$ seq 2 13\n10\n11\n12\n13\n\nExit code: 0\n[4 matches in 12 lines.]",
        ),
        // Distant matches show as separate groups.
        (
            "seq 7 | sed 's/^[27]$/HIT&/'",
            Read::default().search("HIT").after(1),
            "$ seq 7 | sed 's/^[27]$/HIT&/'\nHIT2\n3\n--\nHIT7\n\nExit code: 0\n[2 matches in 7 lines.]",
        ),
        // Overlapping context merges into one group.
        (
            "seq 5 | sed 's/^[24]$/HIT&/'",
            Read::default().search("HIT").after(1),
            "$ seq 5 | sed 's/^[24]$/HIT&/'\nHIT2\n3\nHIT4\n5\n\nExit code: 0\n[2 matches in 5 lines.]",
        ),
        // A model can send any combination; the tool refuses the ones that
        // mean nothing.
        (
            "seq 5",
            Read::default().next(2).head(2),
            "next, head, and tail are mutually exclusive — use only one",
        ),
        (
            "seq 5",
            Read::default().before(2),
            "before and after require search",
        ),
    ];

    for (command, read, want) in corpus {
        assert_eq!(h.run_with(&shell, command, read).await, want, "{command:?}");
    }
}

#[tokio::test]
async fn a_window_on_a_finished_command_reports_its_exit_code() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let windows = [
        ("seq 10", Read::default().head(3)),
        ("seq 3", Read::default().next(3)),
        (
            "seq 6 | sed 's/^[25]$/HIT&/'",
            Read::default().search("HIT").head(1).after(1),
        ),
    ];

    for (command, read) in windows {
        let reply = h.run_with(&shell, command, read).await;

        assert!(
            reply.contains("Exit code: 0") && !reply.contains("still running"),
            "{command:?}: {reply:?}"
        );
    }
}

/// Each MCP server keeps its own place in a command's output, and a place in
/// one command never carries over to a newer one.
#[tokio::test]
async fn each_mcp_server_reads_from_its_own_place() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run_with(&shell, "seq 4", Read::default().head(1)).await;
    let other = h.another_session().await;

    let mine = h.read(&shell, Read::default().next(2)).await;
    let theirs = other.read(&shell, Read::default().next(2)).await;
    h.run_with(&shell, "seq 3", Read::default().head(1)).await;
    let newer = h.read(&shell, Read::default().next(2)).await;

    assert!(mine.starts_with("1\n2\n"), "{mine:?}");
    assert!(theirs.starts_with("1\n2\n"), "{theirs:?}");
    assert!(
        newer.starts_with("1\n2\n") && !newer.contains("already read"),
        "{newer:?}"
    );
}

#[tokio::test]
async fn only_next_moves_the_cursor() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run_with(&shell, "seq 10", Read::default().head(2)).await;
    h.read(&shell, Read::default().tail(2)).await;

    let first = h.read(&shell, Read::default().next(3)).await;
    let second = h.read(&shell, Read::default().next(3)).await;
    let rest = h.read(&shell, Read::default().next(100)).await;
    let past_the_end = h.read(&shell, Read::default().next(10)).await;

    assert!(
        first.contains("1\n2\n3\n") && !first.contains("already read"),
        "{first:?}"
    );
    assert!(
        second.starts_with("<3 lines already read>\n4\n5\n6\n"),
        "{second:?}"
    );
    assert!(
        rest.starts_with("<6 lines already read>\n7\n8\n9\n10\n"),
        "{rest:?}"
    );
    assert!(
        past_the_end.contains("No unread output"),
        "{past_the_end:?}"
    );
}

#[tokio::test]
async fn next_with_search_reads_on_past_the_context_it_showed() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let read = || Read::default().next(1).search("HIT").after(1);

    let first = h
        .run_with(&shell, "seq 5 | sed 's/^[14]$/HIT&/'", read())
        .await;
    let second = h.read(&shell, read()).await;

    assert!(first.contains("HIT1\n2\n"), "{first:?}");
    assert!(
        second.contains("HIT4\n5\n") && !second.contains("HIT1"),
        "{second:?}"
    );
}

// --- Reading a command that is still running ---

#[tokio::test]
async fn next_waits_for_lines_a_running_command_has_not_printed_yet() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    // `tail` leaves the cursor where it was, so the read below starts at line 1.
    h.run_with(&shell, "sleep 2; seq 5", Read::default().tail(3).timeout(1))
        .await;

    let reply = h.read(&shell, Read::default().next(3).timeout(10)).await;

    assert!(
        reply.contains("2\n3\n") && !reply.contains("\n4\n"),
        "{reply:?}"
    );
}

#[tokio::test]
async fn a_line_still_being_written_is_read_once_it_ends() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "seq 1 | tr -d '\\n'; sleep 2; seq 2 2")
        .await;

    let reply = h.read(&shell, Read::default().next(5).timeout(10)).await;

    assert!(reply.contains("12\n"), "{reply:?}");
}

#[tokio::test]
async fn next_with_search_scans_forward_to_a_later_match() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "seq 3; sleep 2; seq 5 | sed s/5/MARK/")
        .await;

    let reply = h
        .read(&shell, Read::default().next(1).search("MARK").timeout(10))
        .await;

    assert!(reply.contains("MARK"), "{reply:?}");
}

#[tokio::test]
async fn head_with_search_waits_for_enough_matches() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(
        &shell,
        "seq 2 | sed s/2/HIT_1/; sleep 2; seq 2 | sed s/2/HIT_2/",
    )
    .await;

    let reply = h
        .read(&shell, Read::default().head(2).search("HIT").timeout(10))
        .await;

    assert!(reply.contains("HIT_1\nHIT_2\n"), "{reply:?}");
}

#[tokio::test]
async fn tail_of_a_running_command_waits_for_it_and_shows_nothing_on_timeout() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "sleep 10").await;
    let started = Instant::now();

    let reply = h.read(&shell, Read::default().tail(3).timeout(2)).await;

    assert!(
        started.elapsed() >= Duration::from_millis(1500),
        "returned after {:?}",
        started.elapsed()
    );
    assert!(
        reply.contains("No new output. Command still running"),
        "{reply:?}"
    );
}

#[tokio::test]
async fn a_bare_read_of_a_running_command_returns_at_once() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "seq 1; sleep 10").await;
    let started = Instant::now();

    let reply = h.read(&shell, Read::default().timeout(10)).await;

    assert!(
        started.elapsed() < Duration::from_millis(500),
        "returned after {:?}",
        started.elapsed()
    );
    assert!(reply.contains("still running"), "{reply:?}");
}

// --- History ---

#[tokio::test]
async fn history_lists_the_newest_command_first() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.run(&shell, "seq 1").await;
    h.run(&shell, "seq 2").await;

    let history = h.history(&shell).await;

    let newest = history.lines().nth(1).unwrap_or("");
    assert!(newest.ends_with("seq 2"), "{history}");
}
