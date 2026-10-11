//! Command parsing: brush-parser → flat list of CommandInfo.
//!
//! Parses a command string into a flat list of `CommandInfo` nodes, one per
//! command that will execute. Pipelines, logical operators, compound commands
//! and command substitutions produce additional independent entries.

use brush_parser::ast;
use brush_parser::word::WordPiece;

// --- Types ---

/// Information about a single command that will execute.
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// Command name (always a literal string — structural check denies expansions).
    pub name: String,
    /// Literal arguments we can see.
    pub args: Vec<String>,
    /// False if dynamic/unknown args possible (expansions, stdin args, etc.).
    pub args_complete: bool,
    /// Shell redirects attached to this command.
    pub redirects: Vec<RedirectInfo>,
}

/// A shell redirect (>, >>, <, etc.) attached to a command.
#[derive(Debug, Clone)]
pub struct RedirectInfo {
    /// The target path/word.
    pub target: String,
    /// True for write redirects (>, >>, &>, >|). False for read (<, <<, <<<).
    pub is_write: bool,
    /// True if target contains expansions ($(), ${}, etc.).
    pub has_expansion: bool,
}

impl CommandInfo {
    #[cfg(test)]
    pub(crate) fn simple(name: &str) -> Self {
        Self {
            name: name.to_string(),
            args: Vec::new(),
            args_complete: true,
            redirects: Vec::new(),
        }
    }
}

/// Error returned when brush-parser can't parse the command.
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
}

// --- Public API ---

/// Parse a command string into the flat list of commands it runs.
pub fn parse_command(command: &str) -> Result<Vec<CommandInfo>, ParseError> {
    let program = parse_with_brush(command)?;
    let mut commands = Vec::new();
    extract_from_program(&program, &mut commands);
    Ok(commands)
}

// --- brush-parser integration ---

fn parse_with_brush(command: &str) -> Result<ast::Program, ParseError> {
    use std::io::BufReader;
    let reader = BufReader::new(command.as_bytes());
    let mut parser = brush_parser::Parser::new(
        reader,
        &brush_parser::ParserOptions::default(),
        &brush_parser::SourceInfo::default(),
    );
    parser
        .parse_program()
        .map_err(|e| ParseError { message: format!("{}", e) })
}

// --- AST walking ---

fn extract_from_program(program: &ast::Program, out: &mut Vec<CommandInfo>) {
    for cc in &program.complete_commands {
        extract_from_compound_list(cc, out);
    }
}

fn extract_from_compound_list(list: &ast::CompoundList, out: &mut Vec<CommandInfo>) {
    for item in &list.0 {
        extract_from_and_or_list(&item.0, out);
    }
}

fn extract_from_and_or_list(and_or: &ast::AndOrList, out: &mut Vec<CommandInfo>) {
    extract_from_pipeline(&and_or.first, out);
    for additional in &and_or.additional {
        let pipeline = match additional {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => p,
        };
        extract_from_pipeline(pipeline, out);
    }
}

fn extract_from_pipeline(pipeline: &ast::Pipeline, out: &mut Vec<CommandInfo>) {
    for command in &pipeline.seq {
        extract_from_command(command, out);
    }
}

fn extract_from_command(command: &ast::Command, out: &mut Vec<CommandInfo>) {
    match command {
        ast::Command::Simple(simple) => extract_from_simple_command(simple, out),
        ast::Command::Compound(compound, _) => extract_from_compound_command(compound, out),
        ast::Command::Function(_) | ast::Command::ExtendedTest(_) => {}
    }
}

/// Extract a SimpleCommand, followed by the commands of its substitutions.
fn extract_from_simple_command(simple: &ast::SimpleCommand, out: &mut Vec<CommandInfo>) {
    let name_word = match &simple.word_or_name {
        Some(w) => w,
        None => return,
    };

    let mut args = Vec::new();
    let mut args_complete = !word_has_expansion(&name_word.value);
    let mut cmd_sub_commands = Vec::new();

    // Collect args from suffix
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    if word_has_expansion(&word.value) {
                        args_complete = false;
                    }
                    args.push(word.value.clone());
                    extract_command_subs_from_word(&word.value, &mut cmd_sub_commands);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    extract_from_compound_list(&subshell.list, &mut cmd_sub_commands);
                    args_complete = false;
                }
                _ => {}
            }
        }
    }

    // Check prefix for process substitutions
    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    extract_from_compound_list(&subshell.list, &mut cmd_sub_commands);
                    args_complete = false;
                }
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    if word_has_expansion(&word.value) {
                        args_complete = false;
                    }
                    extract_command_subs_from_word(&word.value, &mut cmd_sub_commands);
                }
                _ => {}
            }
        }
    }

    // Extract redirects from prefix and suffix
    let mut redirects = Vec::new();
    if let Some(prefix) = &simple.prefix {
        extract_redirects(&prefix.0, &mut redirects);
    }
    if let Some(suffix) = &simple.suffix {
        extract_redirects(&suffix.0, &mut redirects);
    }

    out.push(CommandInfo {
        name: name_word.value.clone(),
        args,
        args_complete,
        redirects,
    });
    out.extend(cmd_sub_commands);
}

fn extract_from_compound_command(compound: &ast::CompoundCommand, out: &mut Vec<CommandInfo>) {
    match compound {
        ast::CompoundCommand::Subshell(s) => extract_from_compound_list(&s.list, out),
        ast::CompoundCommand::BraceGroup(g) => extract_from_compound_list(&g.list, out),
        ast::CompoundCommand::IfClause(c) => {
            extract_from_compound_list(&c.condition, out);
            extract_from_compound_list(&c.then, out);
            if let Some(elses) = &c.elses {
                for el in elses {
                    if let Some(cond) = &el.condition { extract_from_compound_list(cond, out); }
                    extract_from_compound_list(&el.body, out);
                }
            }
        }
        ast::CompoundCommand::WhileClause(c) | ast::CompoundCommand::UntilClause(c) => {
            extract_from_compound_list(&c.0, out);
            extract_from_compound_list(&c.1.list, out);
        }
        ast::CompoundCommand::ForClause(c) => extract_from_compound_list(&c.body.list, out),
        ast::CompoundCommand::CaseClause(c) => {
            for item in &c.cases {
                if let Some(cmd) = &item.cmd { extract_from_compound_list(cmd, out); }
            }
        }
        ast::CompoundCommand::ArithmeticForClause(c) => extract_from_compound_list(&c.body.list, out),
        ast::CompoundCommand::Arithmetic(_) => {}
    }
}

// --- Redirect extraction ---

/// Extract redirect info from prefix/suffix items.
fn extract_redirects(items: &[ast::CommandPrefixOrSuffixItem], out: &mut Vec<RedirectInfo>) {
    for item in items {
        if let ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) = item {
            match redirect {
                ast::IoRedirect::File(_, kind, target) => {
                    let is_write = matches!(
                        kind,
                        ast::IoFileRedirectKind::Write
                            | ast::IoFileRedirectKind::Append
                            | ast::IoFileRedirectKind::Clobber
                            | ast::IoFileRedirectKind::ReadAndWrite
                    );
                    if let ast::IoFileRedirectTarget::Filename(word) = target {
                        out.push(RedirectInfo {
                            target: word.value.clone(),
                            is_write,
                            has_expansion: word_has_expansion(&word.value),
                        });
                    }
                }
                ast::IoRedirect::OutputAndError(word, _) => {
                    out.push(RedirectInfo {
                        target: word.value.clone(),
                        is_write: true,
                        has_expansion: word_has_expansion(&word.value),
                    });
                }
                ast::IoRedirect::HereDocument(_, _) | ast::IoRedirect::HereString(_, _) => {
                    // Here-docs/strings are input, not file targets — skip
                }
            }
        }
    }
}

// --- Word analysis ---

/// Check if a word value contains shell expansions ($VAR, $(cmd), etc.).
pub(crate) fn word_has_expansion(word_value: &str) -> bool {
    let options = brush_parser::ParserOptions::default();
    match brush_parser::word::parse(word_value, &options) {
        Ok(pieces) => pieces.iter().any(|p| match &p.piece {
            WordPiece::ParameterExpansion(_)
            | WordPiece::CommandSubstitution(_)
            | WordPiece::BackquotedCommandSubstitution(_)
            | WordPiece::ArithmeticExpression(_) => true,
            WordPiece::DoubleQuotedSequence(inner) => {
                inner.iter().any(|ip| matches!(
                    &ip.piece,
                    WordPiece::ParameterExpansion(_)
                    | WordPiece::CommandSubstitution(_)
                    | WordPiece::BackquotedCommandSubstitution(_)
                    | WordPiece::ArithmeticExpression(_)
                ))
            }
            _ => false,
        }),
        Err(_) => true,
    }
}

fn extract_command_subs_from_word(word_value: &str, out: &mut Vec<CommandInfo>) {
    let options = brush_parser::ParserOptions::default();
    if let Ok(pieces) = brush_parser::word::parse(word_value, &options) {
        for p in &pieces {
            extract_command_subs_from_piece(&p.piece, out);
        }
    }
}

fn extract_command_subs_from_piece(piece: &WordPiece, out: &mut Vec<CommandInfo>) {
    match piece {
        WordPiece::CommandSubstitution(cmd_str)
        | WordPiece::BackquotedCommandSubstitution(cmd_str) => {
            if let Ok(inner) = parse_command(cmd_str) {
                out.extend(inner);
            }
        }
        WordPiece::DoubleQuotedSequence(inner) => {
            for ip in inner {
                extract_command_subs_from_piece(&ip.piece, out);
            }
        }
        _ => {}
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    // --- Dependency contract: brush-parser ---

    fn parse_program(input: &str) -> ast::Program {
        parse_with_brush(input).unwrap_or_else(|e| panic!("Failed to parse '{}': {}", input, e.message))
    }

    #[test]
    fn brush_parses_simple_command() {
        let prog = parse_program("ls -la");
        let pipeline = &prog.complete_commands[0].0[0].0.first;
        assert_eq!(pipeline.seq.len(), 1);
        match &pipeline.seq[0] {
            ast::Command::Simple(s) => assert_eq!(s.word_or_name.as_ref().unwrap().value, "ls"),
            _ => panic!("expected SimpleCommand"),
        }
    }

    #[test]
    fn brush_parses_pipeline() {
        let prog = parse_program("ls | grep foo");
        assert_eq!(prog.complete_commands[0].0[0].0.first.seq.len(), 2);
    }

    #[test]
    fn brush_parses_and_or_list() {
        let and_or = &parse_program("a && b || c").complete_commands[0].0[0].0;
        assert_eq!(and_or.additional.len(), 2);
    }

    #[test]
    fn brush_parses_command_substitution_in_word() {
        let pieces = brush_parser::word::parse("$(date)", &brush_parser::ParserOptions::default()).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::CommandSubstitution(_))));
    }

    #[test]
    fn brush_parses_subshell() {
        let pipeline = &parse_program("(ls)").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::Subshell(_), _)));
    }

    #[test]
    fn brush_parses_brace_group() {
        let pipeline = &parse_program("{ ls; }").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::BraceGroup(_), _)));
    }

    #[test]
    fn brush_parses_if_clause() {
        let pipeline = &parse_program("if true; then echo yes; fi").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::IfClause(_), _)));
    }

    #[test]
    fn brush_parses_for_clause() {
        let pipeline = &parse_program("for x in a b; do echo $x; done").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::ForClause(_), _)));
    }

    #[test]
    fn brush_parses_quoted_string_not_split() {
        let pipeline = &parse_program("echo 'hello | world'").complete_commands[0].0[0].0.first;
        assert_eq!(pipeline.seq.len(), 1);
    }

    #[test]
    fn brush_rejects_unclosed_quote() {
        assert!(parse_with_brush("echo 'hello").is_err());
    }

    #[test]
    fn brush_detects_expansion_in_word() {
        let pieces = brush_parser::word::parse("$VAR", &brush_parser::ParserOptions::default()).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::ParameterExpansion(_))));
    }

    #[test]
    fn brush_detects_command_sub_in_word() {
        let pieces = brush_parser::word::parse("$(cmd)", &brush_parser::ParserOptions::default()).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::CommandSubstitution(_))));
    }

    // --- Helper to find command by name in flat list ---

    fn find_cmd<'a>(cmds: &'a [CommandInfo], name: &str) -> &'a CommandInfo {
        cmds.iter().find(|c| c.name == name)
            .unwrap_or_else(|| panic!("command '{}' not found in {:?}", name, cmds.iter().map(|c| &c.name).collect::<Vec<_>>()))
    }

    // --- Simple commands ---

    #[test]
    fn simple_command_extracts_name_and_args() {
        let cmds = parse_command("ls -la /tmp").unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].args, vec!["-la", "/tmp"]);
        assert!(cmds[0].args_complete);
    }

    #[test]
    fn command_with_no_args() {
        let cmds = parse_command("pwd").unwrap();
        assert_eq!(cmds[0].name, "pwd");
        assert!(cmds[0].args.is_empty());
    }

    // --- Pipelines ---

    #[test]
    fn pipeline_extracts_each_segment() {
        let cmds = parse_command("ls | grep foo").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "grep"));
    }

    #[test]
    fn pipeline_three_segments() {
        let cmds = parse_command("ls | grep foo | wc -l").unwrap();
        assert!(cmds.iter().any(|c| c.name == "wc"));
    }

    #[test]
    fn quoted_pipe_is_not_pipeline() {
        let cmds = parse_command("echo 'hello | world'").unwrap();
        assert_eq!(cmds.len(), 1);
    }

    // --- Logical operators ---

    #[test]
    fn and_list_extracts_both() {
        let cmds = parse_command("ls && echo done").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "echo"));
    }

    #[test]
    fn or_list_extracts_both() {
        let cmds = parse_command("ls || echo failed").unwrap();
        assert_eq!(cmds.len(), 2);
    }

    // --- Command substitution ---

    #[test]
    fn arg_with_command_substitution() {
        let cmds = parse_command("echo $(date)").unwrap();
        assert!(cmds.iter().any(|c| c.name == "echo"));
        assert!(cmds.iter().any(|c| c.name == "date"));
    }

    #[test]
    fn command_substitution_sets_args_incomplete() {
        let cmds = parse_command("rm $(find . -name '*.tmp')").unwrap();
        let rm = find_cmd(&cmds, "rm");
        assert!(!rm.args_complete);
    }

    #[test]
    fn expansion_in_arg_marks_incomplete() {
        let cmds = parse_command("rm -rf $DIR").unwrap();
        assert!(!cmds[0].args_complete);
    }

    #[test]
    fn literal_args_are_complete() {
        let cmds = parse_command("rm -rf /tmp").unwrap();
        assert!(cmds[0].args_complete);
    }

    // --- Compound commands ---

    #[test]
    fn subshell_extracts_inner() {
        let cmds = parse_command("(ls && pwd)").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "pwd"));
    }

    #[test]
    fn brace_group_extracts_inner() {
        let cmds = parse_command("{ ls; pwd; }").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "pwd"));
    }

    // --- Parse failures ---

    #[test]
    fn empty_string_parses_as_empty() {
        assert!(parse_command("").unwrap().is_empty());
    }

    #[test]
    fn unclosed_quote_fails() {
        assert!(parse_command("echo 'hello").is_err());
    }

    // --- time is a keyword ---

    #[test]
    fn time_is_keyword() {
        let cmds = parse_command("time make -j4").unwrap();
        assert_eq!(cmds[0].name, "make"); // brush-parser handles time as keyword
    }
}
