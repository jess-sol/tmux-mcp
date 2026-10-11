//! Walking brush's syntax tree: every command the line runs, in the context
//! of the shell that runs it, with its substitutions and *Inner command*s.
//! Each command's paths resolve against its context.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use brush_parser::ast;
use brush_parser::word::{WordPiece, WordPieceWithSource};
use cel_parser::ast as cel;

use super::{CommandInfo, Context, Grammars, ParseError, RedirectInfo, Word, parse_with_brush, word};
use crate::policy::grammar::{Inner, With};
use crate::policy::rules::{eval_with, resolve};
use crate::policy::value::TriVal;

struct Walker<'a> {
    grammars: &'a Grammars,
    out: Vec<CommandInfo>,
    /// The context of the shell being walked.
    context: Context,
    /// A cd ran in the and-or list being walked. The cwd it set holds only
    /// for the commands that run if it succeeded: the rest of its `&&` chain.
    moved: bool,
    /// The first shell line of an *Inner command* that did not parse.
    error: Option<ParseError>,
}

/// The shell builtins that change the directory of the rest of their shell.
/// CDPATH is assumed unset: a line that sets it asks.
const CHANGES_DIRECTORY: [&str; 3] = ["cd", "pushd", "popd"];

/// The commands that `program` runs, each parsed by its grammar, the first
/// in `start`.
pub(super) fn walk(program: &ast::Program, grammars: &Grammars, start: &Context) -> Result<Vec<CommandInfo>, ParseError> {
    let mut walker = Walker { grammars, out: Vec::new(), context: start.clone(), moved: false, error: None };
    walker.program(program);
    match walker.error {
        Some(err) => Err(err),
        None => Ok(walker.out),
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

    /// After `||`, and once the list ends (`;`, `&`, a newline), a cd in it
    /// may have failed, so the cwd is Unknown.
    fn and_or_list(&mut self, and_or: &ast::AndOrList) {
        let outer = std::mem::replace(&mut self.moved, false);
        self.pipeline(&and_or.first);
        for additional in &and_or.additional {
            let pipeline = match additional {
                ast::AndOr::And(p) => p,
                ast::AndOr::Or(p) => {
                    self.leave_chain();
                    p
                }
            };
            self.pipeline(pipeline);
        }
        self.leave_chain();
        self.moved = outer;
    }

    fn leave_chain(&mut self) {
        if std::mem::take(&mut self.moved) {
            self.context.cwd = TriVal::Unknown;
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
            // The body runs wherever a later command calls it.
            ast::Command::Function(_) => self.context.cwd = TriVal::Unknown,
            ast::Command::ExtendedTest(_) => {}
        }
    }

    /// A simple command, the commands of its substitutions, and its
    /// *Inner command*s. Words after the name that brush reads as
    /// assignments are plain words to the command.
    fn simple_command(&mut self, simple: &ast::SimpleCommand) {
        let prefix = || simple.prefix.iter().flat_map(|p| &p.0);
        let suffix = || simple.suffix.iter().flat_map(|s| &s.0);
        let home = self.context.home.clone();
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
                    words.push(word(&w.value, &home));
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

        let name = simple.word_or_name.as_ref().map(|n| word(&n.value, &home));
        let redirects = redirects(prefix().chain(suffix()), &self.context);
        let marked_cwd = self.run(name.clone(), &words, variables, redirects);
        if matches!(&name, Some(Word::Known(n)) if CHANGES_DIRECTORY.contains(&n.as_str())) {
            self.context.cwd = marked_cwd.unwrap_or(TriVal::Unknown);
            self.moved = true;
        }
    }

    /// One command in the shell's context, then its *Inner command*s in the
    /// context that its `with` gives them. Returns the cwd that the command's
    /// `marks.cwd` gives, if it gives one.
    fn run(&mut self, name: Option<Word>, words: &[Word], mut variables: Vec<TriVal>, redirects: Vec<RedirectInfo>) -> Option<TriVal> {
        let context = self.context.clone();
        let parsed = match &name {
            Some(Word::Known(n)) => self.grammars.parse(n, words, &context.home),
            _ => self.grammars.parse("", &[], &context.home),
        };
        let exhaustive = match parsed.variables {
            TriVal::List { elements, exhaustive } => {
                variables.extend(elements);
                exhaustive
            }
            _ => false,
        };
        let marked = matches!(parsed.cwd, TriVal::String(_));
        let cwd = join(&context.cwd, &parsed.cwd);
        let inner_context = inner_context(&context, &cwd, &parsed.args, &parsed.with);
        self.out.push(CommandInfo {
            name: match &name {
                None => TriVal::Null,
                Some(Word::Known(n)) => TriVal::String(n.clone()),
                Some(Word::Unknown { .. }) => TriVal::Unknown,
            },
            redirects,
            subcommands: parsed.subcommands,
            args: parsed.args,
            reads: resolved(&parsed.reads, &cwd),
            writes: resolved(&parsed.writes, &cwd),
            cwd: cwd.clone(),
            variables: TriVal::List { elements: variables, exhaustive },
            context,
        });

        for inner in parsed.inner {
            let outer = std::mem::replace(&mut self.context, inner_context.clone());
            match inner {
                Inner::Words(words) => {
                    if let Some((name, rest)) = words.split_first() {
                        self.run(Some(name.clone()), rest, Vec::new(), Vec::new());
                    }
                }
                Inner::Line(Some(line)) => self.line(&line),
                Inner::Line(None) => _ = self.run(Some(Word::Unknown { splits: true }), &[], Vec::new(), Vec::new()),
            }
            self.context = outer;
        }
        marked.then_some(cwd)
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
        let outer = (self.context.clone(), self.moved);
        f(self);
        (self.context, self.moved) = outer;
    }

    /// A loop's body runs again in the context that one pass leaves.
    fn repeated(&mut self, f: impl Fn(&mut Self)) {
        let before = self.context.clone();
        f(self);
        if self.context != before {
            f(self);
        }
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
            ast::CompoundCommand::WhileClause(c) | ast::CompoundCommand::UntilClause(c) => self.repeated(|w| {
                w.compound_list(&c.0);
                w.compound_list(&c.1.list);
            }),
            ast::CompoundCommand::ForClause(c) => {
                for value in c.values.iter().flatten() {
                    self.substitutions(&value.value);
                }
                self.repeated(|w| w.compound_list(&c.body.list));
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
            ast::CompoundCommand::ArithmeticForClause(c) => self.repeated(|w| w.compound_list(&c.body.list)),
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

/// The context of a command's *Inner command*s: its own, with the fields
/// that its `with` keys set, evaluated in the order host, user, cwd. Each key
/// reads the command's args, its own cwd, user and host, and the fields
/// already evaluated. A string sets the field, null keeps it, and anything
/// else makes it Unknown. A host or user that changes leaves the home Unknown,
/// and a user that changes ends the shell's own user.
fn inner_context(outer: &Context, cwd: &TriVal, args: &BTreeMap<String, TriVal>, with: &With) -> Context {
    let args = TriVal::Map(args.clone().into_iter().collect());
    // The value a key gives its field; None keeps the field.
    let value = |key: &Option<cel::IdedExpr>, evaluated: &HashMap<String, TriVal>, home: &TriVal| {
        let vars = HashMap::from([
            ("args".to_string(), args.clone()),
            ("cwd".to_string(), cwd.clone()),
            ("user".to_string(), outer.user.clone()),
            ("host".to_string(), outer.host.clone()),
            ("with".to_string(), TriVal::Map(evaluated.clone())),
        ]);
        match eval_with(key.as_ref()?, vars, home.clone()) {
            TriVal::Null => None,
            v @ TriVal::String(_) => Some(v),
            _ => Some(TriVal::Unknown),
        }
    };
    let same = |new: &TriVal, old: &TriVal| matches!(new, TriVal::String(_)) && new == old;

    let mut inner = outer.clone();
    let mut evaluated = HashMap::new();
    if let Some(host) = value(&with.host, &evaluated, &inner.home) {
        if !same(&host, &inner.host) {
            inner.home = TriVal::Unknown;
        }
        inner.host = host;
    }
    if with.host.is_some() {
        evaluated.insert("host".to_string(), inner.host.clone());
    }
    if let Some(user) = value(&with.user, &evaluated, &inner.home) {
        if !same(&user, &inner.user) {
            inner.home = TriVal::Unknown;
            inner.own_user = false;
        }
        inner.user = user;
    }
    if with.user.is_some() {
        evaluated.insert("user".to_string(), inner.user.clone());
    }
    if let Some(dir) = value(&with.cwd, &evaluated, &inner.home) {
        inner.cwd = join(cwd, &dir);
    }
    inner
}

/// `dir` from the directory `base`: `base` itself when `dir` is Null.
fn join(base: &TriVal, dir: &TriVal) -> TriVal {
    match dir {
        TriVal::Null => base.clone(),
        TriVal::String(d) => resolve(d, as_dir(base)),
        _ => TriVal::Unknown,
    }
}

/// Each path of a list, from the directory `cwd`.
fn resolved(paths: &TriVal, cwd: &TriVal) -> TriVal {
    match paths {
        TriVal::List { elements, exhaustive } => {
            TriVal::List { elements: elements.iter().map(|p| join(cwd, p)).collect(), exhaustive: *exhaustive }
        }
        _ => TriVal::Unknown,
    }
}

fn as_dir(cwd: &TriVal) -> Option<&Path> {
    match cwd {
        TriVal::String(d) => Some(Path::new(d)),
        _ => None,
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

/// The redirects of a command, their targets resolved in the shell's context.
fn redirects<'a>(items: impl Iterator<Item = &'a ast::CommandPrefixOrSuffixItem>, context: &Context) -> Vec<RedirectInfo> {
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
        let target = match word(&target.value, &context.home) {
            Word::Known(text) => resolve(&text, as_dir(&context.cwd)),
            Word::Unknown { .. } => TriVal::Unknown,
        };
        out.push(RedirectInfo { target, is_write });
    }
    out
}
