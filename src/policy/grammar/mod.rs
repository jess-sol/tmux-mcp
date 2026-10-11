//! Grammars: each `[commands.<name>]` table compiled once, then used to parse
//! the words of one command into named parts. A grammar never decides; rules
//! read what it parses.
//!
//! `compile` builds a grammar's levels, `run` parses words with them, its
//! parser in `parser` reading the option words, and `marks` turns the parts
//! into paths, inner commands and variables.

mod compile;
mod marks;
mod parser;
mod run;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use globset::GlobMatcher;

use super::config::GrammarConfig;
use super::parse::Word;
use super::value::TriVal;
use parser::Parser;
use run::Run;

// --- Types ---

/// The compiled grammars of the sources that apply, by command name.
#[derive(Default)]
pub struct Grammars {
    commands: BTreeMap<String, Level>,
}

/// What a grammar makes of one command's words.
pub(crate) struct Parse {
    pub subcommands: TriVal,
    pub args: BTreeMap<String, TriVal>,
    pub cwd: TriVal,
    pub reads: TriVal,
    pub writes: TriVal,
    /// The names of the variables that the marked words set.
    pub variables: TriVal,
    /// The *Inner command*s that the marked words run.
    pub inner: Vec<Inner>,
    /// The context fields that a `with` key changes for the inner commands.
    pub with: With,
}

/// An *Inner command* as the words of its outer command give it.
#[derive(Debug, PartialEq)]
pub(crate) enum Inner {
    /// Words already split; the first is the command name.
    Words(Vec<Word>),
    /// A shell line; None when a word of it cannot be known.
    Line(Option<String>),
}

/// The context fields that `with` keys set.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub(crate) struct With {
    pub cwd: bool,
    pub user: bool,
    pub host: bool,
}

/// A grammar's top level or one of its subcommand levels.
struct Level {
    parser: Parser,
    /// Operands in the order they take words; the subcommand position is one.
    slots: Vec<Slot>,
    /// Options that parse only before this level's subcommand word.
    options: Vec<Opt>,
    /// Options that parse at this level and every level below.
    global: Vec<Opt>,
    defaults: BTreeMap<String, Vec<String>>,
    marks: Marks,
    with: With,
    /// Subcommand words that parse the rest with this level.
    words: BTreeSet<String>,
    /// Subcommand words that parse the rest with a level of their own.
    tables: BTreeMap<String, Level>,
}

struct Slot {
    name: String,
    kind: SlotKind,
    matches: Option<GlobMatcher>,
    /// The options of its group: when any is given, the slot takes no word.
    group: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    One,
    List,
    Subcommand,
}

struct Opt {
    name: String,
    /// Long spellings without their `--`.
    long: Vec<String>,
    short: Vec<char>,
    /// The `find` parser's spellings: whole words, dash included.
    single: Vec<String>,
    value: bool,
    /// Its value is a command up to `;` or `{} +` (`-exec...`).
    command: bool,
}

#[derive(Default)]
struct Marks {
    read: Vec<String>,
    write: Vec<String>,
    cwd: Vec<String>,
    command: Vec<String>,
    shell: Vec<String>,
    variables: Vec<String>,
    /// Command arguments that the tool adds words to.
    appends: BTreeSet<String>,
}

const SUBCOMMAND: &str = "subcommand";

// --- Public API ---

impl Grammars {
    /// Compiles the grammars of each source in turn; a later source's grammar
    /// replaces an earlier one of the same name, with all its levels.
    pub(crate) fn compile<'a>(
        sources: impl IntoIterator<Item = &'a BTreeMap<String, GrammarConfig>>,
    ) -> Result<Grammars, String> {
        let mut commands = BTreeMap::new();
        for source in sources {
            for (name, config) in source {
                commands.insert(name.clone(), Level::compile(&format!("commands.{name}"), config)?);
            }
        }
        Ok(Grammars { commands })
    }

    /// Parses a command's words with the grammar of its name, else of the
    /// last component of its path. A command without a grammar has no parts.
    pub(crate) fn parse(&self, name: &str, words: &[Word]) -> Parse {
        let Some(level) = self.lookup(name) else {
            return Parse {
                subcommands: list(Vec::new(), true),
                args: BTreeMap::new(),
                cwd: TriVal::Null,
                reads: list(Vec::new(), true),
                writes: list(Vec::new(), true),
                variables: list(Vec::new(), true),
                inner: Vec::new(),
                with: With::default(),
            };
        };
        let mut run = Run::default();
        level.parse(&[], words, &mut run);
        run.finish()
    }

    /// The names that the levels along `command` declare, for a rule's
    /// `command` key: `["git", "stash"]` gives git's names and stash's.
    pub(crate) fn declared(&self, command: &[String]) -> Result<BTreeSet<String>, String> {
        let shown = command.join(" ");
        let Some(mut level) = self.lookup(&command[0]) else {
            if command.len() > 1 {
                return Err(format!("`{}` has no grammar, so `{shown}` names no subcommand", command[0]));
            }
            return Ok(BTreeSet::new());
        };
        let mut names = level.names();
        let mut past_words = false;
        for word in &command[1..] {
            if let (false, Some(table)) = (past_words, level.tables.get(word)) {
                level = table;
                names.extend(level.names());
            } else if !past_words && level.words.contains(word) {
                past_words = true;
            } else {
                return Err(format!("`{shown}`: `{word}` is not a subcommand that its grammar allows"));
            }
        }
        Ok(names)
    }

    fn lookup(&self, name: &str) -> Option<&Level> {
        self.commands.get(name).or_else(|| self.commands.get(name.rsplit('/').next()?))
    }
}

// --- Internal ---

impl Level {
    /// The names this level declares, its global options included.
    fn names(&self) -> BTreeSet<String> {
        let slots = self.slots.iter().filter(|s| s.kind != SlotKind::Subcommand).map(|s| &s.name);
        let options = self.options.iter().chain(&self.global).map(|o| &o.name);
        slots.chain(options).cloned().collect()
    }
}

fn value_of(word: &Word) -> TriVal {
    match word {
        Word::Known(w) => TriVal::String(w.clone()),
        Word::Unknown { .. } => TriVal::Unknown,
    }
}

fn list(elements: Vec<TriVal>, exhaustive: bool) -> TriVal {
    TriVal::List { elements, exhaustive }
}
