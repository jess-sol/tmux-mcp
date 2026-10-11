//! After the parse: defaults fill absent arguments, and the marks turn the
//! parts into paths, a directory, *Inner command*s and variables.

use super::run::Run;
use super::{Inner, Parse, With, list};
use crate::policy::parse::Word;
use crate::policy::value::TriVal;

/// A list being gathered from several parts.
struct Gather {
    elements: Vec<TriVal>,
    exhaustive: bool,
}

impl Run<'_> {
    pub(super) fn finish(mut self) -> Parse {
        for level in &self.levels {
            for (name, default) in &level.defaults {
                apply_default(self.parts.get_mut(name), default);
            }
        }

        let exhaustive = !self.unsure && !self.open;
        let mut reads = Gather::new(exhaustive);
        let mut writes = Gather::new(exhaustive);
        let mut cwd_values = Vec::new();
        let mut has_cwd = false;
        for level in &self.levels {
            for name in &level.marks.read {
                reads.add(self.parts.get(name));
            }
            for name in &level.marks.write {
                writes.add(self.parts.get(name));
            }
            for name in &level.marks.cwd {
                has_cwd = true;
                cwd_values.extend(self.parts.get(name).cloned());
            }
        }
        let cwd = if has_cwd { compose_cwd(&cwd_values) } else { TriVal::Null };
        match &cwd {
            TriVal::String(_) => reads.elements.push(TriVal::String(".".into())),
            TriVal::Unknown => reads.elements.push(TriVal::Unknown),
            _ => {}
        }

        let mut variables = Gather::new(true);
        let mut inner = Vec::new();
        let mut with = With::default();
        for level in &self.levels {
            for name in &level.marks.variables {
                variables.add_names(self.parts.get(name));
            }
            for name in &level.marks.command {
                commands(self.parts.get(name), level.marks.appends.contains(name), &mut inner);
            }
            for name in &level.marks.shell {
                inner.extend(line(self.parts.get(name)));
            }
            with.cwd |= level.with.cwd;
            with.user |= level.with.user;
            with.host |= level.with.host;
        }

        Parse {
            subcommands: list(self.subcommands, true),
            args: self.parts,
            cwd,
            reads: list(reads.elements, reads.exhaustive),
            writes: list(writes.elements, writes.exhaustive),
            variables: list(variables.elements, variables.exhaustive),
            inner,
            with,
        }
    }
}

/// A default stands in for an absent argument; next to a non-exhaustive
/// empty list it may stand in, so the list stays non-exhaustive.
fn apply_default(part: Option<&mut TriVal>, default: &[String]) {
    let words = || default.iter().map(|w| TriVal::String(w.clone())).collect::<Vec<_>>();
    match part {
        Some(v @ TriVal::Null) => {
            if let Some(first) = default.first() {
                *v = TriVal::String(first.clone());
            }
        }
        Some(TriVal::List { elements, .. }) if elements.is_empty() => elements.extend(words()),
        _ => {}
    }
}

/// The directory that cwd-marked values lead to, each relative to the one
/// before; Null when none was given.
fn compose_cwd(values: &[TriVal]) -> TriVal {
    let mut dir: Option<String> = None;
    let mut step = |d: &str| {
        dir = Some(match &dir {
            Some(prev) if !d.starts_with('/') => format!("{prev}/{d}"),
            _ => d.to_string(),
        });
    };
    for value in values {
        match value {
            TriVal::String(d) => step(d),
            TriVal::List { elements, exhaustive: true } => {
                for e in elements {
                    match e {
                        TriVal::String(d) => step(d),
                        _ => return TriVal::Unknown,
                    }
                }
            }
            TriVal::Null => {}
            _ => return TriVal::Unknown,
        }
    }
    dir.map_or(TriVal::Null, TriVal::String)
}

/// The commands a `marks.command` part holds: its words, or one list of
/// words for each use of a `-exec...` option. Words that a part may hold
/// beyond those it shows make an Unknown word at the end of the command.
fn commands(part: Option<&TriVal>, appends: bool, out: &mut Vec<Inner>) {
    match part {
        Some(TriVal::List { elements, exhaustive }) if matches!(elements.first(), Some(TriVal::List { .. })) => {
            for use_ in elements {
                if let TriVal::List { elements, exhaustive } = use_ {
                    out.extend(command(elements, *exhaustive, appends));
                }
            }
            if !exhaustive {
                out.push(Inner::Words(vec![Word::Unknown { splits: true }]));
            }
        }
        Some(TriVal::List { elements, exhaustive }) => out.extend(command(elements, *exhaustive, appends)),
        Some(TriVal::Unknown) => out.push(Inner::Words(vec![Word::Unknown { splits: true }])),
        _ => {}
    }
}

/// A part's value lost whether an Unknown word could split, so each may.
fn command(elements: &[TriVal], exhaustive: bool, appends: bool) -> Option<Inner> {
    if elements.is_empty() && exhaustive {
        return None;
    }
    let mut words: Vec<Word> = elements
        .iter()
        .map(|e| match e {
            TriVal::String(w) => Word::Known(w.clone()),
            _ => Word::Unknown { splits: true },
        })
        .collect();
    if !exhaustive || appends {
        words.push(Word::Unknown { splits: true });
    }
    Some(Inner::Words(words))
}

/// The shell line a `marks.shell` part holds: its words joined with spaces.
fn line(part: Option<&TriVal>) -> Option<Inner> {
    match part {
        Some(TriVal::String(s)) => Some(Inner::Line(Some(s.clone()))),
        Some(TriVal::List { elements, exhaustive: true }) if elements.is_empty() => None,
        Some(TriVal::List { elements, exhaustive: true }) => {
            let words: Option<Vec<&str>> = elements
                .iter()
                .map(|e| match e {
                    TriVal::String(w) => Some(w.as_str()),
                    _ => None,
                })
                .collect();
            Some(Inner::Line(words.map(|w| w.join(" "))))
        }
        Some(TriVal::List { .. } | TriVal::Unknown) => Some(Inner::Line(None)),
        _ => None,
    }
}

impl Gather {
    fn new(exhaustive: bool) -> Gather {
        Gather { elements: Vec::new(), exhaustive }
    }

    fn add(&mut self, part: Option<&TriVal>) {
        match part {
            Some(TriVal::List { elements, exhaustive }) => {
                self.elements.extend(elements.iter().cloned());
                self.exhaustive &= exhaustive;
            }
            Some(v @ (TriVal::String(_) | TriVal::Unknown)) => self.elements.push(v.clone()),
            _ => {}
        }
    }

    /// The variable names of `NAME=value` and `NAME` words.
    fn add_names(&mut self, part: Option<&TriVal>) {
        let name = |v: &TriVal| match v {
            TriVal::String(w) => TriVal::String(w.split_once('=').map_or(w.as_str(), |(n, _)| n).to_string()),
            _ => TriVal::Unknown,
        };
        match part {
            Some(TriVal::List { elements, exhaustive }) => {
                self.elements.extend(elements.iter().map(name));
                self.exhaustive &= exhaustive;
            }
            Some(v @ TriVal::String(_)) => self.elements.push(name(v)),
            Some(TriVal::Unknown) => self.exhaustive = false,
            _ => {}
        }
    }
}
