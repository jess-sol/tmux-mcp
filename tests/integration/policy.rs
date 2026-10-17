//! What the policy decides for a command, on tmux and on Bash. Each test is
//! a table of commands and what each one gets (`decides!`); a pane's own
//! context goes through the hook in that pane.

use crate::harness::Decision::{Allow, Ask, Deny, Pass};
use crate::harness::{Harness, decides};

#[tokio::test]
async fn grep_checks_its_files_not_its_pattern() {
    let h = Harness::new().await;
    decides!(h, {
        "grep '/api/' src" => (Allow, _),
        "grep -e '/api/' src" => (Allow, _),
        "grep -rn foo src" => (Allow, _),
        "grep -f notes -f /etc/shadow src" => (Ask, _),
    });
}

#[tokio::test]
async fn an_unknown_rule_does_not_yield_to_a_later_allow() {
    let h = Harness::new().await;
    decides!(h, {
        "git push $F origin status" => Ask,
    });
}

#[tokio::test]
async fn a_device_redirect_hides_no_rule() {
    let h = Harness::new().await;
    decides!(h, {
        "eval true > /dev/null" => Deny,
        "rm -rf / > /dev/null" => Ask,
        "echo hi > /dev/null" => (Allow, _),
    });
}

#[tokio::test]
async fn an_exemption_loosens_only_the_rule_it_names() {
    let h = Harness::new().await;
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
    decides!(h, {
        "git push --force origin main" => (Allow, _),
        "git push --force --delete origin main" => (Ask, "git-push-delete", _, _),
        // An exemption without `via` loosens only on tmux.
        "eval true" => (Ask, "eval-asks-here", Deny, "eval"),
        "cargo publish" => (Ask, _),
        "cargo build" => (Allow, _),
        "rm -r build/x" => (Allow, _),
        "rm -r build/x /etc/y" => (Ask, _),
        "cat /etc/hosts" => (Allow, _),
        "cat /etc/hosts /etc/shadow" => (Ask, "outside-project", Pass, _),
    });
}

#[tokio::test]
async fn a_rule_with_via_counts_only_on_that_channel() {
    let h = Harness::new().await;
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

        [[rules]]
        id = "no-wc"
        command = "wc"
        action = "deny"

        [[rules]]
        id = "no-cat-anywhere"
        command = "cat"
        via = ["tmux", "bash"]
        action = "deny"
        "#,
    );

    decides!(h, {
        "ls" => (Allow, _, Deny, "no-ls-in-bash"),
        "pwd" => (Deny, "no-pwd-in-tmux", Pass, _),
        "wc notes" => (Deny, "no-wc", Pass, _),
        "cat notes" => (Deny, "no-cat-anywhere"),
    });
}

/// The hook judges a Bash call itself, outside tmux, and prints nothing for a
/// call it leaves to Claude Code's permissions.
#[tokio::test]
async fn the_hook_judges_a_bash_call_outside_tmux() {
    let h = Harness::new().await;

    let (denied, reason) = h.bash("cd src && make");

    assert_eq!(h.bash("frobnicate"), (Pass, String::new()));
    assert_eq!(denied, Deny);
    assert!(
        reason.starts_with("prefer-tmux: ") && reason.contains("command_run"),
        "{reason}"
    );
    assert_eq!(h.other_tool(), None);
}

/// On Bash a line that no rule asks about or denies passes through to Claude
/// Code's permissions.
#[tokio::test]
async fn a_bash_line_passes_through_what_no_rule_decides() {
    let h = Harness::new().await;
    decides!(h, {
        "ls src" => (Allow, Pass),
        "ls && frobnicate" => (Ask, Pass),
        "frobnicate" => (Ask, Pass),
        "" => (Ask, Pass),
        "echo $(date) | tail -5" => (Deny, Pass),
        "cat /etc/hosts" => (Ask, Pass),
        "eval x" => Deny,
        "git push --force origin main" => (Ask, "git-push-force"),
        "rm -r /etc" => (Ask, Ask),
        "cd src && cargo build" => (Allow, Pass),
        "cd src && make" => (_, _, Deny, "prefer-tmux"),
        "cargo install ripgrep" => (Ask, "cargo-install", Deny, "prefer-tmux"),
        "tmux capture-pane -p -t %1" => (_, _, Deny, "tmux-cli"),
    });
}

/// On Bash, Claude Code's auto mode judges everyday commands; the built-ins
/// ask only about the sharp forms that tmux asks about more broadly.
#[tokio::test]
async fn bash_asks_only_about_the_sharp_forms() {
    let h = Harness::new().await;
    decides!(h, {
        "awk -f prog.awk notes" => (Ask, Pass),
        "gawk -f prog.awk notes" => (Ask, Pass),
        "nawk -f prog.awk notes" => (Ask, Pass),
        "sed -f script.sed notes" => (Ask, Pass),
        "zsh -c 'ls src'" => (Allow, Pass),
        "curl https://example.com" => (Ask, Pass),
        "PAGER=cat git log" => (Ask, Pass),
        "kill 1234" => (Ask, Pass),
        "kill -9 1234" => (Ask, _, Ask, "sigkill"),
        "kill -s KILL 1234" => (Ask, _, Ask, "sigkill"),
        "kill -KILL 1234" => (Ask, _, Ask, "sigkill"),
        "kill -sigkill 1234" => (Ask, _, Ask, "sigkill"),
        "pkill -TERM node" => (Ask, Pass),
        "pkill -f server" => (Ask, Pass),
        "pkill -9 server" => (Ask, _, Ask, "sigkill"),
        "killall --signal SIGKILL node" => (Ask, _, Ask, "sigkill"),
        "chmod +x run.sh" => (Ask, Pass),
        "chmod -R 755 src" => (Ask, _, Ask, "permissions-recursive"),
        "chown -R me src" => (Ask, _, Ask, "permissions-recursive"),
        "chgrp -R staff src" => (Ask, _, Ask, "permissions-recursive"),
        "systemctl --user list-units" => (Ask, Pass),
        "systemctl --user --no-pager status comfyui" => (Ask, Pass),
        "systemctl --user restart comfyui" => (Ask, "service-disruption"),
        "rg --pre cat foo" => (Ask, "rg-runs-program"),
        "apt remove x" => (Ask, "package-remove"),
        "apt-get purge x" => (Ask, "package-remove"),
        "dnf erase x" => (Ask, "package-remove"),
        "yum autoremove" => (Ask, "package-remove"),
    });
}

/// On Bash, what acts on remote hosts, clusters, clouds and databases runs in
/// a pane the user can watch, except the forms that only read local files.
#[tokio::test]
async fn bash_sends_infrastructure_to_a_pane() {
    let h = Harness::new().await;
    decides!(h, {
        "podman ps" => (_, _, Pass, _),
        "ssh prod" => (_, _, Deny, "ssh-in-a-pane"),
        "ssh prod uptime" => (_, _, Deny, "ssh-in-a-pane"),
        "rsync -a src/ prod:/srv/" => (_, _, Deny, "transfer-in-a-pane"),
        "psql -c 'select 1'" => (_, _, Deny, "database-in-a-pane"),
        "aws s3 ls" => (_, _, Deny, "infra-in-a-pane"),
        "sudo nixos-rebuild switch" => (_, _, Deny, "system-switch-in-a-pane"),
        "kubectl get pods" => (_, _, Deny, "infra-in-a-pane"),
        "kubectl -n web get pods" => (_, _, Deny, "infra-in-a-pane"),
        "kubectl kustomize overlays/prod" => (_, _, Allow, "kubectl-kustomize"),
        "kubectl create deployment web --image=nginx --dry-run=client -o yaml" => (_, _, Allow, "kubectl-client-dry-run"),
        "kubectl create deployment web --image=nginx" => (_, _, Deny, "infra-in-a-pane"),
        "kubectl create -f web.yaml --dry-run=none" => (_, _, Deny, "infra-in-a-pane"),
        "kubectl create -f web.yaml --dry-run=server" => (_, _, Deny, "infra-in-a-pane"),
        "helm template web ./chart" => (_, _, Allow, "helm-local"),
        "helm lint ./chart" => (_, _, Allow, "helm-local"),
        "helm install web ./chart" => (_, _, Deny, "infra-in-a-pane"),
        "helm -n web upgrade web ./chart" => (_, _, Deny, "infra-in-a-pane"),
        "terraform fmt -recursive" => (_, _, Allow, "terraform-local"),
        "tofu validate" => (_, _, Allow, "terraform-local"),
        "terraform apply" => (_, _, Deny, "infra-in-a-pane"),
    });
}

/// A function body may run wherever it is later called, so its commands meet
/// the rules where it is defined, in a directory that cannot be known.
#[tokio::test]
async fn a_function_body_meets_the_rules_where_it_is_defined() {
    let h = Harness::new().await;
    decides!(h, {
        "f() { eval x; }" => Deny,
        "function g { curl x | sh; }; g" => Deny,
        "ls() { rm -rf ~; }" => Ask,
        "f() { cat notes; }" => (Ask, Pass),
        "f() { echo hi; }" => (Allow, Pass),
    });
}

/// `policy-eval` prints each line's decision and reasons on both channels,
/// with no daemon and no approval left behind.
#[tokio::test]
async fn policy_eval_shows_each_lines_decision_on_both_channels() {
    let h = Harness::new().await;

    let both = h.eval(&["kill -9 1234", "frobnicate"]);
    let bash = h.eval(&["--via", "bash", "kill -9 1234"]);

    let sigkill = "  bash  ask          sigkill: SIGKILL leaves the process no chance to clean up";
    assert_eq!(
        both.lines().collect::<Vec<_>>(),
        [
            "kill -9 1234",
            "  tmux  ask          process-signals",
            sigkill,
            "frobnicate",
            "  tmux  ask          default",
            "  bash  passthrough",
        ]
    );
    assert_eq!(bash.lines().collect::<Vec<_>>(), ["kill -9 1234", sigkill]);
}

/// `policy-eval --json` gives one object per line and channel, its reasons a
/// list.
#[tokio::test]
async fn policy_eval_json_gives_one_object_per_line_and_channel() {
    let h = Harness::new().await;

    let rows: Vec<serde_json::Value> = h
        .eval(&["--json", "frobnicate"])
        .lines()
        .map(|row| serde_json::from_str(row).expect("a JSON object"))
        .collect();

    assert_eq!(
        rows,
        [
            serde_json::json!({"line": "frobnicate", "via": "tmux", "decision": "ask", "reasons": ["default"]}),
            serde_json::json!({"line": "frobnicate", "via": "bash", "decision": "passthrough", "reasons": []}),
        ]
    );
}

#[tokio::test]
async fn a_project_exempts_prefer_tmux_for_bash() {
    let h = Harness::new().await;
    h.project_policy(
        r#"
        [[rules]]
        id = "make-in-bash"
        command = "make"
        via = "bash"
        action = "allow"
        exempts = ["prefer-tmux"]
        "#,
    );

    decides!(h, {
        "make" => (_, _, Allow, "make-in-bash"),
        "npm test" => (_, _, Deny, "prefer-tmux"),
    });
}

#[tokio::test]
async fn a_broken_policy_denies_a_bash_call() {
    let h = Harness::new().await;
    h.project_policy("[[rules]]\nid = \"x\"\n");

    let (decision, reason) = h.bash("ls");

    assert_eq!(decision, Deny);
    assert!(reason.contains("policy:load_error"), "{reason}");
}

#[tokio::test]
async fn a_rule_reads_the_parts_its_grammar_names() {
    let h = Harness::new().await;
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
    decides!(h, {
        "deploy staging" => (Allow, _),
        "deploy --forc staging" => (Ask, _),
        "deploy -o /tmp/out staging" => (Ask, _),
        "deploy -o out staging" => (Allow, _),
        "deploy prod" => (Ask, _),
        "deploy staging $X" => (Ask, _),
        "deploy --region staging" => (Ask, _),
        "deploy staging --dry-run" => (Allow, _),
    });
}

/// The built-in policy over common lines, from the project root, on tmux.
#[tokio::test]
async fn the_builtin_policy_decides_common_lines() {
    let h = Harness::new().await;
    decides!(h, {
        "ls" => (Allow, _),
        "ls -la src" => (Allow, _),
        "ls --frob /etc src" => (Ask, _),
        "ls /etc" => (Ask, _),
        "ls | wc -l" => (Allow, _),
        "cat src/*.rs" => (Allow, _),
        "cat {notes,/etc/shadow}" => (Ask, _),
        "cat $F" => (Ask, _),
        "cat notes $F" => (Ask, _),
        "/bin/cat notes" => (Allow, _),
        "./bin/cat notes" => (Ask, _),
        "head -5 notes" => (Allow, _),
        "head -5 /etc/passwd" => (Ask, _),
        "tail +5 notes" => (Allow, _),
        "grep -A 3 foo src" => (Allow, _),
        "grep -e x /etc/shadow" => (Ask, _),
        "rg foo" => (Allow, _),
        "rg --pre ./x foo" => (Ask, _),
        "sort -o out notes" => (Allow, _),
        "cp notes backup" => (Allow, _),
        "cp notes /tmp/x" => (Ask, _),
        "rm notes" => (Allow, _),
        "rm -r src" => (Ask, _),
        "rm --recur src" => (Ask, _),
        "echo hi > out" => (Allow, _),
        "echo hi > /tmp/out" => (Ask, _),
        "echo $(date)" => (Allow, _),
        "find . -name '*.rs'" => (Allow, _),
        "find /etc -name x" => (Ask, _),
        "find . -delete" => (Ask, _),
        "find . -fprint /tmp/x" => (Ask, _),
        "find . -exec rm {} \\;" => (Ask, _),
        "sed 's/a/b/' notes" => (Allow, _),
        "sed -n '/x/p' notes" => (Allow, _),
        "sed 's/a/b/w /tmp/x' notes" => (Ask, _),
        "awk '{print $1}' notes" => (Allow, _),
        "git status" => (Allow, _),
        "git log -5" => (Allow, _),
        "git log --oneline -5" => (Allow, _),
        "git --frob status" => (Ask, _),
        "git --exec-path=/tmp/x status" => (Deny, _),
        "git log @{u}.." => (Allow, _),
        "git diff HEAD~1" => (Allow, _),
        "git -c core.pager=x log" => (Deny, _),
        "git frobnicate" => (Ask, Ask),
        "git update-index --cacheinfo 100644,abc,nix/backups.nix" => (Ask, Pass),
        "git hash-object -w notes" => (Ask, Pass),
        "git update-ref refs/heads/x abc" => (Ask, Pass),
        "git merge-base --is-ancestor HEAD main" => (Ask, Pass),
        "git push" => (Ask, _),
        "git push --force origin main" => (Ask, _),
        "git push -q origin main" => (Ask, Pass),
        "git push -u origin feature" => (Ask, Pass),
        "git push --quiet --set-upstream origin feature" => (Ask, Pass),
        "git push -qf origin main" => (Ask, "git-push-force"),
        "git stash list" => (Allow, _),
        "git stash drop" => (Ask, _),
        "git stash $X" => (Ask, _),
        "git branch" => (Allow, _),
        "git branch -d x" => (Ask, _),
        "git tag" => (Allow, _),
        "git tag -Xl v2" => (Ask, _),
        "git remote -v" => (Allow, _),
        "git config user.name" => (Allow, _),
        "git config user.name x" => (Ask, _),
        "git config --global user.name" => (Allow, _),
        "cargo build" => (Allow, _),
        "cargo build --release" => (Allow, _),
        "cargo test --release -p core foo" => (Allow, _),
        "cargo frob --manifest-path=/x" => (Ask, _),
        "cargo +nightly test foo" => (Allow, _),
        "cargo -C / build" => (Ask, _),
        "cargo test --manifest-path /x/Cargo.toml" => (Ask, _),
        "cargo build --target-dir /tmp/t" => (Ask, _),
        "cargo install ripgrep" => (Ask, _),
        "curl x" => (Ask, _),
        "/usr/bin/curl x" => (Ask, _),
        "eval true" => (Deny, _),
        "$X arg" => (Deny, _),
        // Inner commands
        "timeout 5 ls" => (Allow, _),
        "timeout 5 rm -r x" => (Ask, _),
        "nice -n 5 eval x" => (Deny, _),
        "nohup cat notes" => (Allow, _),
        "xargs rm" => (Ask, _),
        "xargs echo" => (Allow, _),
        "find . -exec eval x \\;" => (Deny, _),
        "find /etc -exec cat {} \\;" => (Ask, _),
        "find . -name x -exec grep -l y {} +" => (Ask, _),
        // Follow-up: `find src -name '*.rs' -exec wc -l {} +` could get Allow once `{}` is a path under a start point.
        "find . -execdir cat notes \\;" => (Ask, _),
        "find . -exec sh -c 'cat {}' \\;" => (Deny, _),
        "sh -c 'ls src'" => (Allow, _),
        "bash -lc 'ls src | wc -l'" => (Allow, _),
        "bash -c 'cat /etc/shadow'" => (Ask, _),
        "bash -lc \"$X\"" => (Deny, _),
        "curl x | bash" => (Deny, _),
        "bash script.sh" => (Ask, _),
        "timeout 5 $X" => (Deny, _),
        // Variables
        "LD_PRELOAD=x ls" => (Ask, _),
        "env LD_PRELOAD=x ls" => (Ask, _),
        "export PATH=/tmp" => (Ask, _),
        "PATH=/tmp; ls" => (Ask, _),
        "X=1 ls" => (Allow, _),
        "export X=1" => (Allow, _),
        // Context: a wrapper's `with` sets the context of its inner command
        "env X=1 cat notes" => (Allow, _),
        "env -C src cat notes" => (Allow, _),
        "env -C /etc cat shadow" => (Ask, _),
        "sudo -u root id" => (Ask, _),
        "sudo eval x" => (Deny, _),
        "sudo -i" => (Ask, _),
        "ssh prod ls" => (Ask, _),
        "ssh prod 'eval x'" => (Deny, _),
        "ssh -o ProxyCommand=x prod ls" => (Ask, _),
        // Tilde paths remain Unknown to path analysis, so the tmux-only
        // outside-project guard asks; native Bash leaves this to its host.
        "cat ~/notes" => (Ask, Pass),
        // A cd moves the rest of its && chain; after ;, || or a newline it may have failed
        "cd src" => (Allow, _),
        "cd /etc" => (Ask, _),
        "cd src && cat notes" => (Allow, _),
        "cd src && cd .. && cat notes" => (Allow, _),
        "pushd src && cat notes" => (Allow, _),
        "cd src; cat notes" => (Ask, _),
        "cd src || cat notes" => (Ask, _),
        "popd && cat notes" => (Ask, _),
        "cd src && pwd" => (Allow, _),
        "(cd src) && cat notes" => (Allow, _),
        "cd src | cat notes" => (Allow, _),
        "{ cd src; } && cat notes" => (Ask, _),
        "for x in a b; do cat notes; cd src; done" => (Ask, _),
        "ls() { cd src; }; ls && cat notes" => (Ask, _),
        "CDPATH=x cd src" => (Ask, _),
        // A relative name stays a path, which never meets a rule for a bare name
        "cd src && ./run" => (Ask, _),
        "ssh prod ./deploy.sh" => (Ask, _),
        "cd src && source env.sh" => (Deny, _),
        ". ./env.sh" => (Deny, _),
    });
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
    decides!(h, shell, {
        "./scripts/bench" => Allow,
        "scripts/bench" => Allow,
        "bench" => Ask,
        format!("{}/scripts/bench", h.dir().display()) => Allow,
    });

    h.run(&shell, "cd sub").await;

    decides!(h, shell, {
        "../scripts/bench" => Allow,
        "./scripts/bench" => Ask,
        "ls .." => Allow,
    });
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
    decides!(h, shell, {
        "deploy" => Allow,
        "cat notes" => Allow,
        "ls" => Allow,
        "sudo cat notes" => Allow,
        "sudo cat /etc/shadow" => Ask,
        "sudo -u nobody cat notes" => Ask,
        "sudo -u nobody eval x" => Deny,
    });

    // The prompt reports its host with `hostname`, as a remote shell would.
    h.run(&shell, "hostname() { echo elsewhere; }").await;

    // No path there is in the project; the listing rule exempts only the path rule.
    decides!(h, shell, {
        "deploy" => Ask,
        "cat notes" => Ask,
        "ls" => Allow,
        "LD_PRELOAD=x ls" => Ask,
        "eval true" => Deny,
    });
}

#[tokio::test]
async fn a_command_typed_into_a_running_program_meets_no_rule_of_this_machine() {
    let h = Harness::new().await;
    let shell = h.shell().await;
    h.start(&shell, "cat").await;

    decides!(h, shell, {
        "ls" => Ask,
        "eval true" => Deny,
    });
}

#[tokio::test]
async fn a_path_in_a_flag_value_is_checked() {
    let h = Harness::new().await;
    decides!(h, {
        "sort -o/tmp/evil notes" => (Ask, "outside-project", _, _),
        "cp -t/tmp/evil notes" => (Ask, "outside-project", _, _),
        "git log --output=/tmp/evil" => (Ask, "outside-project", _, _),
        "git -C / log" => (Ask, "outside-project", _, _),
    });
}

#[tokio::test]
async fn an_input_redirect_is_a_path_the_command_reads() {
    let h = Harness::new().await;
    decides!(h, {
        "cat < /etc/shadow" => (Ask, "outside-project", _, _),
    });
}

#[tokio::test]
async fn a_git_command_that_writes_asks() {
    let h = Harness::new().await;
    decides!(h, {
        "git config core.fsmonitor 'sh -c id'" => (Ask, _),
        "git branch -D main" => (Ask, _),
        "git tag -d v1" => (Ask, _),
        "git remote set-url origin evil" => (Ask, _),
    });
}

#[tokio::test]
async fn an_awk_or_sed_script_that_can_run_a_command_asks() {
    let h = Harness::new().await;
    decides!(h, {
        "awk 'BEGIN{print \"id\" | \"sh\"}'" => (Ask, "awk-program", _, _),
        "awk -f prog.awk notes" => (Ask, "awk-program", _, _),
        "sed -f prog.sed notes" => (Ask, "sed-script", _, _),
    });
}

#[tokio::test]
async fn rules_see_words_without_their_quotes() {
    let h = Harness::new().await;
    decides!(h, {
        "'eval' true" => Deny,
        "cat '/etc/shadow'" => (Ask, _),
        "cat \"/etc/shadow\"" => (Ask, _),
        "echo hi > '/tmp/evil'" => (Ask, _),
        "rm \"-r\" src" => Ask,
        "cargo \"install\" x" => (Ask, _),
    });
}

#[tokio::test]
async fn a_command_substitution_meets_the_rules() {
    let h = Harness::new().await;
    decides!(h, {
        "FOO=$(eval true) ls" => Deny,
        "echo ${x:-$(eval true)}" => Deny,
        "echo hi > $(eval true)" => Deny,
        "for i in $(eval true); do echo; done" => Deny,
        "cat <<EOF\n$(eval true)\nEOF" => Deny,
    });
}

#[tokio::test]
async fn a_sibling_directory_is_outside_the_project() {
    let h = Harness::new().await;
    decides!(h, {
        format!("rm {}-evil/notes", h.dir().display()) => (Ask, "outside-project", _, _),
    });
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
