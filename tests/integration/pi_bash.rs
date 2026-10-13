//! Native Pi Bash checks must work without tmux or daemon approvals.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use tempfile::TempDir;

fn check(home: &Path, cwd: &Path, line: &str) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tmux-mcp"))
        .args(["policy-check", "--pi"])
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env("HOME", home)
        .env("TMUX_MCP_HARNESS", "pi")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("policy-check");
    let payload = json!({"tool_name": "bash", "cwd": cwd, "tool_input": {"command": line}});
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn native_bash_checks_all_decisions_without_tmux() {
    let dir = TempDir::new().unwrap();
    let cwd = dir.path().join("project");
    std::fs::create_dir(&cwd).unwrap();
    for (line, decision, reason) in [
        ("ls", "passthrough", ""),
        ("git push --force origin main", "ask", "git-push-force"),
        ("rm -r out", "ask", "rm-r"),
        ("kill -9 1234", "ask", "sigkill"),
        ("chmod -R u+w out", "ask", "permissions-recursive"),
        ("eval x", "deny", "eval"),
        ("cargo build", "deny", "prefer-tmux"),
        ("ssh prod", "deny", "ssh-in-a-pane"),
        ("kubectl get pods", "deny", "infra-in-a-pane"),
        ("tmux list-panes", "deny", "tmux-cli"),
        ("helm template chart", "allow", "helm-local"),
    ] {
        let answer = check(dir.path(), &cwd, line);
        assert_eq!(answer["decision"], decision, "{line}: {answer}");
        assert!(
            answer["rule"].as_str().unwrap().contains(reason),
            "{line}: {answer}"
        );
        assert_eq!(answer["context"]["cwd"], cwd.to_str().unwrap());
        assert!(answer["reasons"].is_array());
        assert!(answer.get("hookSpecificOutput").is_none());
    }
    assert!(!dir.path().join("tmux-mcp").exists());
}

#[test]
fn native_bash_uses_the_payload_cwd_and_pi_policy_exemptions() {
    let dir = TempDir::new().unwrap();
    let project = dir.path().join("project");
    let nested = project.join("nested");
    let other = dir.path().join("other");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::create_dir(&other).unwrap();
    std::fs::create_dir(project.join(".pi")).unwrap();
    std::fs::write(
        project.join(".pi/tmux-mcp.toml"),
        r#"
[[rules]]
id = "local-build"
command = "cargo"
via = "bash"
action = "allow"
exempts = ["prefer-tmux"]
[[rules]]
id = "tmux-only"
command = "ls"
action = "deny"
"#,
    )
    .unwrap();
    assert_eq!(
        check(dir.path(), &nested, "cargo build")["decision"],
        "allow"
    );
    assert_eq!(check(dir.path(), &other, "cargo build")["decision"], "deny");
    assert_eq!(check(dir.path(), &nested, "ls")["decision"], "passthrough");
    std::fs::write(project.join(".pi/tmux-mcp.toml"), "invalid = true").unwrap();
    let answer = check(dir.path(), &nested, "ls");
    assert_eq!(answer["decision"], "deny");
    assert!(
        answer["rule"]
            .as_str()
            .unwrap()
            .contains("policy:load_error")
    );
}

#[test]
fn pi_skill_toml_examples_load_with_the_current_engine() {
    let doc = include_str!("../../pi/skills/tmux-policy/SKILL.md");
    let dir = TempDir::new().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".pi")).unwrap();
    for block in doc.split("```toml\n").skip(1) {
        let toml = block.split("```").next().unwrap();
        std::fs::write(project.join(".pi/tmux-mcp.toml"), toml).unwrap();
        // Use the subprocess's isolated HOME, not the developer's policies.
        let answer = check(dir.path(), &project, "true");
        assert!(
            !answer["rule"]
                .as_str()
                .unwrap()
                .contains("policy:load_error"),
            "{toml}\n{answer}"
        );
    }
}
