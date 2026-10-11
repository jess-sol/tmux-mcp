//! A command's place in the line: the operators that join it to its
//! neighbours, its text, and the bytes that `drop` cuts. brush places words
//! by character; a redirect only by its target word.

use brush_parser::ast::{self, SourceLocation};

use super::Walker;
use super::context::as_dir;
use crate::policy::parse::{RedirectInfo, Word, word};
use crate::policy::rules::resolve;
use crate::policy::value::TriVal;

/// Where a part of the line's own shell stands: the operators that join it
/// to its neighbours, and the bytes where they end and start.
#[derive(Clone, Copy, Default)]
pub(super) struct Joint {
    before: Option<&'static str>,
    after: Option<&'static str>,
    prev_end: Option<usize>,
    next_start: Option<usize>,
}

/// A command's place in the line, as `CommandInfo` gives it.
pub(super) struct Place {
    pub(super) text: String,
    pub(super) top: bool,
    pub(super) op_before: Option<String>,
    pub(super) op_after: Option<String>,
    pub(super) constant: bool,
    pub(super) expands: Vec<String>,
    pub(super) cut: (usize, usize),
}

/// The operator that ends a list item: `;` or `&`.
pub(super) fn separator(op: &ast::SeparatorOperator) -> &'static str {
    match op {
        ast::SeparatorOperator::Async => "&",
        ast::SeparatorOperator::Sequence => ";",
    }
}

impl Joint {
    /// The line itself, which `after` ends: "&" when it runs in the background.
    pub(super) fn line(after: Option<&'static str>) -> Joint {
        Joint {
            after,
            ..Joint::default()
        }
    }

    /// The joint of part `i` of the parts inside `self`, with their extents;
    /// `ops[i]` joins part `i` to part `i + 1`.
    pub(super) fn of_part(
        self,
        i: usize,
        ops: &[&'static str],
        extents: &[Option<(usize, usize)>],
    ) -> Joint {
        let last = i + 1 == extents.len();
        Joint {
            before: if i == 0 {
                self.before
            } else {
                Some(ops[i - 1])
            },
            after: if last { self.after } else { Some(ops[i]) },
            prev_end: if i == 0 {
                self.prev_end
            } else {
                extents[i - 1].map(|(_, end)| end)
            },
            next_start: if last {
                self.next_start
            } else {
                extents[i + 1].map(|(start, _)| start)
            },
        }
    }
}

impl Place {
    /// A command outside the line's own shell: its text as `text` shows it.
    pub(super) fn away(text: String) -> Place {
        Place {
            text,
            top: false,
            op_before: None,
            op_after: None,
            constant: false,
            expands: Vec::new(),
            cut: (0, 0),
        }
    }
}

impl Walker<'_> {
    /// A simple command's place: in the line's own shell when `joint` places
    /// it there. Dropping it cuts the operator before it, or after it when
    /// it comes first.
    pub(super) fn place(&self, simple: &ast::SimpleCommand, joint: Option<Joint>) -> Place {
        let Some(joint) = joint else {
            return Place::away(simple.to_string());
        };
        let (start, end) = self.simple_extent(simple).unwrap_or((0, 0));
        let cut = match (joint.prev_end, joint.next_start) {
            (Some(prev_end), _) => (prev_end, end),
            (None, Some(next_start)) => (start, next_start),
            (None, None) => (start, end),
        };
        Place {
            text: self.line[start..end].to_string(),
            top: true,
            op_before: joint.before.map(String::from),
            op_after: joint.after.map(String::from),
            constant: false,
            expands: Vec::new(),
            cut,
        }
    }

    /// A command's redirects: the file each opens, resolved in the shell's
    /// context, and its text. A redirect of the line's own shell has its
    /// text from the line and the bytes that dropping it removes.
    pub(super) fn redirects<'i>(
        &self,
        items: impl Iterator<Item = &'i ast::CommandPrefixOrSuffixItem>,
        top: bool,
    ) -> Vec<RedirectInfo> {
        let mut out = Vec::new();
        for item in items {
            let ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) = item else {
                continue;
            };
            let (file, is_write) = match redirect {
                ast::IoRedirect::File(_, kind, ast::IoFileRedirectTarget::Filename(w)) => {
                    let is_write = matches!(
                        kind,
                        ast::IoFileRedirectKind::Write
                            | ast::IoFileRedirectKind::Append
                            | ast::IoFileRedirectKind::Clobber
                            | ast::IoFileRedirectKind::ReadAndWrite
                    );
                    (Some(w), is_write)
                }
                ast::IoRedirect::OutputAndError(w, _) => (Some(w), true),
                _ => (None, false),
            };
            let target = match file.map(|w| word(&w.value, &self.context.home)) {
                Some(Word::Known(text)) => resolve(&text, as_dir(&self.context.cwd)),
                Some(Word::Unknown { .. }) => TriVal::Unknown,
                None => TriVal::Null,
            };
            let (text, cut) = match self.redirect_extent(redirect).filter(|_| top) {
                Some((start, end)) => (self.line[start..end].to_string(), (start, end)),
                None => (redirect.to_string(), (0, 0)),
            };
            out.push(RedirectInfo {
                target,
                is_write,
                text,
                cut,
            });
        }
        out
    }

    pub(super) fn and_or_extent(&self, and_or: &ast::AndOrList) -> Option<(usize, usize)> {
        let first = self.pipeline_extent(&and_or.first)?;
        let last = match and_or.additional.last() {
            Some(ast::AndOr::And(p) | ast::AndOr::Or(p)) => self.pipeline_extent(p)?,
            None => first,
        };
        Some((first.0, last.1))
    }

    pub(super) fn pipeline_extent(&self, pipeline: &ast::Pipeline) -> Option<(usize, usize)> {
        let first = self.command_extent(pipeline.seq.first()?)?;
        let last = self.command_extent(pipeline.seq.last()?)?;
        Some((first.0, last.1))
    }

    pub(super) fn command_extent(&self, command: &ast::Command) -> Option<(usize, usize)> {
        match command {
            ast::Command::Simple(simple) => self.simple_extent(simple),
            other => self.bytes_of(other.location()),
        }
    }

    /// A simple command's bytes: its words, which brush places, and its
    /// redirects, which it does not.
    fn simple_extent(&self, simple: &ast::SimpleCommand) -> Option<(usize, usize)> {
        let items = simple
            .prefix
            .iter()
            .flat_map(|p| &p.0)
            .chain(simple.suffix.iter().flat_map(|s| &s.0));
        let redirects = items.filter_map(|item| match item {
            ast::CommandPrefixOrSuffixItem::IoRedirect(r) => self.redirect_extent(r),
            _ => None,
        });
        self.bytes_of(simple.location())
            .into_iter()
            .chain(redirects)
            .reduce(|a, b| (a.0.min(b.0), a.1.max(b.1)))
    }

    /// A redirect's bytes, from its fd and operator to the end of its target.
    /// brush places only the target word, so the start is found by reading
    /// back over the operator and the fd digits.
    fn redirect_extent(&self, redirect: &ast::IoRedirect) -> Option<(usize, usize)> {
        let target = match redirect {
            ast::IoRedirect::File(
                _,
                _,
                ast::IoFileRedirectTarget::Filename(w) | ast::IoFileRedirectTarget::Duplicate(w),
            )
            | ast::IoRedirect::HereString(_, w)
            | ast::IoRedirect::OutputAndError(w, _) => w,
            _ => return None,
        };
        let (start, end) = self.bytes_of(target.loc.clone())?;
        let head = self
            .line
            .get(..start)?
            .trim_end()
            .trim_end_matches(['<', '>', '&', '|']);
        Some((
            head.trim_end_matches(|c: char| c.is_ascii_digit()).len(),
            end,
        ))
    }

    /// The bytes of the line that a place in the syntax tree covers.
    fn bytes_of(&self, location: Option<brush_parser::TokenLocation>) -> Option<(usize, usize)> {
        let byte = |index: usize| self.bytes.get(index).copied().unwrap_or(self.line.len());
        location.map(|l| (byte(l.start.index), byte(l.end.index)))
    }
}
