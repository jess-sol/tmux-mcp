//! What `command_run` hands back from a command.

use std::time::{Duration, Instant};

use crate::harness::{Harness, Read};

/// Every command's reply, exact: the command, its output, its exit code.
#[tokio::test]
async fn finished_commands_reply_with_their_output_and_exit_code() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let numbers: Vec<String> = (1..=500).map(|n| n.to_string()).collect();
    let long = format!("$ seq 500\n{}\n\nExit code: 0", numbers.join("\n"));
    let corpus = [
        ("seq 5", "$ seq 5\n1\n2\n3\n4\n5\n\nExit code: 0"),
        ("false", "$ false\n\nExit code: 1"),
        ("true", "$ true\n\nExit code: 0"),
        ("seq 500", &long),
        (
            "(echo aaa; echo bbb; echo ccc)",
            "$ (echo aaa; echo bbb; echo ccc)\naaa\nbbb\nccc\n\nExit code: 0",
        ),
        (
            "(echo outer; (echo inner))",
            "$ (echo outer; (echo inner))\nouter\ninner\n\nExit code: 0",
        ),
        (
            "{ echo aaa; echo bbb; }",
            "$ { echo aaa; echo bbb; }\naaa\nbbb\n\nExit code: 0",
        ),
        (
            "for i in x y z; do echo $i; done",
            "$ for i in x y z; do echo $i; done\nx\ny\nz\n\nExit code: 0",
        ),
        (
            "echo hello | tr a-z A-Z | sed 's/H/h/'",
            "$ echo hello | tr a-z A-Z | sed 's/H/h/'\nhELLO\n\nExit code: 0",
        ),
        (
            "echo $(echo inner)",
            "$ echo $(echo inner)\ninner\n\nExit code: 0",
        ),
        (
            "cat <<'EOF'\nhello heredoc\nworld\nEOF",
            "$ cat <<'EOF'\nhello heredoc\nworld\nEOF\nhello heredoc\nworld\n\nExit code: 0",
        ),
        (
            "cat <<'EOF'\nit's \"quoted\"\nEOF",
            "$ cat <<'EOF'\nit's \"quoted\"\nEOF\nit's \"quoted\"\n\nExit code: 0",
        ),
        // stderr reaches the terminal like stdout does.
        ("echo err >&2", "$ echo err >&2\nerr\n\nExit code: 0"),
        // Output is the text a person sees: colours and terminal controls go.
        (
            "printf '\\033[31m%s\\033[0m\\n' \"${HOME##*/}\"",
            "$ printf '\\033[31m%s\\033[0m\\n' \"${HOME##*/}\"\nhome\n\nExit code: 0",
        ),
        (
            "printf '\\033]0;title\\007%s\\033[K\\n' \"${HOME##*/}\"",
            "$ printf '\\033]0;title\\007%s\\033[K\\n' \"${HOME##*/}\"\nhome\n\nExit code: 0",
        ),
        // A lone carriage return redraws the line: only its last state shows.
        (
            "for p in 50 100; do printf '\\r%s%%' \"$p\"; done; printf '\\n%s\\n' \"$p\"",
            "$ for p in 50 100; do printf '\\r%s%%' \"$p\"; done; printf '\\n%s\\n' \"$p\"\n100%\n100\n\nExit code: 0",
        ),
        // A signal's exit code is 128 plus the signal number.
        (
            "bash -c 'kill -TERM $$'",
            "$ bash -c 'kill -TERM $$'\nTerminated                 bash -c 'kill -TERM $$'\n\nExit code: 143",
        ),
    ];

    for (command, want) in corpus {
        assert_eq!(h.run(&shell, command).await, want, "{command:?}");
    }
}

#[tokio::test]
async fn back_to_back_commands_each_get_their_own_output() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    for n in 1..=5 {
        let reply = h.run(&shell, &format!("seq {n} {n}")).await;

        assert!(
            reply.ends_with(&format!("{n}\n\nExit code: 0")),
            "{reply:?}"
        );
    }
}

#[tokio::test]
async fn a_bare_command_run_waits_for_the_command_to_finish() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let reply = h.run(&shell, "sleep 2; seq 3").await;

    assert!(reply.ends_with("3\n\nExit code: 0"), "{reply:?}");
}

// The commands below sleep for 30 s, longer than the 10 s these tests allow,
// so a reply in time means `command_run` returned early.

#[tokio::test]
async fn next_with_search_returns_at_the_first_match_and_leaves_the_command_running() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let started = Instant::now();

    let reply = h
        .run_with(
            &shell,
            "seq 3; sleep 2; seq 5 | sed s/5/MARK/; sleep 30",
            Read::default().next(1).search("MARK"),
        )
        .await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "returned after {:?}",
        started.elapsed()
    );
    assert!(
        reply.contains("\nMARK\n") && reply.contains("Command still running"),
        "{reply:?}"
    );
}

#[tokio::test]
async fn head_with_search_returns_once_enough_matches_arrive() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let started = Instant::now();

    let command = "seq 2 | sed s/2/HIT_1/; sleep 2; seq 2 | sed s/2/HIT_2/; sleep 30";
    let reply = h
        .run_with(&shell, command, Read::default().head(2).search("HIT"))
        .await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "returned after {:?}",
        started.elapsed()
    );
    assert!(
        reply.contains("HIT_1\nHIT_2\n") && reply.contains("Command still running"),
        "{reply:?}"
    );
}

#[tokio::test]
async fn a_search_window_waits_for_its_matchs_after_context() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let window = || Read::default().head(1).search("HIT").after(1);

    let finished = h
        .run_with(
            &shell,
            "seq 1 | sed s/1/HIT/; sleep 1; seq 1 | sed s/1/ctx/",
            window(),
        )
        .await;
    let started = Instant::now();
    let command = "seq 1 | sed s/1/HIT/; sleep 2; seq 1 | sed s/1/ctx/; sleep 30";
    let running = h.run_with(&shell, command, window()).await;

    assert!(
        finished.contains("HIT\nctx\n") && finished.contains("Exit code: 0"),
        "{finished:?}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "returned after {:?}",
        started.elapsed()
    );
    assert!(
        running.contains("HIT\nctx\n") && running.contains("Command still running"),
        "{running:?}"
    );
}

#[tokio::test]
async fn next_returns_once_enough_lines_arrive() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let started = Instant::now();

    let reply = h
        .run_with(&shell, "sleep 1; seq 3; sleep 30", Read::default().next(3))
        .await;

    assert!(
        started.elapsed() < Duration::from_secs(10),
        "returned after {:?}",
        started.elapsed()
    );
    assert!(
        reply.contains("2\n3\n") && reply.contains("Command still running"),
        "{reply:?}"
    );
}

#[tokio::test]
async fn multibyte_output_arrives_whole() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    // `\303\251` is "é", so the command line itself holds none.
    let reply = h.run(&shell, r"printf '\303\251%.0s' $(seq 1 30000)").await;

    let excerpt: String = reply.chars().take(100).collect();
    assert_eq!(reply.matches('é').count(), 30000, "{excerpt}");
}
