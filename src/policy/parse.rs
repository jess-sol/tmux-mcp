//! Command parsing: brush-parser → flat list of CommandInfo.
//!
//! Parses a command string into a flat list of `CommandInfo` nodes, one per
//! command that will execute. Pipelines, logical operators, compound commands,
//! command substitutions and *Inner command*s produce additional independent
//! entries. Each command's words lose their quotes, and its grammar parses
//! them into parts.

use std::collections::BTreeMap;

use brush_parser::ast;
use brush_parser::word::{BraceExpressionOrText, WordPiece, WordPieceWithSource};

pub use super::grammar::Grammars;
use super::grammar::{Inner, With};
use super::rules::TriVal;

// --- Types ---

/// Information about a single command that will execute.
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// The command name without its quotes: Unknown when the shell expands
    /// it, Null for a command that only assigns or redirects.
    pub name: TriVal,
    /// Shell redirects attached to this command.
    pub redirects: Vec<RedirectInfo>,
    /// The subcommand words below the command name: a list that ends in
    /// Unknown when a word cannot be known or is not one the grammar allows.
    pub subcommands: TriVal,
    /// The parts of every level that parsed, by name.
    pub args: BTreeMap<String, TriVal>,
    /// The directory the command runs in, relative to the pane's: Null when
    /// nothing changes it; Unknown when an earlier `cd` or an outer
    /// command's `with` changed it.
    pub cwd: TriVal,
    /// The words the grammar marks as read paths, relative to `cwd`.
    pub reads: TriVal,
    /// The words the grammar marks as write paths, relative to `cwd`.
    pub writes: TriVal,
    /// The names of the variables the command sets: its prefix assignments
    /// and its `marks.variables` words.
    pub variables: TriVal,
    /// The user the command runs as. Null: the pane's; Unknown: changed.
    pub user: TriVal,
    /// The host the command runs on. Null: the pane's; Unknown: changed.
    pub host: TriVal,
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

/// Parse a command string into the flat list of commands it runs. A shell
/// line that an *Inner command* runs must parse too.
pub fn parse_command(command: &str, grammars: &Grammars) -> Result<Vec<CommandInfo>, ParseError> {
    let program = parse_with_brush(command)?;
    let mut commands = Vec::new();
    let mut walker = Walker { grammars, out: &mut commands, context: Context::pane(), error: None };
    walker.program(&program);
    match walker.error {
        Some(err) => Err(err),
        None => Ok(commands),
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
    /// The context of the shell being walked.
    context: Context,
    /// The first shell line of an *Inner command* that did not parse.
    error: Option<ParseError>,
}

/// The context a command runs in, against the pane's: a field is Null while
/// it is the pane's, and Unknown once a command changed it.
#[derive(Clone)]
struct Context {
    cwd: TriVal,
    user: TriVal,
    host: TriVal,
}

/// The shell builtins that change the directory of the rest of their shell.
const CHANGES_DIRECTORY: [&str; 3] = ["cd", "pushd", "popd"];

impl Context {
    fn pane() -> Context {
        Context { cwd: TriVal::Null, user: TriVal::Null, host: TriVal::Null }
    }

    /// The context of the *Inner command*s of a command whose levels set `with`.
    fn with(&self, with: With) -> Context {
        let change = |set: bool, field: &TriVal| if set { TriVal::Unknown } else { field.clone() };
        Context { cwd: change(with.cwd, &self.cwd), user: change(with.user, &self.user), host: change(with.host, &self.host) }
    }
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

    /// Each stage of a pipeline of several runs in a subshell.
    fn pipeline(&mut self, pipeline: &ast::Pipeline) {
        let subshells = pipeline.seq.len() > 1;
        for command in &pipeline.seq {
            if subshells {
                self.subshell(|w| w.command(command));
            } else {
                self.command(command);
            }
        }
    }

    fn command(&mut self, command: &ast::Command) {
        match command {
            ast::Command::Simple(simple) => self.simple_command(simple),
            ast::Command::Compound(compound, redirects) => {
                self.compound_command(compound);
                for redirect in redirects.iter().flat_map(|r| &r.0) {
                    self.redirect_substitutions(redirect);
                }
            }
            ast::Command::Function(_) | ast::Command::ExtendedTest(_) => {}
        }
    }

    /// A simple command, the commands of its substitutions, and its
    /// *Inner command*s. Words after the name that brush reads as
    /// assignments are plain words to the command.
    fn simple_command(&mut self, simple: &ast::SimpleCommand) {
        let prefix = || simple.prefix.iter().flat_map(|p| &p.0);
        let suffix = || simple.suffix.iter().flat_map(|s| &s.0);
        let mut variables = Vec::new();
        let mut words = Vec::new();
        let mut raws: Vec<&str> = simple.word_or_name.iter().map(|n| n.value.as_str()).collect();
        for item in prefix() {
            match item {
                ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, w) => {
                    variables.push(TriVal::String(variable_name(&assignment.name)));
                    raws.push(&w.value);
                }
                ast::CommandPrefixOrSuffixItem::Word(w) => raws.push(&w.value),
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, s) => self.subshell(|w| w.compound_list(&s.list)),
                ast::CommandPrefixOrSuffixItem::IoRedirect(r) => self.redirect_substitutions(r),
            }
        }
        for item in suffix() {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(w) | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
                    words.push(word(&w.value));
                    raws.push(&w.value);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, s) => {
                    words.push(Word::Unknown { splits: false });
                    self.subshell(|w| w.compound_list(&s.list));
                }
                ast::CommandPrefixOrSuffixItem::IoRedirect(r) => self.redirect_substitutions(r),
            }
        }
        for raw in raws {
            self.substitutions(raw);
        }

        let name = simple.word_or_name.as_ref().map(|n| word(&n.value));
        self.run(name.clone(), &words, variables, redirects(prefix().chain(suffix())));
        if matches!(&name, Some(Word::Known(n)) if CHANGES_DIRECTORY.contains(&n.as_str())) {
            self.context.cwd = TriVal::Unknown;
        }
    }

    /// One command in the shell's context, then its *Inner command*s in the
    /// context that its `with` gives them.
    fn run(&mut self, name: Option<Word>, words: &[Word], mut variables: Vec<TriVal>, redirects: Vec<RedirectInfo>) {
        let parsed = match &name {
            Some(Word::Known(n)) => self.grammars.parse(n, words),
            _ => self.grammars.parse("", &[]),
        };
        let exhaustive = match parsed.variables {
            TriVal::List { elements, exhaustive } => {
                variables.extend(elements);
                exhaustive
            }
            _ => false,
        };
        let cwd = if self.context.cwd == TriVal::Unknown { TriVal::Unknown } else { parsed.cwd };
        self.out.push(CommandInfo {
            name: match &name {
                None => TriVal::Null,
                Some(Word::Known(n)) => TriVal::String(n.clone()),
                Some(Word::Unknown { .. }) => TriVal::Unknown,
            },
            redirects,
            subcommands: parsed.subcommands,
            args: parsed.args,
            cwd,
            reads: parsed.reads,
            writes: parsed.writes,
            variables: TriVal::List { elements: variables, exhaustive },
            user: self.context.user.clone(),
            host: self.context.host.clone(),
        });

        let context = self.context.with(parsed.with);
        for inner in parsed.inner {
            let outer = std::mem::replace(&mut self.context, context.clone());
            match inner {
                Inner::Words(words) => {
                    if let Some((name, rest)) = words.split_first() {
                        self.run(Some(name.clone()), rest, Vec::new(), Vec::new());
                    }
                }
                Inner::Line(Some(line)) => self.line(&line),
                Inner::Line(None) => self.run(Some(Word::Unknown { splits: true }), &[], Vec::new(), Vec::new()),
            }
            self.context = outer;
        }
    }

    /// A shell line that an *Inner command* runs.
    fn line(&mut self, text: &str) {
        match parse_with_brush(text) {
            Ok(program) => self.program(&program),
            Err(err) => _ = self.error.get_or_insert(err),
        }
    }

    /// Walks `f` in a shell of its own: what a `cd` there changes stays there.
    fn subshell(&mut self, f: impl FnOnce(&mut Self)) {
        let outer = self.context.clone();
        f(self);
        self.context = outer;
    }

    fn compound_command(&mut self, compound: &ast::CompoundCommand) {
        match compound {
            ast::CompoundCommand::Subshell(s) => self.subshell(|w| w.compound_list(&s.list)),
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
            ast::CompoundCommand::ForClause(c) => {
                for value in c.values.iter().flatten() {
                    self.substitutions(&value.value);
                }
                self.compound_list(&c.body.list);
            }
            ast::CompoundCommand::CaseClause(c) => {
                self.substitutions(&c.value.value);
                for item in &c.cases {
                    for pattern in &item.patterns {
                        self.substitutions(&pattern.value);
                    }
                    if let Some(cmd) = &item.cmd {
                        self.compound_list(cmd);
                    }
                }
            }
            ast::CompoundCommand::ArithmeticForClause(c) => self.compound_list(&c.body.list),
            ast::CompoundCommand::Arithmetic(_) => {}
        }
    }

    /// The commands of the `$(…)` and backquote substitutions in a word,
    /// inside `${…}` and `$((…))` too. Each runs in a subshell.
    fn substitutions(&mut self, raw: &str) {
        let options = brush_parser::ParserOptions::default();
        if let Ok(pieces) = brush_parser::word::parse(raw, &options) {
            for p in &pieces {
                self.substitution_piece(p, raw);
            }
        }
    }

    fn substitution_piece(&mut self, piece: &WordPieceWithSource, raw: &str) {
        match &piece.piece {
            WordPiece::CommandSubstitution(text) | WordPiece::BackquotedCommandSubstitution(text) => {
                if let Ok(program) = parse_with_brush(text) {
                    self.subshell(|w| w.program(&program));
                }
            }
            WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
                for ip in inner {
                    self.substitution_piece(ip, raw);
                }
            }
            // The words of `${x:-…}`, `${x#…}` and the rest are inside its braces.
            WordPiece::ParameterExpansion(_) => {
                let source = raw.get(piece.start_index..piece.end_index);
                if let Some(inner) = source.and_then(|s| s.strip_prefix("${")?.strip_suffix('}')) {
                    self.substitutions(inner);
                }
            }
            WordPiece::ArithmeticExpression(expr) => self.substitutions(&expr.value),
            _ => {}
        }
    }

    /// The commands of a redirect: substitutions in its target, a process
    /// substitution, or an expanded here-document.
    fn redirect_substitutions(&mut self, redirect: &ast::IoRedirect) {
        match redirect {
            ast::IoRedirect::File(_, _, target) => match target {
                ast::IoFileRedirectTarget::Filename(w) | ast::IoFileRedirectTarget::Duplicate(w) => {
                    self.substitutions(&w.value);
                }
                ast::IoFileRedirectTarget::ProcessSubstitution(_, s) => self.subshell(|w| w.compound_list(&s.list)),
                ast::IoFileRedirectTarget::Fd(_) => {}
            },
            ast::IoRedirect::HereString(_, w) | ast::IoRedirect::OutputAndError(w, _) => self.substitutions(&w.value),
            ast::IoRedirect::HereDocument(_, doc) if doc.requires_expansion => {
                self.substitutions(&here_document_word(&doc.doc.value));
            }
            ast::IoRedirect::HereDocument(..) => {}
        }
    }
}

/// A here-document body expands as a double-quoted word in which `"` is
/// plain text. Every backslash is kept, so an escaped `$(` still counts.
fn here_document_word(body: &str) -> String {
    format!("\"{}\"", body.replace('\\', "\\\\").replace('"', "\\\""))
}

fn variable_name(name: &ast::AssignmentName) -> String {
    match name {
        ast::AssignmentName::VariableName(n) | ast::AssignmentName::ArrayElementName(n, _) => n.clone(),
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
        let (target, has_expansion) = match word(&target.value) {
            Word::Known(text) => (text, false),
            Word::Unknown { .. } => (target.value.clone(), true),
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

    fn simple(line: &str) -> ast::SimpleCommand {
        match &parse_program(line).complete_commands[0].0[0].0.first.seq[0] {
            ast::Command::Simple(s) => s.clone(),
            _ => panic!("{line}: expected a simple command"),
        }
    }

    #[test]
    fn brush_reads_assignments_as_their_own_items() {
        let kinds = |line: &str| {
            let s = simple(line);
            let items = s.prefix.iter().flat_map(|p| &p.0).chain(s.suffix.iter().flat_map(|p| &p.0));
            items.map(|i| matches!(i, ast::CommandPrefixOrSuffixItem::AssignmentWord(..))).collect::<Vec<_>>()
        };
        assert_eq!(kinds("X=1 Y=2 ls a"), [true, true, false]);
        // After the name too, though only the command gives it a meaning.
        assert_eq!(kinds("export X=1 Y"), [true, false]);
        assert_eq!(kinds("env X=1 ls a=b"), [true, false, true]);
        assert!(simple("X=1").word_or_name.is_none());
    }

    #[test]
    fn brush_gives_each_word_piece_its_source_span() {
        let options = brush_parser::ParserOptions::default();
        let raw = "a${x:-$(eval true)}\"$((1 + $(id)))\"";
        let pieces = brush_parser::word::parse(raw, &options).unwrap();
        assert_eq!(&raw[pieces[1].start_index..pieces[1].end_index], "${x:-$(eval true)}");
        let WordPiece::DoubleQuotedSequence(inner) = &pieces[2].piece else { panic!("{:?}", pieces[2]) };
        assert!(matches!(&inner[0].piece, WordPiece::ArithmeticExpression(e) if e.value.contains("$(id)")));
        let raw = "\"a${x:-$(id)}\"";
        let pieces = brush_parser::word::parse(raw, &options).unwrap();
        let WordPiece::DoubleQuotedSequence(inner) = &pieces[0].piece else { panic!("{:?}", pieces[0]) };
        assert_eq!(&raw[inner[1].start_index..inner[1].end_index], "${x:-$(id)}");
    }

    #[test]
    fn brush_keeps_a_here_document_body_as_a_word() {
        let s = simple("cat <<EOF\n$(id)\nEOF\n");
        let docs: Vec<_> = s
            .suffix
            .iter()
            .flat_map(|p| &p.0)
            .filter_map(|i| match i {
                ast::CommandPrefixOrSuffixItem::IoRedirect(ast::IoRedirect::HereDocument(_, d)) => Some(d.clone()),
                _ => None,
            })
            .collect();
        assert!(docs[0].requires_expansion && docs[0].doc.value.contains("$(id)"));
        let quoted = simple("cat <<'EOF'\n$(id)\nEOF\n");
        assert!(quoted.suffix.iter().flat_map(|p| &p.0).any(|i| matches!(
            i,
            ast::CommandPrefixOrSuffixItem::IoRedirect(ast::IoRedirect::HereDocument(_, d)) if !d.requires_expansion
        )));
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

    /// Each command's name, sorted: `-` for none, `?` for Unknown.
    fn names(line: &str) -> Vec<String> {
        let commands = parse_command(line, &Grammars::default()).unwrap_or_else(|e| panic!("{line}: {}", e.message));
        let mut names: Vec<String> = commands
            .into_iter()
            .map(|c| match c.name {
                TriVal::String(n) => n,
                TriVal::Null => "-".into(),
                _ => "?".into(),
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_line_holds_every_command_it_runs() {
        let rows: &[(&str, &[&str])] = &[
            ("ls | grep foo | wc -l", &["grep", "ls", "wc"]),
            ("ls && echo done || echo failed", &["echo", "echo", "ls"]),
            ("echo 'hello | world'", &["echo"]),
            ("echo $(date)", &["date", "echo"]),
            ("(ls && pwd)", &["ls", "pwd"]),
            ("{ ls; pwd; }", &["ls", "pwd"]),
            ("time make -j4", &["make"]),
            ("X=1", &["-"]),
            ("> out", &["-"]),
            ("$X a", &["?"]),
            // Every word where a substitution can stand
            ("FOO=$(id) ls", &["id", "ls"]),
            ("echo ${x:-$(id)} \"${y#`date`}\"", &["date", "echo", "id"]),
            ("echo $((1 + $(id)))", &["echo", "id"]),
            ("echo hi > $(id)", &["echo", "id"]),
            ("{ echo; } > $(id)", &["echo", "id"]),
            ("cat <(id)", &["cat", "id"]),
            ("for i in $(id); do echo; done", &["echo", "id"]),
            ("case $(id) in $(date)) echo;; esac", &["date", "echo", "id"]),
            ("cat <<EOF\na \"$(id)\" \\$(date)\nEOF\n", &["cat", "date", "id"]),
            ("cat <<'EOF'\n$(id)\nEOF\n", &["cat"]),
            ("cat <<< $(id)", &["cat", "id"]),
        ];
        for (line, want) in rows {
            assert_eq!(names(line), *want, "{line}");
        }
        assert!(parse_command("", &Grammars::default()).unwrap().is_empty());
        assert!(parse_command("echo 'hello", &Grammars::default()).is_err());
    }

    #[test]
    fn a_prefix_assignment_is_a_variable_of_its_command() {
        let cmds = parse_command("A=1 B[0]=2 env C=3 ls", &Grammars::default()).unwrap();
        let strings = |vals: &[&str]| TriVal::List {
            elements: vals.iter().map(|v| TriVal::String(v.to_string())).collect(),
            exhaustive: true,
        };
        assert_eq!(cmds[0].variables, strings(&["A", "B"]));
    }

    /// `cd` changes the directory of the commands after it in its shell;
    /// a subshell, a substitution and a pipeline stage are shells of their own.
    #[test]
    fn a_cd_leaves_the_directory_of_the_rest_of_its_shell_unknown() {
        let rows = [
            ("cd x && cat y", TriVal::Unknown),
            ("pushd x; cat y", TriVal::Unknown),
            ("{ cd x; } && cat y", TriVal::Unknown),
            ("if true; then cd x; fi; cat y", TriVal::Unknown),
            ("(cd x) && cat y", TriVal::Null),
            ("cd x | cat y", TriVal::Null),
            ("echo $(cd x) && cat y", TriVal::Null),
            ("cat y", TriVal::Null),
        ];
        for (line, want) in rows {
            let cmds = parse_command(line, &Grammars::default()).unwrap();
            let cat = cmds.iter().find(|c| c.name == TriVal::String("cat".into())).unwrap();
            assert_eq!(cat.cwd, want, "{line}");
        }
        let cmds = parse_command("cd $(pwd)", &Grammars::default()).unwrap();
        assert!(cmds.iter().all(|c| c.cwd == TriVal::Null));
    }

    #[test]
    fn a_redirect_target_loses_its_quotes() {
        let cmds = parse_command("echo hi > '/tmp/evil' 2>&1 < \"in\" >> $X", &Grammars::default()).unwrap();
        let got: Vec<_> = cmds[0].redirects.iter().map(|r| (r.target.as_str(), r.is_write, r.has_expansion)).collect();
        assert_eq!(got, [("/tmp/evil", true, false), ("in", false, false), ("$X", true, true)]);
    }
}
