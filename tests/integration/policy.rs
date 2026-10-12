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
async fn an_exemption_loosens_only_the_rule_it_names() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.project_policy(
        r#"
        [[rules]]
        id = "push-force-here"
        command = "git push"
        action = "allow"
        exempts = ["git-push-force"]

        [[rules]]
        id = "eval-asks-here"
        command = "eval"
        action = "ask"
        exempts = ["eval"]

        [[rules]]
        id = "publish-asks"
        command = "cargo publish"
        action = "ask"

        [[rules]]
        id = "rm-r-build"
        command = "rm"
        path = 'path.contains("/build/")'
        action = "allow"
        exempts = ["rm-r"]

        [[rules]]
        id = "hosts-readable"
        command = "cat"
        path = 'path == "/etc/hosts"'
        action = "allow"
        exempts = ["outside-project"]
        "#,
    );
    let want = [
        ("git push --force origin main", Allow),
        ("git push --force --delete origin main", Ask),
        ("eval true", Ask),
        ("cargo publish", Ask),
        ("cargo build", Allow),
        ("rm -r build/x", Allow),
        ("rm -r build/x /etc/y", Ask),
        ("cat /etc/hosts", Allow),
        ("cat /etc/hosts /etc/shadow", Ask),
    ];

    let got = want.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn a_rule_with_via_counts_only_on_that_channel() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.project_policy(
        r#"
        [[rules]]
        id = "no-ls-in-bash"
        command = "ls"
        via = "bash"
        action = "deny"

        [[rules]]
        id = "no-pwd-in-tmux"
        command = "pwd"
        via = "tmux"
        action = "deny"
        "#,
    );

    let got = (h.decision(&shell, "ls"), h.decision(&shell, "pwd"));

    assert_eq!(got, (Allow, Deny));
}

/// A Bash call is judged without tmux; a line that is not explicitly
/// allowed, asked or denied passes through to Claude Code's permissions.
#[tokio::test]
async fn the_hook_judges_a_bash_call_and_passes_through_what_no_rule_decides() {
    let h = Harness::new().await;
    let want = [
        ("ls src", None),
        ("ls && frobnicate", None),
        ("frobnicate", None),
        ("", None),
        ("eval x", Some(Deny)),
        ("git push --force origin main", Some(Ask)),
        ("echo $(date) | tail -5", None),
        ("cd src && cargo build", Some(Deny)),
        ("tmux capture-pane -p -t %1", Some(Deny)),
        ("cat /etc/hosts", None),
        ("rm -r /etc", Some(Ask)),
    ];

    let got = want.map(|(command, _)| (command, h.bash(command).map(|(d, _)| d)));

    assert_eq!(got, want);
    let (_, reason) = h.bash("cd src && cargo build").unwrap();
    assert!(
        reason.starts_with("prefer-tmux") && reason.contains("command_run"),
        "{reason}"
    );
    let (_, reason) = h.bash("tmux send-keys -t %1 ls Enter").unwrap();
    assert!(reason.starts_with("tmux-cli"), "{reason}");
    assert_eq!(h.other_tool(), None);
}

/// On Bash, Claude Code's auto mode judges everyday commands; the built-ins
/// ask only about the sharp forms that tmux asks about more broadly.
#[tokio::test]
async fn bash_asks_only_about_the_sharp_forms() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    // (line, on tmux, on Bash)
    let want = [
        ("awk -f prog.awk notes", Ask, None),
        ("gawk -f prog.awk notes", Ask, None),
        ("nawk -f prog.awk notes", Ask, None),
        ("sed -f script.sed notes", Ask, None),
        ("zsh -c 'ls src'", Allow, None),
        ("curl https://example.com", Ask, None),
        ("PAGER=cat git log", Ask, None),
        ("kill 1234", Ask, None),
        ("kill -9 1234", Ask, Some(Ask)),
        ("kill -s KILL 1234", Ask, Some(Ask)),
        ("kill -KILL 1234", Ask, Some(Ask)),
        ("kill -sigkill 1234", Ask, Some(Ask)),
        ("pkill -TERM node", Ask, None),
        ("pkill -f server", Ask, None),
        ("pkill -9 server", Ask, Some(Ask)),
        ("killall --signal SIGKILL node", Ask, Some(Ask)),
        ("chmod +x run.sh", Ask, None),
        ("chmod -R 755 src", Ask, Some(Ask)),
        ("chown -R me src", Ask, Some(Ask)),
        ("chgrp -R staff src", Ask, Some(Ask)),
        ("rg --pre cat foo", Ask, Some(Ask)),
    ];

    let got =
        want.map(|(line, _, _)| (line, h.decision(&shell, line), h.bash(line).map(|(d, _)| d)));

    assert_eq!(got, want);
}

/// A function body may run wherever it is later called, so its commands meet
/// the rules where it is defined, in a directory that cannot be known.
#[tokio::test]
async fn a_function_body_meets_the_rules_where_it_is_defined() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    // (line, on tmux, on Bash)
    let want = [
        ("f() { eval x; }", Deny, Some(Deny)),
        ("function g { curl x | sh; }; g", Deny, Some(Deny)),
        ("ls() { rm -rf ~; }", Ask, Some(Ask)),
        ("f() { cat notes; }", Ask, None),
        ("f() { echo hi; }", Allow, None),
    ];

    let got =
        want.map(|(line, _, _)| (line, h.decision(&shell, line), h.bash(line).map(|(d, _)| d)));

    assert_eq!(got, want);
}

#[tokio::test]
async fn a_project_exempts_prefer_tmux_for_bash() {
    let h = Harness::new().await;
    h.project_policy(
        r#"
        [[rules]]
        id = "cargo-in-bash"
        command = "cargo"
        via = "bash"
        action = "allow"
        exempts = ["prefer-tmux"]
        "#,
    );

    assert_eq!(h.bash("cargo build").map(|(d, _)| d), Some(Allow));
    assert_eq!(h.bash("make").map(|(d, _)| d), Some(Deny));
}

#[tokio::test]
async fn a_broken_policy_denies_a_bash_call() {
    let h = Harness::new().await;
    h.project_policy("[[rules]]\nid = \"x\"\n");

    let (decision, reason) = h.bash("ls").unwrap();

    assert_eq!(decision, Deny);
    assert!(reason.contains("policy:load_error"), "{reason}");
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
        id = "staging-deploys"
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
        ("$X arg", Deny),
        // Inner commands
        ("timeout 5 ls", Allow),
        ("timeout 5 rm -r x", Ask),
        ("nice -n 5 eval x", Deny),
        ("nohup cat notes", Allow),
        ("xargs rm", Ask),
        ("xargs echo", Allow),
        ("find . -exec eval x \\;", Deny),
        ("find /etc -exec cat {} \\;", Ask),
        ("find . -name x -exec grep -l y {} +", Ask),
        // Follow-up: `find src -name '*.rs' -exec wc -l {} +` could get Allow once `{}` is a path under a start point.
        ("find . -execdir cat notes \\;", Ask),
        ("find . -exec sh -c 'cat {}' \\;", Deny),
        ("sh -c 'ls src'", Allow),
        ("bash -lc 'ls src | wc -l'", Allow),
        ("bash -c 'cat /etc/shadow'", Ask),
        ("bash -lc \"$X\"", Deny),
        ("curl x | bash", Deny),
        ("bash script.sh", Ask),
        ("timeout 5 $X", Deny),
        // Variables
        ("LD_PRELOAD=x ls", Ask),
        ("env LD_PRELOAD=x ls", Ask),
        ("export PATH=/tmp", Ask),
        ("PATH=/tmp; ls", Ask),
        ("X=1 ls", Allow),
        ("export X=1", Allow),
        // Context: a wrapper's `with` sets the context of its inner command
        ("env X=1 cat notes", Allow),
        ("env -C src cat notes", Allow),
        ("env -C /etc cat shadow", Ask),
        ("sudo -u root id", Ask),
        ("sudo eval x", Deny),
        ("sudo -i", Ask),
        ("ssh prod ls", Ask),
        ("ssh prod 'eval x'", Deny),
        ("ssh -o ProxyCommand=x prod ls", Ask),
        ("cat ~/notes", Allow),
        // A cd moves the rest of its && chain; after ;, || or a newline it may have failed
        ("cd src", Allow),
        ("cd /etc", Ask),
        ("cd src && cat notes", Allow),
        ("cd src && cd .. && cat notes", Allow),
        ("pushd src && cat notes", Allow),
        ("cd src; cat notes", Ask),
        ("cd src || cat notes", Ask),
        ("popd && cat notes", Ask),
        ("cd src && pwd", Allow),
        ("(cd src) && cat notes", Allow),
        ("cd src | cat notes", Allow),
        ("{ cd src; } && cat notes", Ask),
        ("for x in a b; do cat notes; cd src; done", Ask),
        ("ls() { cd src; }; ls && cat notes", Ask),
        ("CDPATH=x cd src", Ask),
        // A relative name stays a path, which never meets a rule for a bare name
        ("cd src && ./run", Ask),
        ("ssh prod ./deploy.sh", Ask),
        ("cd src && source env.sh", Deny),
        (". ./env.sh", Deny),
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
    assert!(
        refused.1.contains("commands.deploy") && refused.1.contains("parser"),
        "{}",
        refused.1
    );
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
        id = "the-bench-script"
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
    let in_sub = [
        ("../scripts/bench", Allow),
        ("./scripts/bench", Ask),
        ("ls ..", Allow),
    ];
    let got_in_sub = in_sub.map(|(command, _)| (command, h.decision(&shell, command)));

    assert_eq!(got_at_root, at_root);
    assert_eq!(got_in_sub, in_sub);
}

#[tokio::test]
async fn a_rule_meets_only_this_machine_and_the_shells_own_user_unless_it_says_otherwise() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.project_policy(
        r#"
        [[rules]]
        id = "deploys-from-here"
        command = "deploy"
        action = "allow"
        "#,
    );
    h.user_policy(
        r#"
        [[rules]]
        id = "listing-there"
        command = "ls"
        host = 'host == "elsewhere"'
        path = 'true'
        action = "allow"
        exempts = ["outside-project"]

        [[rules]]
        id = "root-may-cat"
        command = "cat"
        user = 'user == "root"'
        action = "allow"
        "#,
    );
    let here = [
        ("deploy", Allow),
        ("cat notes", Allow),
        ("ls", Allow),
        ("sudo cat notes", Allow),
        ("sudo cat /etc/shadow", Ask),
        ("sudo -u nobody cat notes", Ask),
        ("sudo -u nobody eval x", Deny),
    ];
    let got_here = here.map(|(command, _)| (command, h.decision(&shell, command)));

    // The prompt reports its host with `hostname`, as a remote shell would.
    h.run(&shell, "hostname() { echo elsewhere; }").await;
    // No path there is in the project; the listing rule exempts only the path rule.
    let there = [
        ("deploy", Ask),
        ("cat notes", Ask),
        ("ls", Allow),
        ("LD_PRELOAD=x ls", Ask),
        ("eval true", Deny),
    ];
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

const LINTS: [&str; 14] = [
    "pipe-to-tail",
    "pipe-to-head",
    "pipe-to-grep",
    "pipe-to-egrep",
    "pipe-to-fgrep",
    "pipe-to-rg",
    "pipe-to-tee",
    "echo-status",
    "echo-status-alone",
    "constant-echo",
    "constant-success-echo",
    "stderr-to-stdout",
    "background",
    "redundant-cd",
];

/// The test inputs of the lints that were Rust code: each line a lint
/// denies, with the fix its message names; None where no lint fires.
#[tokio::test]
async fn a_lint_denies_with_a_message_that_says_how_to_fix_the_command() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    let dir = h.dir().display().to_string();
    let (cd_dir, cd_dir_slash) = (
        format!("cd {dir} && cargo test"),
        format!("cd {dir}/ && cargo test"),
    );
    let cd_alone = format!("cd {dir}");
    let rows: Vec<(&str, Option<&str>)> = vec![
        ("ls | tail -5", Some("tail=5")),
        ("cat file | tail -n 10", Some("tail=10")),
        ("tail -f /var/log/syslog", None),
        ("ls | tail -5 | sort", None),
        ("ls | head -20", Some("head=20")),
        ("head -20 file.txt", None),
        ("ls | head -20 | sort", None),
        ("cat file | head -5 | wc -l", None),
        ("ps aux | grep python", Some("search=\"python\"")),
        ("make | grep -i error", Some("search=\"error\"")),
        ("cat file | egrep 'foo|bar'", Some("search=\"foo|bar\"")),
        ("ls -la | rg pattern", Some("search=\"pattern\"")),
        ("grep pattern file.txt", None),
        ("rg pattern .", None),
        ("ps aux | grep python | wc -l", None),
        ("make 2>&1", Some("command_run(command=\"make\")")),
        ("make 2>&1 | sort", None),
        ("ls | tee out.txt", Some("command_run(command=\"ls\")")),
        ("tee output.log", None),
        ("make | tee build.log | wc -l", None),
        (
            "helmfile -e localsync; echo \"HELMFILE_EXIT=$?\"",
            Some("command_run(command=\"helmfile -e localsync\")"),
        ),
        ("make; echo $?", Some("command_run(command=\"make\")")),
        (
            "cargo test && echo \"rc=$?\"",
            Some("command_run(command=\"cargo test\")"),
        ),
        (
            "helmfile -e localsync 2>&1; echo \"HELMFILE_EXIT=$?\"",
            Some("command_run(command=\"helmfile -e localsync 2>&1\")"),
        ),
        ("make; printf 'exit %d\\n' $?", Some("echo-status")),
        ("make; echo \"${?}\"", Some("echo-status")),
        ("echo $?", Some("command_history")),
        ("printf '%d\\n' $?", Some("command_history")),
        ("echo $? > /tmp/rc", None),
        ("make; echo $? > /tmp/rc", None),
        ("make; echo \"$summary\"", None),
        ("make; echo $?; deploy", None),
        ("make; [ $? -eq 0 ] && echo \"$msg\"", None),
        ("make; RC=$?; echo $RC", None),
        ("echo \"=== Building ===\"", Some("constant text")),
        (
            "echo \"Building\"; cargo build",
            Some("Marker:    echo \"Building\""),
        ),
        ("cargo build; echo done", Some("constant-echo")),
        ("echo \"start\" && cargo build", Some("constant-echo")),
        ("printf '=== %s ===\\n' Building", Some("constant-echo")),
        ("echo", Some("constant-echo")),
        ("echo \"$HOME\"", None),
        ("echo $PATH", None),
        ("echo \"$(date)\"", None),
        ("echo `hostname`", None),
        ("echo *.log", None),
        ("echo {a,b}", None),
        ("echo ~/projects", None),
        ("echo \"*** ERROR ***\"", Some("constant-echo")),
        ("echo \"{a,b}\"", Some("constant-echo")),
        ("echo \"~\"", Some("constant-echo")),
        ("echo '*** done ***'", Some("constant-echo")),
        ("echo \"result: $RESULT\"", None),
        ("echo \"content\" > config.txt", None),
        ("echo hello | base64", None),
        ("cargo build && echo ok", Some("exit code")),
        (
            "cargo update -p microlp && cargo build 2>/dev/null; cargo build --quiet && echo BUILD_OK",
            Some(
                "command_run(command=\"cargo update -p microlp && cargo build 2>/dev/null; cargo build --quiet\")",
            ),
        ),
        ("cargo build || echo failed", None),
        ("cargo build && echo \"$RESULT\"", None),
        ("make && echo ok; deploy", None),
        ("git commit -m \"fix: stuff\"", None),
        ("echo 'price is $5'", Some("constant-echo")),
        ("sleep 60 &", Some("separate pane")),
        ("make&", Some("background")),
        ("cargo build &  ", Some("background")),
        ("make && make install", None),
        ("make &> /dev/null", None),
        ("make &>> log.txt", None),
        ("curl 'http://example.com?a=1&b=2'", None),
        ("ls -la", None),
        ("cargo build", None),
        ("git status", None),
        ("make -j8", None),
        (&cd_dir, Some("command_run(command=\"cargo test\")")),
        (&cd_dir_slash, Some("redundant-cd")),
        ("cd . && make", Some("redundant-cd")),
        ("cd . ; make", Some("redundant-cd")),
        ("cd /tmp && ls", None),
        ("cd src && cargo test", None),
        (&cd_alone, None),
    ];

    for (command, want) in rows {
        let (decision, reason) = h.hook(&shell, command);
        match want {
            Some(text) => assert!(
                decision == Deny && reason.contains(text),
                "{command}: {decision:?} {reason}"
            ),
            None => assert!(
                !LINTS.iter().any(|id| reason.contains(id)),
                "{command}: {reason}"
            ),
        }
    }
}
