//! What the policy hook tells Claude Code before a command runs. Each test is
//! a table of commands and the decision each one gets.

use crate::harness::Decision::{Allow, Ask, Deny};
use crate::harness::Harness;

#[tokio::test]
async fn grep_checks_its_files_not_its_pattern() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("grep '/api/' src", Allow),
        ("grep -e '/api/' src", Allow),
        ("grep -rn foo src", Allow),
        ("grep -f notes -f /etc/shadow src", Ask),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn an_unknown_rule_does_not_yield_to_a_later_allow() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let decision = h.decision(&shell, "git push $F origin status");

    assert_eq!(decision, Ask);
}

#[tokio::test]
async fn a_device_redirect_hides_no_rule() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("eval true > /dev/null", Deny),
        ("rm -rf / > /dev/null", Ask),
        ("echo hi > /dev/null", Allow),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn an_override_decides_alone() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.project_policy(
        r#"
        [[rules]]
        description = "push freely here"
        command = "git push"
        action = "allow"
        override = true

        [[rules]]
        description = "eval is fine here, with a human"
        command = "eval"
        action = "ask"
        override = true

        [[rules]]
        description = "publishing needs a human"
        command = "cargo publish"
        action = "ask"
        "#,
    );
    let want = [
        ("git push --force origin main", Allow),
        ("eval true", Ask),
        ("cargo publish", Ask),
        ("cargo build", Allow),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn a_rule_reads_the_parts_its_grammar_names() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.project_policy(
        r#"
        [commands.deploy]
        parser = "gnu"
        arguments = ["targets...", "--force|-f", "--output|-o:"]
        marks.write = ["output"]

        [[rules]]
        description = "staging deploys"
        command = "deploy"
        when = 'args.targets == ["staging"] && !args.force'
        action = "allow"
        "#,
    );
    let want = [
        ("deploy staging", Allow),
        ("deploy --forc staging", Ask),
        ("deploy -o /tmp/out staging", Ask),
        ("deploy -o out staging", Allow),
        ("deploy prod", Ask),
        ("deploy staging $X", Ask),
        ("deploy --region staging", Ask),
        ("deploy staging --dry-run", Allow),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

/// The built-in policy over common lines, in a pane at the project root.
#[tokio::test]
async fn the_builtin_policy_decides_common_lines() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("ls", Allow),
        ("ls -la src", Allow),
        ("ls --frob /etc src", Ask),
        ("ls /etc", Ask),
        ("ls | wc -l", Allow),
        ("cat src/*.rs", Allow),
        ("cat {notes,/etc/shadow}", Ask),
        ("cat $F", Ask),
        ("cat notes $F", Ask),
        ("/bin/cat notes", Allow),
        ("./bin/cat notes", Ask),
        ("head -5 notes", Allow),
        ("head -5 /etc/passwd", Ask),
        ("tail +5 notes", Allow),
        ("grep -A 3 foo src", Allow),
        ("grep -e x /etc/shadow", Ask),
        ("rg foo", Allow),
        ("rg --pre ./x foo", Ask),
        ("sort -o out notes", Allow),
        ("cp notes backup", Allow),
        ("cp notes /tmp/x", Ask),
        ("rm notes", Allow),
        ("rm -r src", Ask),
        ("rm --recur src", Ask),
        ("echo hi > out", Allow),
        ("echo hi > /tmp/out", Ask),
        ("echo $(date)", Allow),
        ("find . -name '*.rs'", Allow),
        ("find /etc -name x", Ask),
        ("find . -delete", Ask),
        ("find . -fprint /tmp/x", Ask),
        ("find . -exec rm {} \\;", Ask),
        ("sed 's/a/b/' notes", Allow),
        ("sed -n '/x/p' notes", Allow),
        ("sed 's/a/b/w /tmp/x' notes", Ask),
        ("awk '{print $1}' notes", Allow),
        ("git status", Allow),
        ("git log -5", Allow),
        ("git log --oneline -5", Allow),
        ("git --frob status", Ask),
        ("git --exec-path=/tmp/x status", Deny),
        ("git log @{u}..", Allow),
        ("git diff HEAD~1", Allow),
        ("git -c core.pager=x log", Deny),
        ("git frobnicate", Ask),
        ("git push", Ask),
        ("git push --force origin main", Ask),
        ("git stash list", Allow),
        ("git stash drop", Ask),
        ("git stash $X", Ask),
        ("git branch", Allow),
        ("git branch -d x", Ask),
        ("git tag", Allow),
        ("git tag -Xl v2", Ask),
        ("git remote -v", Allow),
        ("git config user.name", Allow),
        ("git config user.name x", Ask),
        ("git config --global user.name", Allow),
        ("cargo build", Allow),
        ("cargo build --release", Allow),
        ("cargo test --release -p core foo", Allow),
        ("cargo frob --manifest-path=/x", Ask),
        ("cargo +nightly test foo", Allow),
        ("cargo -C / build", Ask),
        ("cargo test --manifest-path /x/Cargo.toml", Ask),
        ("cargo build --target-dir /tmp/t", Ask),
        ("cargo install ripgrep", Ask),
        ("curl x", Ask),
        ("/usr/bin/curl x", Ask),
        ("apt remove x", Ask),
        ("eval true", Deny),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn a_policy_that_does_not_load_denies_every_command() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.project_policy("[commands.deploy]\narguments = [\"targets...\"]\n");

    let refused = (h.decision(&shell, "ls"), h.run(&shell, "ls").await);
    h.project_policy("[commands.deploy]\nparser = \"gnu\"\narguments = [\"targets...\"]\n");
    let mended = h.decision(&shell, "ls");

    assert_eq!(refused.0, Deny);
    assert!(refused.1.contains("commands.deploy") && refused.1.contains("parser"), "{}", refused.1);
    assert_eq!(mended, Allow);
}

#[tokio::test]
async fn a_command_is_named_by_its_path_from_the_project_root() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    std::fs::create_dir(h.dir().join("sub")).expect("a subdirectory");
    h.project_policy(
        r#"
        [[rules]]
        description = "the bench script"
        command = "./scripts/bench"
        action = "allow"
        "#,
    );
    let absolute = format!("{}/scripts/bench", h.dir().display());
    let at_root = [
        ("./scripts/bench", Allow),
        ("scripts/bench", Allow),
        ("bench", Ask),
        (absolute.as_str(), Allow),
    ];
    let got_at_root = at_root.map(|(command, _)| (command, h.decision(&shell, command)));

    h.run(&shell, "cd sub").await;
    let in_sub = [("../scripts/bench", Allow), ("./scripts/bench", Ask), ("ls ..", Allow)];
    let got_in_sub = in_sub.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got_at_root, at_root);
    assert_eq!(got_in_sub, in_sub);
}

#[tokio::test]
async fn a_rule_without_a_host_meets_only_this_machine() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.project_policy(
        r#"
        [[rules]]
        description = "deploys from here"
        command = "deploy"
        action = "allow"
        "#,
    );
    h.user_policy(
        r#"
        [[rules]]
        description = "listing there"
        command = "ls"
        host = 'host == "elsewhere"'
        action = "allow"
        "#,
    );
    let here = [("deploy", Allow), ("cat notes", Allow), ("ls", Allow)];
    let got_here = here.map(|(command, _)| (command, h.decision(&shell, command)));

    // The prompt reports its host with `hostname`, as a remote shell would.
    h.run(&shell, "hostname() { echo elsewhere; }").await;
    let there = [("deploy", Ask), ("cat notes", Ask), ("ls", Allow), ("eval true", Deny)];
    let got_there = there.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got_here, here);
    assert_eq!(got_there, there);
}

#[tokio::test]
async fn a_command_typed_into_a_running_program_meets_no_rule_of_this_machine() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "cat").await;
    let want = [("ls", Ask), ("eval true", Deny)];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn a_path_in_a_flag_value_is_checked() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("sort -o/tmp/evil notes", Ask),
        ("cp -t/tmp/evil notes", Ask),
        ("git log --output=/tmp/evil", Ask),
        ("git -C / log", Ask),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn an_input_redirect_is_a_path_the_command_reads() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let decision = h.decision(&shell, "cat < /etc/shadow");

    assert_eq!(decision, Ask);
}

#[tokio::test]
async fn a_git_command_that_writes_asks() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("git config core.fsmonitor 'sh -c id'", Ask),
        ("git branch -D main", Ask),
        ("git tag -d v1", Ask),
        ("git remote set-url origin evil", Ask),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn an_awk_or_sed_script_that_can_run_a_command_asks() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("awk 'BEGIN{print \"id\" | \"sh\"}'", Ask),
        ("awk -f prog.awk notes", Ask),
        ("sed -f prog.sed notes", Ask),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn rules_see_words_without_their_quotes() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("'eval' true", Deny),
        ("cat '/etc/shadow'", Ask),
        ("cat \"/etc/shadow\"", Ask),
        ("echo hi > '/tmp/evil'", Ask),
        ("rm \"-r\" src", Ask),
        ("cargo \"install\" x", Ask),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
#[ignore = "commands: red — command substitutions in assignments, ${…}, redirect targets, for words and heredocs go unread"]
async fn a_command_substitution_meets_the_rules() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let want = [
        ("FOO=$(eval true) ls", Deny),
        ("echo ${x:-$(eval true)}", Deny),
        ("echo hi > $(eval true)", Deny),
        ("for i in $(eval true); do echo; done", Deny),
        ("cat <<EOF\n$(eval true)\nEOF", Deny),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn a_sibling_directory_is_outside_the_project() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let beside = format!("{}-evil/notes", h.dir().display());

    let decision = h.decision(&shell, &format!("rm {beside}"));

    assert_eq!(decision, Ask);
}
