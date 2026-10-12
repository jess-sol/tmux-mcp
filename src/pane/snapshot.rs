//! What a decision reads about a pane, at one *Pane version*.

use crate::pane::osc133::Osc133Phase;
use crate::pane::registry::PaneMode;
use crate::pane::state::{Activity, CommandRecord};

/// A fact the stream can announce before its value: Unknown until the value
/// arrives. A decision that needs an Unknown fact waits for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fact<T> {
    Unknown,
    Known(T),
}

/// The facts of a pane at one *Pane version*. The event loop publishes a new
/// *Pane snapshot* whenever a fact changes; a reader takes it whole.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PaneSnapshot {
    pub version: u64,
    /// The count of policy reloads: every decision reads the policy.
    pub policy: u64,
    pub mode: Fact<PaneMode>,
    pub phase: Osc133Phase,
    pub activity: Activity,
    /// The text after the prompt.
    pub input: String,
    pub input_garbled: bool,
    /// OSC 7 reports since the last command started; a command that runs
    /// (`ssh`) can leave an older report standing for another machine.
    pub cwd: Option<String>,
    pub host: Option<String>,
    pub user: Option<String>,
    /// Newest first.
    pub commands: Vec<CommandSummary>,
}

/// A command as the *Pane snapshot* lists it; its output is read apart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandSummary {
    pub id: u64,
    pub command: String,
    pub exit_code: Option<i32>,
    pub output_done: bool,
    pub completed: bool,
}

impl From<&CommandRecord> for CommandSummary {
    fn from(record: &CommandRecord) -> Self {
        CommandSummary {
            id: record.id,
            command: record.command.clone(),
            exit_code: record.exit_code,
            output_done: record.output_done,
            completed: record.completed,
        }
    }
}
