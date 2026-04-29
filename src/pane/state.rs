/// Structured pane state — the queryable output of the parsing pipeline.

use std::collections::VecDeque;
use std::time::Instant;

/// Maximum commands retained in history per pane.
const MAX_COMMAND_HISTORY: usize = 100;

/// A completed command with its output and metadata.
#[derive(Debug, Clone)]
pub struct CommandRecord {
    /// The command text as typed by the user.
    pub command: String,
    /// Command output (text between C and D markers, escape sequences stripped).
    pub output: String,
    /// Exit code from D marker. None if unknown (e.g., D never arrived).
    pub exit_code: Option<i32>,
    /// Monotonic sequence number for ordering (higher = newer).
    pub seq: u64,
    /// When this command was finalized.
    pub timestamp: Instant,
}

/// Activity state of a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Activity {
    /// Shell is at a prompt, ready for input.
    Idle,
    /// A command is executing.
    Busy,
    /// State unknown (no markers seen yet, or after recovery).
    #[default]
    Unknown,
}

/// Structured state for a pane, maintained by the mode parser.
#[derive(Debug)]
pub struct PaneState {
    /// Completed commands, most recent first.
    pub commands: VecDeque<CommandRecord>,
    /// Current activity.
    pub activity: Activity,
    /// Current working directory (from OSC 7).
    pub cwd: Option<String>,
    /// Hostname (from OSC 7, None if localhost).
    pub hostname: Option<String>,
    /// Next sequence number.
    seq_counter: u64,
    /// Incremented on every D marker (command completion signal).
    /// Used by command_run to detect completion even when the command
    /// isn't recorded in history (e.g., no C marker, empty command).
    pub completion_seq: u64,
    /// Exit code from the most recent D marker.
    pub last_exit_code: Option<i32>,
    /// When the most recent OSC 133 marker was received.
    /// Used to determine if shell integration is active.
    pub last_osc133_marker: Option<Instant>,
    /// When the most recent terminal data (%output) was received.
    pub last_data: Option<Instant>,
}

impl PaneState {
    pub fn new() -> Self {
        Self {
            commands: VecDeque::new(),
            activity: Activity::Unknown,
            cwd: None,
            hostname: None,
            seq_counter: 0,
            completion_seq: 0,
            last_exit_code: None,
            last_osc133_marker: None,
            last_data: None,
        }
    }

    /// Record a completed command.
    pub fn push_command(&mut self, command: String, output: String, exit_code: Option<i32>) {
        self.seq_counter += 1;
        self.commands.push_front(CommandRecord {
            command,
            output,
            exit_code,
            seq: self.seq_counter,
            timestamp: Instant::now(),
        });
        while self.commands.len() > MAX_COMMAND_HISTORY {
            self.commands.pop_back();
        }
    }

    /// Get the most recent N commands (newest first).
    pub fn recent_commands(&self, n: usize) -> Vec<&CommandRecord> {
        self.commands.iter().take(n).collect()
    }

    /// Update cwd and hostname from an OSC 7 URI.
    /// Format: file://hostname/path
    pub fn update_cwd_from_osc7(&mut self, uri: &str) {
        if let Some(rest) = uri.strip_prefix("file://") {
            let (hostname, path) = match rest.find('/') {
                Some(idx) => (&rest[..idx], &rest[idx..]),
                None => ("", rest),
            };

            self.cwd = Some(path.to_string());

            // Filter localhost variants
            self.hostname = if hostname.is_empty()
                || hostname == "localhost"
                || hostname == "127.0.0.1"
                || hostname == "::1"
            {
                None
            } else {
                Some(hostname.to_string())
            };
        }
    }
}

impl Default for PaneState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_empty() {
        let s = PaneState::new();
        assert!(s.commands.is_empty());
        assert_eq!(s.activity, Activity::Unknown);
        assert!(s.cwd.is_none());
        assert!(s.hostname.is_none());
    }

    #[test]
    fn push_and_retrieve_commands() {
        let mut s = PaneState::new();
        s.push_command("ls".into(), "file1\nfile2".into(), Some(0));
        s.push_command("pwd".into(), "/home/user".into(), Some(0));

        let recent = s.recent_commands(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].command, "pwd"); // newest first
        assert_eq!(recent[1].command, "ls");
        assert!(recent[0].seq > recent[1].seq);
    }

    #[test]
    fn command_history_capped() {
        let mut s = PaneState::new();
        for i in 0..150 {
            s.push_command(format!("cmd{}", i), String::new(), Some(0));
        }
        assert_eq!(s.commands.len(), MAX_COMMAND_HISTORY);
        assert_eq!(s.commands.front().unwrap().command, "cmd149");
    }

    #[test]
    fn osc7_with_hostname() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://myhost/home/user");
        assert_eq!(s.cwd.as_deref(), Some("/home/user"));
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
    }

    #[test]
    fn osc7_localhost_filtered() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://localhost/tmp");
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
        assert!(s.hostname.is_none());
    }

    #[test]
    fn osc7_no_hostname() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file:///home/user");
        assert_eq!(s.cwd.as_deref(), Some("/home/user"));
        assert!(s.hostname.is_none());
    }

    #[test]
    fn osc7_127001_filtered() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://127.0.0.1/var/log");
        assert_eq!(s.cwd.as_deref(), Some("/var/log"));
        assert!(s.hostname.is_none());
    }
}
