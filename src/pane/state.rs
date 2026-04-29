/// Structured pane state — the queryable output of the parsing pipeline.

use std::collections::VecDeque;
use std::time::Instant;

/// Maximum commands retained in history per pane.
const MAX_COMMAND_HISTORY: usize = 100;

/// A command with its output — may be in-progress or completed.
#[derive(Debug, Clone)]
pub struct CommandRecord {
    /// Monotonic ID for addressing (higher = newer).
    pub id: u64,
    /// The command text as typed by the user.
    pub command: String,
    /// Command output — accumulates live during execution, persists after completion.
    pub output: String,
    /// Exit code from D marker. None if still running or unknown.
    pub exit_code: Option<i32>,
    /// Whether the D marker has been received (output is done).
    /// Distinct from exit_code because D can arrive without an exit code parameter.
    pub output_done: bool,
    /// Whether the command has completed (B marker finalized).
    pub completed: bool,
    /// Read cursor — line offset into output. Only `next` advances this.
    pub read_cursor: usize,
    /// When this record was created.
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
    /// Username (from OSC 7 userinfo, e.g. file://user@host/path).
    pub user: Option<String>,
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
    /// LRU cache of leaf PIDs → OSC 133 confirmed (true) or failed (false).
    /// Bounded to 5 entries. Used by command_run to gate on marker availability.
    pub osc133_cache: VecDeque<(u32, bool)>,
    /// Cursor position (line, col) at B marker time — the start of user input,
    /// right after the prompt. Used by PaneProcessor to read typed input from
    /// the terminal grid.
    pub input_start: Option<(usize, usize)>,
}

const MAX_OSC133_CACHE: usize = 5;

impl PaneState {
    pub fn new() -> Self {
        Self {
            commands: VecDeque::new(),
            activity: Activity::Unknown,
            cwd: None,
            user: None,
            hostname: None,
            seq_counter: 0,
            completion_seq: 0,
            last_exit_code: None,
            last_osc133_marker: None,
            last_data: None,
            osc133_cache: VecDeque::new(),
            input_start: None,
        }
    }

    /// Look up a leaf PID in the OSC 133 cache.
    /// Returns Some(true) if confirmed, Some(false) if failed, None if unknown.
    pub fn osc133_lookup(&self, pid: u32) -> Option<bool> {
        self.osc133_cache.iter().find(|(p, _)| *p == pid).map(|(_, v)| *v)
    }

    /// Mark a leaf PID as having working OSC 133.
    pub fn osc133_confirm(&mut self, pid: u32) {
        self.osc133_upsert(pid, true);
    }

    /// Mark a leaf PID as not having working OSC 133.
    pub fn osc133_fail(&mut self, pid: u32) {
        self.osc133_upsert(pid, false);
    }

    fn osc133_upsert(&mut self, pid: u32, confirmed: bool) {
        self.osc133_cache.retain(|(p, _)| *p != pid);
        self.osc133_cache.push_front((pid, confirmed));
        while self.osc133_cache.len() > MAX_OSC133_CACHE {
            self.osc133_cache.pop_back();
        }
    }

    /// Push an incomplete (in-progress) command record. Returns its ID.
    pub fn push_command_start(&mut self, command: String) -> u64 {
        self.seq_counter += 1;
        let id = self.seq_counter;
        self.commands.push_front(CommandRecord {
            id,
            command,
            output: String::new(),
            exit_code: None,
            output_done: false,
            completed: false,
            read_cursor: 0,
            timestamp: Instant::now(),
        });
        while self.commands.len() > MAX_COMMAND_HISTORY {
            self.commands.pop_back();
        }
        id
    }

    /// Get the active (incomplete) command, if any.
    pub fn active_command(&self) -> Option<&CommandRecord> {
        self.commands.front().filter(|c| !c.completed)
    }

    /// Get a mutable ref to the active command.
    pub fn active_command_mut(&mut self) -> Option<&mut CommandRecord> {
        self.commands.front_mut().filter(|c| !c.completed)
    }

    /// Append output to the active (incomplete) command.
    /// Stops accumulating after D marker (output_done), remaining text before B
    /// is prompt content.
    pub fn append_active_output(&mut self, text: &str) {
        if let Some(cmd) = self.commands.front_mut() {
            if !cmd.completed && !cmd.output_done {
                cmd.output.push_str(text);
            }
        }
    }

    /// Find a command by ID.
    pub fn command_by_id(&self, id: u64) -> Option<&CommandRecord> {
        self.commands.iter().find(|c| c.id == id)
    }

    /// Find a command by ID (mutable).
    pub fn command_by_id_mut(&mut self, id: u64) -> Option<&mut CommandRecord> {
        self.commands.iter_mut().find(|c| c.id == id)
    }

    /// Get the most recent N commands (newest first).
    pub fn recent_commands(&self, n: usize) -> Vec<&CommandRecord> {
        self.commands.iter().take(n).collect()
    }

    /// Update user, hostname, and cwd from an OSC 7 URI.
    /// Format: file://[user@]hostname/path
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
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
    }

    #[test]
    fn push_and_retrieve_commands() {
        let mut s = PaneState::new();
        let id1 = s.push_command_start("ls".into());
        let cmd = s.command_by_id_mut(id1).unwrap();
        cmd.output = "file1\nfile2".into();
        cmd.exit_code = Some(0);
        cmd.completed = true;

        let id2 = s.push_command_start("pwd".into());
        let cmd = s.command_by_id_mut(id2).unwrap();
        cmd.output = "/home/user".into();
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
        let mut s = PaneState::new();
        for i in 0..150 {
            let id = s.push_command_start(format!("cmd{}", i));
            let cmd = s.command_by_id_mut(id).unwrap();
            cmd.completed = true;
        }
        assert_eq!(s.commands.len(), MAX_COMMAND_HISTORY);
        assert_eq!(s.commands.front().unwrap().command, "cmd149");
    }

    // --- OSC 7 parsing: all combinations of user/host/path ---

    #[test]
    fn osc7_full() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@myhost/home/alice");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/home/alice"));
    }

    #[test]
    fn osc7_user_localhost() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@localhost/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_user_127001() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@127.0.0.1/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_user_ipv6_loopback() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@::1/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_user_empty_host() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_host_no_user() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://myhost/home/user");
        assert!(s.user.is_none());
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/home/user"));
    }

    #[test]
    fn osc7_localhost_no_user() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://localhost/tmp");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_empty_authority() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file:///home/user");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert_eq!(s.cwd.as_deref(), Some("/home/user"));
    }

    #[test]
    fn osc7_no_path() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@myhost");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_only_user_no_path() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@");
        assert_eq!(s.user.as_deref(), Some("alice"));
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_empty_user_at_host() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://@myhost/tmp");
        assert!(s.user.is_none());
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_at_sign_in_user() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://a@b@myhost/tmp");
        assert_eq!(s.user.as_deref(), Some("a@b"));
        assert_eq!(s.hostname.as_deref(), Some("myhost"));
        assert_eq!(s.cwd.as_deref(), Some("/tmp"));
    }

    #[test]
    fn osc7_bare_file() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_not_file_scheme() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("https://example.com/path");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_empty_string() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("");
        assert!(s.user.is_none());
        assert!(s.hostname.is_none());
        assert!(s.cwd.is_none());
    }

    #[test]
    fn osc7_overwrites_previous() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@host1/a");
        s.update_cwd_from_osc7("file://bob@host2/b");
        assert_eq!(s.user.as_deref(), Some("bob"));
        assert_eq!(s.hostname.as_deref(), Some("host2"));
        assert_eq!(s.cwd.as_deref(), Some("/b"));
    }

    #[test]
    fn osc7_clears_user_when_absent() {
        let mut s = PaneState::new();
        s.update_cwd_from_osc7("file://alice@myhost/tmp");
        assert_eq!(s.user.as_deref(), Some("alice"));
        // Next OSC 7 without user should clear it
        s.update_cwd_from_osc7("file://myhost/tmp");
        assert!(s.user.is_none());
    }

    // --- OSC 133 cache ---

    #[test]
    fn osc133_cache_lookup_empty() {
        let s = PaneState::new();
        assert_eq!(s.osc133_lookup(100), None);
    }

    #[test]
    fn osc133_cache_confirm_and_lookup() {
        let mut s = PaneState::new();
        s.osc133_confirm(100);
        assert_eq!(s.osc133_lookup(100), Some(true));
        assert_eq!(s.osc133_lookup(200), None);
    }

    #[test]
    fn osc133_cache_fail_and_lookup() {
        let mut s = PaneState::new();
        s.osc133_fail(100);
        assert_eq!(s.osc133_lookup(100), Some(false));
    }

    #[test]
    fn osc133_cache_confirm_overrides_fail() {
        let mut s = PaneState::new();
        s.osc133_fail(100);
        assert_eq!(s.osc133_lookup(100), Some(false));
        s.osc133_confirm(100);
        assert_eq!(s.osc133_lookup(100), Some(true));
    }

    #[test]
    fn osc133_cache_fail_overrides_confirm() {
        let mut s = PaneState::new();
        s.osc133_confirm(100);
        s.osc133_fail(100);
        assert_eq!(s.osc133_lookup(100), Some(false));
    }

    #[test]
    fn osc133_cache_separate_pids() {
        let mut s = PaneState::new();
        s.osc133_confirm(100);
        s.osc133_fail(200);
        assert_eq!(s.osc133_lookup(100), Some(true));
        assert_eq!(s.osc133_lookup(200), Some(false));
        assert_eq!(s.osc133_lookup(300), None);
    }

    #[test]
    fn osc133_cache_bounded_at_5() {
        let mut s = PaneState::new();
        for pid in 1..=6 {
            s.osc133_confirm(pid);
        }
        // PID 1 should have been evicted
        assert_eq!(s.osc133_lookup(1), None);
        // PIDs 2-6 should be present
        for pid in 2..=6 {
            assert_eq!(s.osc133_lookup(pid), Some(true));
        }
    }
}
