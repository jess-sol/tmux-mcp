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
        when = 'command.name == "git" && command.args[0] == "push"'
        action = "allow"
        override = true

        [[rules]]
        description = "eval is fine here, with a human"
        when = 'command.name == "eval"'
        action = "ask"
        override = true

        [[rules]]
        description = "publishing needs a human"
        when = 'command.name == "cargo" && command.args[0] == "publish"'
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
#[ignore = "harness: red — path rules skip every argument that starts with -"]
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
#[ignore = "harness: red — no rule reads command.read_targets"]
async fn an_input_redirect_is_a_path_the_command_reads() {
    let h = Harness::new().await;
    let shell = h.shell().await;

    let decision = h.decision(&shell, "cat < /etc/shadow");

    assert_eq!(decision, Ask);
}

#[tokio::test]
#[ignore = "harness: red — the git rules allow config, branch, tag and remote in every mode"]
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
#[ignore = "harness: red — only awk's system is asked about; pipes and -f scripts pass"]
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
#[ignore = "harness: red — rules see words with their quotes still on"]
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
#[ignore = "harness: red — command substitutions in assignments, ${…}, redirect targets, for words and heredocs go unread"]
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
#[ignore = "harness: red — in_project compares path strings, not path components"]
async fn a_sibling_directory_is_outside_the_project() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let beside = format!("{}-evil/notes", h.dir().display());

    let decision = h.decision(&shell, &format!("rm {beside}"));

    assert_eq!(decision, Ask);
}
