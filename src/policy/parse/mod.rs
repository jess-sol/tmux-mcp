//! Command parsing: brush-parser → flat list of CommandInfo.
//!
//! Parses a command string into a flat list of `CommandInfo` nodes, one per
//! command that will execute. Pipelines, logical operators, compound commands,
//! command substitutions and *Inner command*s produce additional independent
//! entries. Each command's words lose their quotes (`word`), and its grammar
//! parses them into parts as the walker reaches it (`walker`). The walker
//! gives each command its context and resolves every path it touches.

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
    /// The directory the command runs in: its context's, with its
    /// `marks.cwd` applied. Absolute or Unknown.
    pub cwd: TriVal,
    /// The paths the grammar marks as read, resolved: absolute or Unknown.
    pub reads: TriVal,
    /// The paths the grammar marks as written, resolved: absolute or Unknown.
    pub writes: TriVal,
    /// The names of the variables the command sets: its prefix assignments
    /// and its `marks.variables` words.
    pub variables: TriVal,
    /// The context of the shell that runs the command.
    pub context: Context,
    /// The command as written: from the line for a command that is `top`.
    pub text: String,
    /// It runs in the line's own shell, as a stage of one of its pipelines,
    /// outside any compound command, substitution or *Inner command*.
    pub top: bool,
    /// The operators that join it to the commands before and after it:
    /// "|", "&&", "||", ";" or "&"; None at the ends of the line, and for a
    /// command that is not `top`.
    pub op_before: Option<String>,
    pub op_after: Option<String>,
    /// Every word after its name is constant text.
    pub constant: bool,
    /// The names of the parameters its words expand ("?" for `$?`).
    pub expands: Vec<String>,
    /// The bytes of the line that dropping it removes: the command and the
    /// operator that joins it to the command before it, or after it when
    /// it comes first.
    pub cut: (usize, usize),
}

/// Where a command runs. A line starts from the context that OSC 7 reported.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    /// Absolute or Unknown.
    pub cwd: TriVal,
    /// A host name or Unknown.
    pub host: TriVal,
    /// A user name or Unknown.
    pub user: TriVal,
    /// Absolute or Unknown; only tilde expansion and `home()` read it.
    pub home: TriVal,
    /// It runs as the shell's own user, whose name may be Unknown.
    pub own_user: bool,
}

/// A shell redirect (>, >>, <, 2>&1, etc.) attached to a command.
#[derive(Debug, Clone)]
pub struct RedirectInfo {
    /// The file it opens, resolved against the shell's directory: absolute
    /// or Unknown; null for a redirect that opens no file (`2>&1`, `<<EOF`).
    pub target: TriVal,
    /// True for write redirects (>, >>, &>, >|). False for read (<).
    pub is_write: bool,
    /// The redirect as written.
    pub text: String,
    /// The bytes of the line that dropping it removes.
    pub cut: (usize, usize),
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

/// Parse a command string into the flat list of commands it runs, the first
/// in `start`. A shell line that an *Inner command* runs must parse too.
pub fn parse_command(command: &str, grammars: &Grammars, start: &Context) -> Result<Vec<CommandInfo>, ParseError> {
    walker::walk(command, &parse_with_brush(command)?, grammars, start)
}

/// `line` without the bytes `start..end`; the spaces left at the cut
/// collapse to one.
pub(crate) fn cut(line: &str, (start, end): (usize, usize)) -> String {
    let (Some(left), Some(right)) = (line.get(..start), line.get(end..)) else { return line.to_string() };
    let (left, right) = (left.trim_end(), right.trim_start());
    if left.is_empty() || right.is_empty() { format!("{left}{right}") } else { format!("{left} {right}") }
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
