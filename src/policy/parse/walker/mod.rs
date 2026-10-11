//! Walking brush's syntax tree: every command the line runs, in the context
//! of the shell that runs it, with its substitutions and *Inner command*s.
//! Each command's paths resolve against its context (`context`), and a
//! command of the line's own shell knows its place in the line (`place`).

mod context;
mod place;

use brush_parser::ast;
use brush_parser::word::{WordPiece, WordPieceWithSource};

use super::word::{constant, expanded};
use super::{
    CommandInfo, Context, Grammars, ParseError, RedirectInfo, Word, parse_with_brush, word,
};
use crate::policy::grammar::Inner;
use crate::policy::value::TriVal;
use context::{inner_context, join, resolved};
use place::{Joint, Place, separator};

struct Walker<'a> {
    grammars: &'a Grammars,
    /// The line, whose syntax tree places by character.
    line: &'a str,
    /// The byte offset of each character of `line`, and of its end.
    bytes: Vec<usize>,
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

/// The commands that `program`, the parse of `line`, runs, each parsed by
/// its grammar, the first in `start`.
pub(super) fn walk(
    line: &str,
    program: &ast::Program,
    grammars: &Grammars,
    start: &Context,
) -> Result<Vec<CommandInfo>, ParseError> {
    let bytes = line
        .char_indices()
        .map(|(b, _)| b)
        .chain([line.len()])
        .collect();
    let mut walker = Walker {
        grammars,
        line,
        bytes,
        out: Vec::new(),
        context: start.clone(),
        moved: false,
        error: None,
    };
    walker.top(program);
    match walker.error {
        Some(err) => Err(err),
        None => Ok(walker.out),
    }
}

impl Walker<'_> {
    /// The line's own shell: its list items, across its lines, in order. A
    /// newline joins them as `;` does.
    fn top(&mut self, program: &ast::Program) {
        let items: Vec<&ast::CompoundListItem> = program
            .complete_commands
            .iter()
            .flat_map(|cc| &cc.0)
            .collect();
        let ops: Vec<_> = items.iter().map(|item| separator(&item.1)).collect();
        let extents: Vec<_> = items
            .iter()
            .map(|item| self.and_or_extent(&item.0))
            .collect();
        let line = Joint::line(ops.last().copied().filter(|op| *op == "&"));
        for (k, item) in items.iter().enumerate() {
            self.and_or_list(&item.0, Some(line.of_part(k, &ops, &extents)));
        }
    }

    fn program(&mut self, program: &ast::Program) {
        for cc in &program.complete_commands {
            self.compound_list(cc);
        }
    }

    fn compound_list(&mut self, list: &ast::CompoundList) {
        for item in &list.0 {
            self.and_or_list(&item.0, None);
        }
    }

    /// After `||`, and once the list ends (`;`, `&`, a newline), a cd in it
    /// may have failed, so the cwd is Unknown. `joint` places a list of the
    /// line's own shell.
    fn and_or_list(&mut self, and_or: &ast::AndOrList, joint: Option<Joint>) {
        let outer = std::mem::replace(&mut self.moved, false);
        let mut pipelines = vec![&and_or.first];
        let mut ops = Vec::new();
        for additional in &and_or.additional {
            let (op, pipeline) = match additional {
                ast::AndOr::And(p) => ("&&", p),
                ast::AndOr::Or(p) => ("||", p),
            };
            ops.push(op);
            pipelines.push(pipeline);
        }
        let extents: Vec<_> = pipelines.iter().map(|p| self.pipeline_extent(p)).collect();
        for (j, pipeline) in pipelines.iter().enumerate() {
            if j > 0 && ops[j - 1] == "||" {
                self.leave_chain();
            }
            self.pipeline(pipeline, joint.map(|j_| j_.of_part(j, &ops, &extents)));
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
    fn pipeline(&mut self, pipeline: &ast::Pipeline, joint: Option<Joint>) {
        let stages = pipeline.seq.len();
        let ops = vec!["|"; stages.saturating_sub(1)];
        let extents: Vec<_> = pipeline
            .seq
            .iter()
            .map(|c| self.command_extent(c))
            .collect();
        for (i, command) in pipeline.seq.iter().enumerate() {
            let joint = joint.map(|outer| outer.of_part(i, &ops, &extents));
            if stages > 1 {
                self.subshell(|w| w.command(command, joint));
            } else {
                self.command(command, joint);
            }
        }
    }

    fn command(&mut self, command: &ast::Command, joint: Option<Joint>) {
        match command {
            ast::Command::Simple(simple) => self.simple_command(simple, joint),
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
    fn simple_command(&mut self, simple: &ast::SimpleCommand, joint: Option<Joint>) {
        let prefix = || simple.prefix.iter().flat_map(|p| &p.0);
        let suffix = || simple.suffix.iter().flat_map(|s| &s.0);
        let home = self.context.home.clone();
        let mut variables = Vec::new();
        let mut words = Vec::new();
        let mut constant_words = true;
        let mut raws: Vec<&str> = simple
            .word_or_name
            .iter()
            .map(|n| n.value.as_str())
            .collect();
        for item in prefix() {
            match item {
                ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, w) => {
                    variables.push(TriVal::String(variable_name(&assignment.name)));
                    raws.push(&w.value);
                }
                ast::CommandPrefixOrSuffixItem::Word(w) => raws.push(&w.value),
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, s) => {
                    self.subshell(|w| w.compound_list(&s.list))
                }
                ast::CommandPrefixOrSuffixItem::IoRedirect(r) => self.redirect_substitutions(r),
            }
        }
        for item in suffix() {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(w)
                | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
                    words.push(word(&w.value, &home));
                    constant_words &= constant(&w.value);
                    raws.push(&w.value);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, s) => {
                    words.push(Word::Unknown { splits: false });
                    constant_words = false;
                    self.subshell(|w| w.compound_list(&s.list));
                }
                ast::CommandPrefixOrSuffixItem::IoRedirect(r) => self.redirect_substitutions(r),
            }
        }
        let mut expands: Vec<String> = Vec::new();
        for raw in &raws {
            self.substitutions(raw);
            for name in expanded(raw) {
                if !expands.contains(&name) {
                    expands.push(name);
                }
            }
        }

        let place = Place {
            constant: constant_words,
            expands,
            ..self.place(simple, joint)
        };
        let name = simple.word_or_name.as_ref().map(|n| word(&n.value, &home));
        let redirects = self.redirects(prefix().chain(suffix()), place.top);
        let marked_cwd = self.run(name.clone(), &words, variables, redirects, place);
        if matches!(&name, Some(Word::Known(n)) if CHANGES_DIRECTORY.contains(&n.as_str())) {
            self.context.cwd = marked_cwd.unwrap_or(TriVal::Unknown);
            self.moved = true;
        }
    }

    /// One command in the shell's context, then its *Inner command*s in the
    /// context that its `with` gives them. Returns the cwd that the command's
    /// `marks.cwd` gives, if it gives one.
    fn run(
        &mut self,
        name: Option<Word>,
        words: &[Word],
        mut variables: Vec<TriVal>,
        redirects: Vec<RedirectInfo>,
        place: Place,
    ) -> Option<TriVal> {
        let context = self.context.clone();
        let parsed = match &name {
            Some(Word::Known(n)) => self.grammars.parse(n, words, &context.home),
            _ => self.grammars.parse("", &[], &context.home),
        };
        let exhaustive = match parsed.variables {
            TriVal::List {
                elements,
                exhaustive,
            } => {
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
            variables: TriVal::List {
                elements: variables,
                exhaustive,
            },
            context,
            text: place.text,
            top: place.top,
            op_before: place.op_before,
            op_after: place.op_after,
            constant: place.constant,
            expands: place.expands,
            cut: place.cut,
        });

        for inner in parsed.inner {
            let outer = std::mem::replace(&mut self.context, inner_context.clone());
            match inner {
                Inner::Words(words) => {
                    if let Some((name, rest)) = words.split_first() {
                        let text = words.iter().map(|w| match w {
                            Word::Known(w) => w.as_str(),
                            Word::Unknown { .. } => "…",
                        });
                        let place = Place::away(text.collect::<Vec<_>>().join(" "));
                        self.run(Some(name.clone()), rest, Vec::new(), Vec::new(), place);
                    }
                }
                Inner::Line(Some(line)) => self.line(&line),
                Inner::Line(None) => {
                    _ = self.run(
                        Some(Word::Unknown { splits: true }),
                        &[],
                        Vec::new(),
                        Vec::new(),
                        Place::away("…".into()),
                    )
                }
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
            ast::CompoundCommand::WhileClause(c) | ast::CompoundCommand::UntilClause(c) => self
                .repeated(|w| {
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
            ast::CompoundCommand::ArithmeticForClause(c) => {
                self.repeated(|w| w.compound_list(&c.body.list))
            }
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
            WordPiece::CommandSubstitution(text)
            | WordPiece::BackquotedCommandSubstitution(text) => {
                if let Ok(program) = parse_with_brush(text) {
                    self.subshell(|w| w.program(&program));
                }
            }
            WordPiece::DoubleQuotedSequence(inner)
            | WordPiece::GettextDoubleQuotedSequence(inner) => {
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
                ast::IoFileRedirectTarget::Filename(w)
                | ast::IoFileRedirectTarget::Duplicate(w) => {
                    self.substitutions(&w.value);
                }
                ast::IoFileRedirectTarget::ProcessSubstitution(_, s) => {
                    self.subshell(|w| w.compound_list(&s.list))
                }
                ast::IoFileRedirectTarget::Fd(_) => {}
            },
            ast::IoRedirect::HereString(_, w) | ast::IoRedirect::OutputAndError(w, _) => {
                self.substitutions(&w.value)
            }
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
        ast::AssignmentName::VariableName(n) | ast::AssignmentName::ArrayElementName(n, _) => {
            n.clone()
        }
    }
}
