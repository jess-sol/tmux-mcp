//! Walking brush's syntax tree: every command the line runs, in the context
//! of the shell that runs it, with its substitutions and *Inner command*s.

use brush_parser::ast;
use brush_parser::word::{WordPiece, WordPieceWithSource};

use super::{CommandInfo, Grammars, ParseError, RedirectInfo, Word, parse_with_brush, word};
use crate::policy::grammar::{Inner, With};
use crate::policy::value::TriVal;

struct Walker<'a> {
    grammars: &'a Grammars,
    out: Vec<CommandInfo>,
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

/// The commands that `program` runs, each parsed by its grammar.
pub(super) fn walk(program: &ast::Program, grammars: &Grammars) -> Result<Vec<CommandInfo>, ParseError> {
    let mut walker = Walker { grammars, out: Vec::new(), context: Context::pane(), error: None };
    walker.program(program);
    match walker.error {
        Some(err) => Err(err),
        None => Ok(walker.out),
    }
}

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
