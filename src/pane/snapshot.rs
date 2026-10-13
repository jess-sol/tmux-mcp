//! What a decision reads about a pane, at one *Pane version*.

use crate::pane::osc133::Osc133Phase;
use crate::pane::registry::PaneMode;
use crate::pane::state::CommandRecord;
use crate::policy::PaneContext;

/// A fact of a pane as far as the daemon knows it: Unknown from an event
/// that changes it until the event that says to what.
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
    /// Where the shell's OSC 133 markers have taken the pane: Known at
    /// `Input` or `Executing`, Unknown from a command line of the daemon's
    /// until a marker answers it.
    pub phase: Fact<Osc133Phase>,
    /// Whether the shell sends OSC 133 markers: true from any marker, false
    /// when a command line of the daemon's waited for one in vain.
    pub integration: Fact<bool>,
    /// The text after the prompt.
    pub input: String,
    pub input_garbled: bool,
    /// OSC 7 reports since the last command started; a command that runs
    /// (`ssh`) can leave an older report standing for another machine.
    pub cwd: Option<String>,
    pub host: Option<String>,
    pub user: Option<String>,
    /// The shell's process, once tmux has said.
    pub pid: Option<u32>,
    /// Newest first; each command's output is read apart.
    pub commands: Vec<CommandRecord>,
}

/// What policy and approvals judge a command by: where the next command runs.
impl From<&PaneSnapshot> for PaneContext {
    fn from(snapshot: &PaneSnapshot) -> Self {
        PaneContext {
            hostname: snapshot.host.clone(),
            cwd: snapshot.cwd.clone(),
            user: snapshot.user.clone(),
        }
    }
}
