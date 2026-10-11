//! Command parsing: brush-parser → flat list of CommandInfo.
//!
//! Parses a command string into a flat list of `CommandInfo` nodes, one per
//! command that will execute. Pipelines, logical operators, compound commands
//! and command substitutions produce additional independent entries. Each
//! command's words lose their quotes, and its grammar parses them into parts.

use std::collections::BTreeMap;

use brush_parser::ast;
use brush_parser::word::{BraceExpressionOrText, WordPiece};

pub use super::grammar::Grammars;
use super::rules::TriVal;

// --- Types ---

/// Information about a single command that will execute.
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// The command name as written, quotes included.
    pub name: String,
    /// Shell redirects attached to this command.
    pub redirects: Vec<RedirectInfo>,
    /// The subcommand words below the command name: a list that ends in
    /// Unknown when a word cannot be known or is not one the grammar allows.
    pub subcommands: TriVal,
    /// The parts of every level that parsed, by name.
    pub args: BTreeMap<String, TriVal>,
    /// The directory the command runs in, relative to the shell's: Null when
    /// the grammar marks none.
    pub cwd: TriVal,
    /// The words the grammar marks as read paths, relative to `cwd`.
    pub reads: TriVal,
    /// The words the grammar marks as write paths, relative to `cwd`.
    pub writes: TriVal,
}

/// A shell redirect (>, >>, <, etc.) attached to a command.
#[derive(Debug, Clone)]
pub struct RedirectInfo {
    /// The target path without its quotes; as written when `has_expansion`.
    pub target: String,
    /// True for write redirects (>, >>, &>, >|). False for read (<).
    pub is_write: bool,
    /// True when the target cannot be known before it runs.
    pub has_expansion: bool,
}

/// Error returned when brush-parser can't parse the command.
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
}

/// One word of a command after quote removal.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Word {
    Known(String),
    /// Expanded at run time. `splits` when it can become several words, or none.
    Unknown { splits: bool },
}

// --- Public API ---

/// Parse a command string into the flat list of commands it runs.
pub fn parse_command(command: &str, grammars: &Grammars) -> Result<Vec<CommandInfo>, ParseError> {
    let program = parse_with_brush(command)?;
    let mut commands = Vec::new();
    Walker { grammars, out: &mut commands }.program(&program);
    Ok(commands)
}

/// The text of a word without its quotes, or None when the shell expands it.
pub(crate) fn literal(raw: &str) -> Option<String> {
    match word(raw) {
        Word::Known(text) => Some(text),
        Word::Unknown { .. } => None,
    }
}

/// A word as the command receives it: quotes removed, or Unknown when the
/// shell expands it. Glob and `~user` words stay as written; a brace word is
/// Unknown, since it can become several words.
pub(crate) fn word(raw: &str) -> Word {
    let options = brush_parser::ParserOptions::default();
    if let Ok(Some(pieces)) = brush_parser::word::parse_brace_expansions(raw, &options)
        && pieces.iter().any(|p| matches!(p, BraceExpressionOrText::Expr(_)))
    {
        return Word::Unknown { splits: true };
    }
    let Ok(pieces) = brush_parser::word::parse(raw, &options) else {
        return Word::Unknown { splits: true };
    };
    let mut text = String::new();
    let mut unknown: Option<bool> = None;
    for piece in &pieces {
        append_piece(&piece.piece, false, raw, &mut text, &mut unknown);
    }
    match unknown {
        Some(splits) => Word::Unknown { splits },
        None => Word::Known(text),
    }
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
    parser.parse_program().map_err(|e| ParseError { message: format!("{}", e) })
}

// --- AST walking ---

struct Walker<'a> {
    grammars: &'a Grammars,
    out: &'a mut Vec<CommandInfo>,
}

impl Walker<'_> {
    fn program(&mut self, program: &ast::Program) {
        for cc in &program.complete_commands {
            self.compound_list(cc);
        }
    }

    fn compound_list(&mut self, list: &ast::CompoundList) {
        for item in &list.0 {
            self.and_or_list(&item.0);
        }
    }

    fn and_or_list(&mut self, and_or: &ast::AndOrList) {
        self.pipeline(&and_or.first);
        for additional in &and_or.additional {
            let pipeline = match additional {
                ast::AndOr::And(p) | ast::AndOr::Or(p) => p,
            };
            self.pipeline(pipeline);
        }
    }

    fn pipeline(&mut self, pipeline: &ast::Pipeline) {
        for command in &pipeline.seq {
            match command {
                ast::Command::Simple(simple) => self.simple_command(simple),
                ast::Command::Compound(compound, _) => self.compound_command(compound),
                ast::Command::Function(_) | ast::Command::ExtendedTest(_) => {}
            }
        }
    }

    /// A simple command, followed by the commands of its substitutions.
    fn simple_command(&mut self, simple: &ast::SimpleCommand) {
        let Some(name) = &simple.word_or_name else { return };
        let prefix = simple.prefix.iter().flat_map(|p| &p.0).map(|item| (item, false));
        let suffix = simple.suffix.iter().flat_map(|s| &s.0).map(|item| (item, true));
        let mut words = Vec::new();
        let mut raw_words = Vec::new();
        for (item, in_suffix) in prefix.chain(suffix) {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(w) => {
                    if in_suffix {
                        words.push(word(&w.value));
                    }
                    raw_words.push(w.value.clone());
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    if in_suffix {
                        words.push(Word::Unknown { splits: false });
                    }
                    self.compound_list(&subshell.list);
                }
                _ => {}
            }
        }

        let parsed = match literal(&name.value) {
            Some(n) => self.grammars.parse(&n, &words),
            None => self.grammars.parse("", &[]),
        };
        let items = simple.prefix.iter().flat_map(|p| &p.0).chain(simple.suffix.iter().flat_map(|s| &s.0));
        self.out.push(CommandInfo {
            name: name.value.clone(),
            redirects: redirects(items),
            subcommands: parsed.subcommands,
            args: parsed.args,
            cwd: parsed.cwd,
            reads: parsed.reads,
            writes: parsed.writes,
        });
        for raw in std::iter::once(&name.value).chain(&raw_words) {
            self.substitutions(raw);
        }
    }

    fn compound_command(&mut self, compound: &ast::CompoundCommand) {
        match compound {
            ast::CompoundCommand::Subshell(s) => self.compound_list(&s.list),
            ast::CompoundCommand::BraceGroup(g) => self.compound_list(&g.list),
            ast::CompoundCommand::IfClause(c) => {
                self.compound_list(&c.condition);
                self.compound_list(&c.then);
                for el in c.elses.iter().flatten() {
                    if let Some(cond) = &el.condition {
                        self.compound_list(cond);
                    }
                    self.compound_list(&el.body);
                }
            }
            ast::CompoundCommand::WhileClause(c) | ast::CompoundCommand::UntilClause(c) => {
                self.compound_list(&c.0);
                self.compound_list(&c.1.list);
            }
            ast::CompoundCommand::ForClause(c) => self.compound_list(&c.body.list),
            ast::CompoundCommand::CaseClause(c) => {
                for item in &c.cases {
                    if let Some(cmd) = &item.cmd {
                        self.compound_list(cmd);
                    }
                }
            }
            ast::CompoundCommand::ArithmeticForClause(c) => self.compound_list(&c.body.list),
            ast::CompoundCommand::Arithmetic(_) => {}
        }
    }

    /// The commands of the `$(…)` and backquote substitutions in a word.
    fn substitutions(&mut self, raw: &str) {
        let options = brush_parser::ParserOptions::default();
        if let Ok(pieces) = brush_parser::word::parse(raw, &options) {
            for p in &pieces {
                self.substitution_piece(&p.piece);
            }
        }
    }

    fn substitution_piece(&mut self, piece: &WordPiece) {
        match piece {
            WordPiece::CommandSubstitution(text) | WordPiece::BackquotedCommandSubstitution(text) => {
                if let Ok(program) = parse_with_brush(text) {
                    self.program(&program);
                }
            }
            WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
                for ip in inner {
                    self.substitution_piece(&ip.piece);
                }
            }
            _ => {}
        }
    }
}

fn redirects<'a>(items: impl Iterator<Item = &'a ast::CommandPrefixOrSuffixItem>) -> Vec<RedirectInfo> {
    let mut out = Vec::new();
    for item in items {
        let ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) = item else { continue };
        let (target, is_write) = match redirect {
            ast::IoRedirect::File(_, kind, ast::IoFileRedirectTarget::Filename(w)) => {
                let is_write = matches!(
                    kind,
                    ast::IoFileRedirectKind::Write
                        | ast::IoFileRedirectKind::Append
                        | ast::IoFileRedirectKind::Clobber
                        | ast::IoFileRedirectKind::ReadAndWrite
                );
                (w, is_write)
            }
            ast::IoRedirect::OutputAndError(w, _) => (w, true),
            _ => continue,
        };
        let (target, has_expansion) = match literal(&target.value) {
            Some(text) => (text, false),
            None => (target.value.clone(), true),
        };
        out.push(RedirectInfo { target, is_write, has_expansion });
    }
    out
}

// --- Word analysis ---

/// Appends what `piece` contributes to a word's text. An expansion makes the
/// word Unknown: one that may split when unquoted, or when it is `$@`-like.
fn append_piece(piece: &WordPiece, quoted: bool, raw: &str, text: &mut String, unknown: &mut Option<bool>) {
    match piece {
        WordPiece::Text(s) | WordPiece::SingleQuotedText(s) => text.push_str(s),
        WordPiece::AnsiCQuotedText(s) if !s.contains('\\') => text.push_str(s),
        WordPiece::AnsiCQuotedText(_) => *unknown = Some(unknown.unwrap_or(false)),
        WordPiece::TildePrefix(user) => {
            text.push('~');
            text.push_str(user);
        }
        WordPiece::EscapeSequence(s) => text.push_str(s.strip_prefix('\\').unwrap_or(s)),
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            for ip in inner {
                append_piece(&ip.piece, true, raw, text, unknown);
            }
        }
        WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_)
        | WordPiece::ArithmeticExpression(_) => {
            let splits = !quoted || raw.contains('@') || raw.contains('*');
            *unknown = Some(unknown.unwrap_or(false) || splits);
        }
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
    fn brush_parses_compound_commands() {
        let first = |line: &str| match &parse_program(line).complete_commands[0].0[0].0.first.seq[0] {
            ast::Command::Compound(c, _) => std::mem::discriminant(c),
            _ => panic!("{line}: expected a compound command"),
        };
        assert_ne!(first("(ls)"), first("{ ls; }"));
        assert_ne!(first("if true; then echo yes; fi"), first("for x in a b; do echo $x; done"));
    }

    #[test]
    fn brush_keeps_a_quoted_pipe_in_one_word() {
        let pipeline = &parse_program("echo 'hello | world'").complete_commands[0].0[0].0.first;
        assert_eq!(pipeline.seq.len(), 1);
    }

    #[test]
    fn brush_rejects_unclosed_quote() {
        assert!(parse_with_brush("echo 'hello").is_err());
    }

    #[test]
    fn brush_finds_brace_expressions_outside_quotes_only() {
        let options = brush_parser::ParserOptions::default();
        let has_expr = |w: &str| {
            brush_parser::word::parse_brace_expansions(w, &options)
                .unwrap()
                .is_some_and(|p| p.iter().any(|p| matches!(p, BraceExpressionOrText::Expr(_))))
        };
        assert!(has_expr("{a,b}"));
        assert!(has_expr("x{1..3}"));
        assert!(!has_expr("'{a,b}'"));
        assert!(!has_expr("{}"));
        assert!(!has_expr("@{u}"));
    }

    // --- Words ---

    #[test]
    fn a_word_loses_its_quotes_and_an_expansion_is_unknown() {
        let known = |w: &str| Word::Known(w.to_string());
        let rows = [
            ("plain", known("plain")),
            ("'eval'", known("eval")),
            ("\"/etc/shadow\"", known("/etc/shadow")),
            ("a\\ b", known("a b")),
            ("src/*.rs", known("src/*.rs")),
            ("~/x", known("~/x")),
            ("~bob/x", known("~bob/x")),
            ("{}", known("{}")),
            ("@{u}..", known("@{u}..")),
            ("$X", Word::Unknown { splits: true }),
            ("\"$X\"", Word::Unknown { splits: false }),
            ("\"$@\"", Word::Unknown { splits: true }),
            ("a$(date)", Word::Unknown { splits: true }),
            ("{notes,/etc/shadow}", Word::Unknown { splits: true }),
        ];
        for (raw, want) in rows {
            assert_eq!(word(raw), want, "{raw}");
        }
    }

    // --- Commands of a line ---

    fn names(line: &str) -> Vec<String> {
        parse_command(line, &Grammars::default()).unwrap().into_iter().map(|c| c.name).collect()
    }

    #[test]
    fn a_line_holds_every_command_it_runs() {
        let rows: [(&str, &[&str]); 7] = [
            ("ls | grep foo | wc -l", &["ls", "grep", "wc"]),
            ("ls && echo done || echo failed", &["ls", "echo", "echo"]),
            ("echo 'hello | world'", &["echo"]),
            ("echo $(date)", &["echo", "date"]),
            ("(ls && pwd)", &["ls", "pwd"]),
            ("{ ls; pwd; }", &["ls", "pwd"]),
            ("time make -j4", &["make"]),
        ];
        for (line, want) in rows {
            assert_eq!(names(line), want, "{line}");
        }
        assert!(parse_command("", &Grammars::default()).unwrap().is_empty());
        assert!(parse_command("echo 'hello", &Grammars::default()).is_err());
    }

    #[test]
    fn a_redirect_target_loses_its_quotes() {
        let cmds = parse_command("echo hi > '/tmp/evil' 2>&1 < \"in\" >> $X", &Grammars::default()).unwrap();
        let got: Vec<_> = cmds[0].redirects.iter().map(|r| (r.target.as_str(), r.is_write, r.has_expansion)).collect();
        assert_eq!(got, [("/tmp/evil", true, false), ("in", false, false), ("$X", true, true)]);
    }
}
