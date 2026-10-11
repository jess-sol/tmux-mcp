//! The parsers: how one level reads its option words, and which words it
//! takes as operands. Each parser is a module of its own.

mod find;
mod gnu;
mod posix;

use super::Opt;
use super::run::{Read, Run};
use crate::policy::config::{ParserConfig, ParserName};
use crate::policy::parse::Word;

/// A level's parser, with the settings that copy one tool.
#[derive(Clone)]
pub(super) struct Parser {
    pub(super) name: ParserName,
    /// `head -5`: a first-argument number is a value of this option.
    leading_number: Option<String>,
    /// `git log -5`: a number anywhere is a value of this option.
    number: Option<String>,
    /// pflag tools read `-e=x` as the value `x`.
    short_equals: bool,
}

/// Where one level's parse stands, for its parser.
#[derive(Default)]
pub(super) struct Scan {
    /// No later word is an option (`--`, or a `posix` operand).
    pub(super) options_end: bool,
    /// `find` is past its start points, in the expression.
    expression: bool,
}

impl Parser {
    pub(super) fn from(config: &ParserConfig) -> Parser {
        match config {
            ParserConfig::Name(name) => {
                Parser { name: *name, leading_number: None, number: None, short_equals: false }
            }
            ParserConfig::Settings(s) => Parser {
                name: s.name,
                leading_number: s.leading_number.clone(),
                number: s.number.clone(),
                short_equals: s.short_equals,
            },
        }
    }

    /// Whether the word `text` is read as an option.
    pub(super) fn is_option(&self, text: &str, at_start: bool, scan: &Scan) -> bool {
        if scan.options_end {
            return false;
        }
        match self.name {
            ParserName::Find => find::is_option(text),
            ParserName::Gnu | ParserName::Posix => gnu::is_option(self, text, at_start),
        }
    }

    /// Reads the option word `text`: matched exactly, never by prefix.
    pub(super) fn option(&self, text: &str, rest: &[Word], at_start: bool, active: &[&Opt], scan: &mut Scan, run: &mut Run) -> Read {
        match self.name {
            ParserName::Find => find::option(text, rest, active, scan, run),
            ParserName::Gnu | ParserName::Posix => gnu::option(self, text, rest, at_start, active, scan, run),
        }
    }

    /// A word that is not an option: true when it is an operand for the
    /// level's slots.
    pub(super) fn operand(&self, scan: &mut Scan) -> bool {
        match self.name {
            ParserName::Find => find::operand(scan),
            ParserName::Posix => posix::operand(scan),
            ParserName::Gnu => true,
        }
    }
}
