/// Structured pane state — the queryable output of the parsing pipeline.
use std::collections::{HashMap, VecDeque};
use std::time::Instant;

/// Maximum commands retained in history per pane.
const MAX_COMMAND_HISTORY: usize = 100;

/// A command — may be in-progress or completed. Its output is read apart,
/// through `TerminalState::output`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandRecord {
    /// Monotonic ID for addressing (higher = newer).
    pub id: u64,
    /// The command text as typed by the user.
    pub command: String,
    /// Exit code from D marker. None if still running or unknown.
    pub exit_code: Option<i32>,
    /// Whether the D marker has been received (output is done).
    /// Distinct from exit_code because D can arrive without an exit code parameter.
    pub output_done: bool,
    /// Whether the command has completed (B marker finalized).
    pub completed: bool,
}

/// Structured state for a pane, maintained by the mode parser.
#[derive(Debug)]
pub struct TerminalState {
    /// Completed commands, most recent first.
    pub commands: VecDeque<CommandRecord>,
    /// Each command's output by id: it accumulates live during execution and
    /// persists after completion, as long as its record.
    outputs: HashMap<u64, String>,
    /// Current working directory (from OSC 7).
    pub cwd: Option<String>,
    /// Username (from OSC 7 userinfo, e.g. file://user@host/path).
    pub user: Option<String>,
    /// Hostname (from OSC 7, None if localhost).
    pub hostname: Option<String>,
    /// OSC 7 reported `cwd` and `hostname` since the last command started
    /// (OSC 133 C). While a command runs, a session it opens (`ssh`) can
    /// leave the last report standing for another machine.
    pub osc7_since_command: bool,
    /// Next sequence number.
    seq_counter: u64,
    /// When the most recent terminal data (%output) was received.
    pub last_data: Option<Instant>,
    /// Cursor position at B marker time — the start of user input, right
    /// after the prompt — as (absolute line, column). Absolute lines keep the
    /// input anchored when output scrolls the terminal grid.
    pub input_start: Option<(i64, usize)>,
}

impl TerminalState {
    pub fn new() -> Self {
        Self {
            commands: VecDeque::new(),
            outputs: HashMap::new(),
            cwd: None,
            user: None,
            hostname: None,
            osc7_since_command: false,
            seq_counter: 0,
            last_data: None,
            input_start: None,
        }
    }

    /// Push an incomplete (in-progress) command record. Returns its ID.
    pub fn push_command_start(&mut self, command: String) -> u64 {
        self.seq_counter += 1;
        let id = self.seq_counter;
        self.commands.push_front(CommandRecord {
            id,
            command,
            exit_code: None,
            output_done: false,
            completed: false,
        });
        self.outputs.insert(id, String::new());
        while self.commands.len() > MAX_COMMAND_HISTORY {
            if let Some(oldest) = self.commands.pop_back() {
                self.outputs.remove(&oldest.id);
            }
        }
        id
    }

    /// The output of the command `command_id`, while its record is kept.
    pub fn output(&self, command_id: u64) -> Option<&str> {
        self.outputs.get(&command_id).map(String::as_str)
    }

    /// Get a mutable ref to the active command.
    pub fn active_command_mut(&mut self) -> Option<&mut CommandRecord> {
        self.commands.front_mut().filter(|c| !c.completed)
    }

    /// Append output to the active (incomplete) command.
    /// Stops accumulating after D marker (output_done), remaining text before B
    /// is prompt content.
    pub fn append_active_output(&mut self, text: &str) {
        if let Some(cmd) = self.commands.front()
            && !cmd.completed
            && !cmd.output_done
        {
            self.outputs.entry(cmd.id).or_default().push_str(text);
        }
    }

    /// Get the most recent N commands (newest first).
    pub fn recent_commands(&self, n: usize) -> Vec<&CommandRecord> {
        self.commands.iter().take(n).collect()
    }

    /// Update user, hostname, and cwd from an OSC 7 URI.
    /// Format: `file://[user@]hostname/path`
    pub fn update_cwd_from_osc7(&mut self, uri: &str) {
        let Some(rest) = uri.strip_prefix("file://") else {
            return;
        };

        let (authority, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], Some(&rest[idx..])),
            None => (rest, None),
        };

        // Split authority on last '@' — userinfo can contain '@', hostname cannot
        let (user, hostname) = match authority.rfind('@') {
            Some(idx) => (Some(&authority[..idx]), &authority[idx + 1..]),
            None => (None, authority),
        };

        self.user = user.filter(|u| !u.is_empty()).map(|u| u.to_string());
        self.cwd = path.map(|p| p.to_string());
        self.osc7_since_command = true;

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

impl Default for TerminalState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_state_is_empty() {
        let s = TerminalState::new();
        assert!(s.commands.is_empty());
        assert!(s.cwd.is_none());
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
    }

    #[test]
    fn push_and_retrieve_commands() {
        let mut s = TerminalState::new();
        s.push_command_start("ls".into());
        s.append_active_output("file1\nfile2");
        let cmd = s.commands.front_mut().unwrap();
        cmd.exit_code = Some(0);
        cmd.completed = true;

        s.push_command_start("pwd".into());
        s.append_active_output("/home/user");
        let cmd = s.commands.front_mut().unwrap();
        cmd.exit_code = Some(0);
        cmd.completed = true;

        let recent = s.recent_commands(10);
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].command, "pwd"); // newest first
        assert_eq!(recent[1].command, "ls");
        assert!(recent[0].id > recent[1].id);
    }

    #[test]
    fn command_history_capped() {
        let mut s = TerminalState::new();
        for i in 0..150 {
            s.push_command_start(format!("cmd{}", i));
            s.commands.front_mut().unwrap().completed = true;
        }
        assert_eq!(s.commands.len(), MAX_COMMAND_HISTORY);
        assert_eq!(s.commands.front().unwrap().command, "cmd149");
    }

    // --- OSC 7 parsing: all combinations of user/host/path ---

    #[test]
    fn osc7_full() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@myhost/home/alice");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/home/alice"));
    }

    #[test]
    fn osc7_user_localhost() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@localhost/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_user_127001() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@127.0.0.1/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_user_ipv6_loopback() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@::1/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_user_empty_host() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_host_no_user() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://myhost/home/user");
        assert!(s.user.is_none());
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/home/user"));
    }

    #[test]
    fn osc7_localhost_no_user() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://localhost/tmp");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_empty_authority() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file:///home/user");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/home/user"));
    }

    #[test]
    fn osc7_no_path() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@myhost");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_only_user_no_path() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_empty_user_at_host() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://@myhost/tmp");
        assert!(s.user.is_none());
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_at_sign_in_user() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://a@b@myhost/tmp");
        assert_eq!(s.user.as_deref(), Some("a@b"));
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_bare_file() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_not_file_scheme() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("https://example.com/path");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_empty_string() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_overwrites_previous() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@host1/a");
        s.update_cwd_from_osc7("file://bob@host2/b");
        assert_eq!(s.user.as_deref(), Some("bob"));
        assert_eq!(s.hostname.as_deref(), Some("host2"));
        assert_eq!(s.cwd.as_deref(), Some("/b"));
    }

    #[test]
    fn osc7_clears_user_when_absent() {
        let mut s = TerminalState::new();
        s.update_cwd_from_osc7("file://alice@myhost/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        // Next OSC 7 without user should clear it
        s.update_cwd_from_osc7("file://myhost/tmp");
        assert!(s.user.is_none());
    }
}
