//! Command parsing: brush-parser → flat list of CommandInfo.
//!
//! Parses a command string into a flat list of `CommandInfo` nodes, one per
//! command that will execute. Pipelines, logical operators, compound commands,
//! command substitutions and *Inner command*s produce additional independent
//! entries. Each command's words lose their quotes (`word`), and its grammar
//! parses them into parts as the walker reaches it (`walker`).

mod walker;
mod word;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use brush_parser::ast;

pub use super::grammar::Grammars;
use super::value::TriVal;
pub(crate) use word::word;

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
    walker::walk(&parse_with_brush(command)?, grammars)
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
