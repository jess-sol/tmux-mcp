//! Policy engine: evaluates whether a command should run in a given pane context.
//!
//! This module has a narrow public interface (`evaluate`) so the implementation
//! can be replaced without touching the rest of the codebase.

pub mod approval;
pub mod parse;

/// Pane context used for policy evaluation. Derives PartialEq so adding
/// a field automatically includes it in approval drift detection.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PaneContext {
    pub hostname: Option<String>,
    pub cwd: Option<String>,
    pub foreground: Option<String>,
    pub user: Option<String>,
}

pub struct PolicyResult {
    pub decision: Decision,
    pub rule: String,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

/// Commands that are always safe to auto-approve (read-only, no side effects).
/// Matched by prefix: "git status" matches "git status --short".
const SAFE_COMMANDS: &[&str] = &[
    // shell builtins (no-ops / safe)
    ":",
    // filesystem reads
    "ls",
    "cat",
    "head",
    "tail",
    "wc",
    "sort",
    "uniq",
    "grep",
    "rg",
    "find",
    "fd",
    "tree",
    // info
    "echo",
    "printf",
    "pwd",
    "whoami",
    "hostname",
    "date",
    "uname",
    "env",
    "id",
    "which",
    "file",
    "stat",
    "realpath",
    // comparison / hashing
    "diff",
    "md5sum",
    "sha256sum",
    "base64",
    // system info
    "ps",
    "uptime",
    // harmless utilities
    "seq",
    "sleep",
    "true",
    "false",
    "yes",
    "test",
    // git read-only
    "git status",
    "git log",
    "git diff",
    "git show",
    "git branch",
    // cargo
    "cargo check",
    "cargo test",
    "cargo build",
    "cargo clippy",
];

/// Evaluate whether a command should be allowed in the given pane context.
pub fn evaluate(command: &str, _ctx: &PaneContext) -> PolicyResult {
    let cmd = command.trim();
    for safe in SAFE_COMMANDS {
        if cmd == *safe || cmd.starts_with(&format!("{} ", safe)) {
            return PolicyResult {
                decision: Decision::Allow,
                rule: format!("safe_command:{}", safe),
            };
        }
    }
    PolicyResult {
        decision: Decision::Ask,
        rule: "default".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local_ctx() -> PaneContext {
        PaneContext {
            hostname: None,
            cwd: Some("/home/user".into()),
            foreground: Some("bash".into()),
            user: Some("user".into()),
        }
    }

    #[test]
    fn safe_command_exact() {
        let r = evaluate("ls", &local_ctx());
        assert_eq!(r.decision, Decision::Allow);
        assert_eq!(r.rule, "safe_command:ls");
    }

    #[test]
    fn safe_command_with_args() {
        let r = evaluate("ls -la /tmp", &local_ctx());
        assert_eq!(r.decision, Decision::Allow);
        assert_eq!(r.rule, "safe_command:ls");
    }

    #[test]
    fn safe_multi_word_command() {
        let r = evaluate("git status --short", &local_ctx());
        assert_eq!(r.decision, Decision::Allow);
        assert_eq!(r.rule, "safe_command:git status");
    }

    #[test]
    fn unsafe_command() {
        let r = evaluate("rm -rf /", &local_ctx());
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(r.rule, "default");
    }

    #[test]
    fn unknown_command() {
        let r = evaluate("rustup update", &local_ctx());
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(r.rule, "default");
    }

    #[test]
    fn whitespace_trimmed() {
        let r = evaluate("  ls -la  ", &local_ctx());
        assert_eq!(r.decision, Decision::Allow);
    }

    #[test]
    fn partial_match_not_allowed() {
        // "lsblk" should NOT match "ls"
        let r = evaluate("lsblk", &local_ctx());
        assert_eq!(r.decision, Decision::Ask);
    }

    #[test]
    fn cargo_commands() {
        assert_eq!(evaluate("cargo test", &local_ctx()).decision, Decision::Allow);
        assert_eq!(evaluate("cargo test --release", &local_ctx()).decision, Decision::Allow);
        assert_eq!(evaluate("cargo build", &local_ctx()).decision, Decision::Allow);
        assert_eq!(evaluate("cargo clippy -- -W warnings", &local_ctx()).decision, Decision::Allow);
    }

    #[test]
    fn cargo_unknown_subcommand() {
        // "cargo install" is not in the safe list
        assert_eq!(evaluate("cargo install foo", &local_ctx()).decision, Decision::Ask);
    }
}
