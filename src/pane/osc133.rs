/// OSC 133 state machine — tracks shell command cycles.
///
/// Consumes OscEvent::Osc133 events from the processor and maintains
/// structured command state. Designed to recover gracefully from any
/// unexpected state: mode switches mid-command, garbled output, missing
/// markers, duplicate markers, etc.
///
/// Recovery principles:
/// - A is always a hard reset (shell returned to prompt)
/// - B is the latch (shell ready for input, finalizes any in-flight command)
/// - Accept loss gracefully (lose one command, never corrupt future state)
///
/// Command text priority: E > C cmdline_url > B-to-C capture (escape-stripped)
///
/// A single `capture` buffer accumulates all terminal text. C clears it
/// (delimiter between command echo and output). D consumes it. B clears
/// it (new cycle). This captures output even when C is absent (subshells,
/// shells without preexec support).

use crate::pane::state::{Activity, PaneState};

// --- State Machine ---

/// Current phase of the OSC 133 command cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Osc133Phase {
    /// No active command cycle. Waiting for A or recovering.
    Idle,
    /// A marker seen — prompt is being displayed.
    Prompt,
    /// B marker seen — user is at the input prompt, shell ready.
    Input,
    /// C marker seen — a command is executing.
    Executing {
        /// Command text (from E marker, C cmdline_url, or capture fallback).
        command: String,
    },
}

impl Osc133Phase {
    /// If the phase is Executing, return the command text.
    pub fn executing_command(&self) -> Option<&str> {
        match self {
            Osc133Phase::Executing { command } => Some(command.as_str()),
            _ => None,
        }
    }
}

/// The OSC 133 parser — a state machine that produces CommandRecords.
#[derive(Debug)]
pub struct Osc133Parser {
    /// Current phase in the command cycle.
    phase: Osc133Phase,
    /// Buffered command text from E marker (may arrive before C).
    pending_command_text: Option<String>,
    /// For B-Latch: pending exit code from D (finalized by next B).
    pending_exit_code: Option<i32>,
    /// Command text from the most recently completed Executing phase,
    /// held until B finalizes it.
    pending_command: Option<String>,
    /// Output from the most recently completed Executing phase,
    /// held until B finalizes it.
    pending_output: Option<String>,
    /// Single capture buffer — always accumulates terminal text.
    /// C clears it (delimiter). D consumes it. B clears it (new cycle).
    capture: String,
}

impl Osc133Parser {
    pub fn new() -> Self {
        Self {
            phase: Osc133Phase::Idle,
            pending_command_text: None,
            pending_exit_code: None,
            pending_command: None,
            pending_output: None,
            capture: String::new(),
        }
    }

    /// Current phase (for external queries).
    pub fn phase(&self) -> &Osc133Phase {
        &self.phase
    }

    /// Handle an OSC 133 marker event.
    ///
    /// `marker` is the uppercase letter (A-E).
    /// `param` is the optional parameter (exit code for D, command text for E,
    /// cmdline_url for C).
    /// `screen_input` is the command text read from the terminal grid (for C
    /// marker fallback). Provided by PaneProcessor which has access to the
    /// terminal.
    pub fn handle_marker(
        &mut self,
        marker: u8,
        param: Option<&str>,
        state: &mut PaneState,
        screen_input: Option<&str>,
    ) {
        state.last_osc133_marker = Some(std::time::Instant::now());
        match marker {
            b'A' => self.handle_a(state),
            b'B' => self.handle_b(state),
            b'C' => self.handle_c(param, state, screen_input),
            b'D' => self.handle_d(param, state),
            b'E' => self.handle_e(param, state),
            _ => tracing::debug!("Unknown OSC 133 marker: {}", marker as char),
        }
    }

    /// Append raw output text (bytes fed to terminal between OSC events).
    /// Accumulates into capture buffer AND the active command record's output.
    pub fn append_output(&mut self, text: &str, state: &mut PaneState) {
        self.capture.push_str(text);
        state.append_active_output(text);
    }

    /// Force reset to Idle (for mode switches or external reset).
    pub fn reset(&mut self, state: &mut PaneState) {
        self.abandon_in_flight(state);
        self.phase = Osc133Phase::Idle;
        self.pending_command_text = None;
        self.pending_exit_code = None;
        self.pending_command = None;
        self.pending_output = None;
        self.capture.clear();
        state.activity = Activity::Unknown;
    }

    // --- Marker Handlers ---

    /// A (Prompt Start): hard reset from any state.
    /// If Executing without D, the command is abandoned and recorded.
    /// Pending D state is preserved — D→A→B is the normal flow and B
    /// needs to see the pending state to finalize the command.
    fn handle_a(&mut self, state: &mut PaneState) {
        // If Executing without D, mark active command as abandoned.
        if matches!(self.phase, Osc133Phase::Executing { .. }) {
            if let Some(cmd) = state.active_command_mut() {
                tracing::debug!("Abandoning in-flight command on A: {:?}", cmd.command);
                cmd.completed = true; // exit_code stays None (abandoned)
            }
        }
        // NOTE: active command from D is NOT completed here.
        // D→A→B is the normal completion flow — B marks completed.
        self.phase = Osc133Phase::Prompt;
        self.pending_command_text = None;
        state.activity = Activity::Idle;
    }

    /// B (Input Ready / THE LATCH): shell is ready for input.
    /// From any state: transition to Input.
    /// If there's a pending command from D, finalize it now.
    fn handle_b(&mut self, state: &mut PaneState) {
        // B marks the active command as completed.
        if let Some(cmd) = state.active_command_mut() {
            // If we're in Executing and got B without D (pre-exec failure),
            // set exit code 1.
            if cmd.exit_code.is_none()
                && matches!(self.phase, Osc133Phase::Executing { .. })
            {
                cmd.exit_code = Some(1);
            }
            cmd.completed = true;
        }

        self.phase = Osc133Phase::Input;
        self.capture.clear();
        self.pending_command.take();
        self.pending_output.take();
        self.pending_exit_code.take();
        state.activity = Activity::Idle;
    }

    /// C (Command Executing): a command has started.
    /// From any state except Executing: create new Executing state.
    /// If already Executing: ignore (duplicate C).
    ///
    /// Command text priority: E (pending_command_text) > C cmdline_url > screen_input.
    /// `screen_input` is the command text read from the terminal grid by
    /// PaneProcessor — it handles all editing (backspace, cursor movement,
    /// etc.) correctly because it reads from the real terminal emulator.
    fn handle_c(&mut self, param: Option<&str>, state: &mut PaneState, screen_input: Option<&str>) {
        if matches!(self.phase, Osc133Phase::Executing { .. }) {
            tracing::debug!("Duplicate OSC 133;C — ignoring");
            return;
        }

        // Command text: E > C cmdline_url > screen input from terminal grid
        let command = self
            .pending_command_text
            .take()
            .or_else(|| param.and_then(parse_cmdline_url))
            .unwrap_or_else(|| screen_input.unwrap_or_default().to_string());

        // C is the delimiter — everything after is command output.
        self.capture.clear();

        // Push incomplete record so output accumulates into it live.
        state.push_command_start(command.clone());

        self.phase = Osc133Phase::Executing { command };
    }

    /// D (Command Done): command finished with optional exit code.
    /// Sets exit_code on the active record. Completion is finalized by B.
    /// Always increments completion_seq so command_run can detect completion.
    fn handle_d(&mut self, param: Option<&str>, state: &mut PaneState) {
        let exit_code = param.and_then(|p| p.parse::<i32>().ok());
        state.completion_seq += 1;
        state.last_exit_code = exit_code;

        match &self.phase {
            Osc133Phase::Executing { .. } => {
                // Normal case: C→D. Record already has output (pushed on C).
                // Set exit_code, wait for B to mark completed.
                if let Some(cmd) = state.active_command_mut() {
                    cmd.exit_code = exit_code;
                }
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
            Osc133Phase::Input => {
                // B→D pattern: no C marker (subshell, cd, export, etc.)
                // Push record now with captured output.
                let command = self.pending_command_text.take().unwrap_or_default();
                let output = std::mem::take(&mut self.capture);
                if !command.is_empty() || !output.is_empty() {
                    state.push_command_start(command);
                    if let Some(cmd) = state.active_command_mut() {
                        cmd.output = output;
                        cmd.exit_code = exit_code;
                    }
                }
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
            _ => {
                // D from unexpected state (race condition, mid-join, etc.)
                tracing::debug!(
                    "OSC 133;D from unexpected state {:?} — recovering",
                    self.phase
                );
                let output = std::mem::take(&mut self.capture);
                if !output.is_empty() {
                    state.push_command_start(String::new());
                    if let Some(cmd) = state.active_command_mut() {
                        cmd.output = output;
                        cmd.exit_code = exit_code;
                    }
                }
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
        }
    }

    /// E (Explicit Command Text): authoritative command text from the shell.
    fn handle_e(&mut self, param: Option<&str>, state: &mut PaneState) {
        let Some(text) = param else { return };

        match &mut self.phase {
            Osc133Phase::Executing { command } => {
                // E during Executing: update command text (authoritative override)
                *command = text.to_string();
                // Also update the active record
                if let Some(cmd) = state.active_command_mut() {
                    cmd.command = text.to_string();
                }
            }
            _ => {
                // E before C: buffer for later
                self.pending_command_text = Some(text.to_string());
            }
        }
    }

    // --- Internal Helpers ---

    /// Abandon any in-flight command from Executing state.
    /// Called on unexpected A marker (hard reset).
    fn abandon_in_flight(&mut self, state: &mut PaneState) {
        // Mark active command as completed (abandoned)
        if let Some(cmd) = state.active_command_mut() {
            tracing::debug!("Abandoning in-flight command on reset: {:?}", cmd.command);
            cmd.completed = true;
        }
        self.pending_command.take();
        self.pending_output.take();
        self.pending_exit_code.take();
    }
}

impl Default for Osc133Parser {
    fn default() -> Self {
        Self::new()
    }
}

// --- Helpers ---

/// Parse `cmdline_url=<percent-encoded>` from a C marker parameter string.
fn parse_cmdline_url(param: &str) -> Option<String> {
    let value = param.strip_prefix("cmdline_url=")?;
    Some(percent_decode(value))
}

/// Percent-decode a URL-encoded string.
fn percent_decode(input: &str) -> String {
    let mut result = Vec::with_capacity(input.len());
    let mut bytes = input.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let hi = bytes.next().and_then(|c| char::from(c).to_digit(16));
            let lo = bytes.next().and_then(|c| char::from(c).to_digit(16));
            if let (Some(h), Some(l)) = (hi, lo) {
                result.push((h * 16 + l) as u8);
            }
        } else if b == b'+' {
            result.push(b' ');
        } else {
            result.push(b);
        }
    }
    String::from_utf8_lossy(&result).into_owned()
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand: call handle_marker with no screen input.
    fn marker(parser: &mut Osc133Parser, m: u8, param: Option<&str>, state: &mut PaneState) {
        parser.handle_marker(m, param, state, None);
    }

    /// Shorthand for C marker with screen_input (simulating terminal grid read).
    fn marker_c(parser: &mut Osc133Parser, param: Option<&str>, state: &mut PaneState, screen_input: &str) {
        parser.handle_marker(b'C', param, state, Some(screen_input));
    }

    // --- Normal flows ---

    #[test]
    fn full_cycle_with_e_marker() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        assert_eq!(p.phase, Osc133Phase::Prompt);

        marker(&mut p, b'B', None, &mut s);
        assert_eq!(p.phase, Osc133Phase::Input);

        marker(&mut p, b'E', Some("ls -la"), &mut s);
        marker(&mut p, b'C', None, &mut s);
        assert!(matches!(p.phase, Osc133Phase::Executing { .. }));

        p.append_output("file1.txt\nfile2.txt\n", &mut s);

        marker(&mut p, b'D', Some("0"), &mut s);
        assert_eq!(p.phase, Osc133Phase::Idle);

        // Command exists but not completed — need B (the latch)
        assert_eq!(s.commands.len(), 1);
        assert!(!s.commands[0].completed);
        assert_eq!(s.commands[0].exit_code, Some(0)); // D set exit code

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        // NOW the command is finalized
        assert_eq!(s.commands.len(), 1);
        let cmd = &s.commands[0];
        assert_eq!(cmd.command, "ls -la");
        assert_eq!(cmd.output, "file1.txt\nfile2.txt\n");
        assert_eq!(cmd.exit_code, Some(0));
        assert!(cmd.completed);
    }

    #[test]
    fn full_cycle_without_e_uses_screen_input_fallback() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        p.append_output("$ echo hello", &mut s);
        // C marker with screen_input — no E, so screen input is used for command text
        p.handle_marker(b'C', None, &mut s, Some("echo hello"));
        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "echo hello");
        } else {
            panic!("Expected Executing");
        }

        p.append_output("hello\n", &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "echo hello");
        assert_eq!(s.commands[0].output, "hello\n");
    }

    #[test]
    fn c_without_e_or_screen_input_defaults_to_empty() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        // C with no E, no cmdline_url, no screen_input
        marker(&mut p, b'C', None, &mut s);
        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn c_cmdline_url_param() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        // C with cmdline_url parameter (URL-encoded)
        marker(&mut p, b'C', Some("cmdline_url=echo%20hello%20world"), &mut s);

        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "echo hello world");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn e_overrides_c_cmdline_url() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        // E arrives before C — takes priority over cmdline_url
        marker(&mut p, b'E', Some("correct"), &mut s);
        marker(&mut p, b'C', Some("cmdline_url=wrong"), &mut s);

        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "correct");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn e_before_c() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some("pwd"), &mut s);
        marker(&mut p, b'C', None, &mut s);

        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "pwd");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn e_overrides_during_executing() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        p.append_output("$ wrong", &mut s);
        marker(&mut p, b'C', None, &mut s);
        // E arrives after C and overrides the capture-based fallback
        marker(&mut p, b'E', Some("correct"), &mut s);

        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "correct");
        } else {
            panic!("Expected Executing");
        }
    }

    // --- No-output commands ---

    #[test]
    fn no_output_command_bd_pattern() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some("cd /tmp"), &mut s);
        // B→D pattern: D without C (cd has no output phase)
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "cd /tmp");
        assert_eq!(s.commands[0].exit_code, Some(0));
        assert!(s.commands[0].output.is_empty());
    }

    // --- Output capture without C marker (subshells, etc.) ---

    #[test]
    fn output_captured_without_c_marker() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        // Subshell: no C/E, but output arrives
        p.append_output("sub\n", &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        // Output is captured even without C
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].output, "sub\n");
        assert_eq!(s.commands[0].exit_code, Some(0));
    }

    #[test]
    fn partial_capture_on_mid_join() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        // Start monitoring mid-command — some output arrives before D
        p.append_output("partial output\n", &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        // Partial output is captured
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].output, "partial output\n");
    }

    // --- Recovery: missing markers ---

    #[test]
    fn missing_d_abandoned_on_a() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        p.append_output("$ long-cmd", &mut s);
        marker_c(&mut p, None, &mut s, "long-cmd");
        p.append_output("partial output", &mut s);

        // No D marker — A arrives (Ctrl+C, reset, etc.)
        marker(&mut p, b'A', None, &mut s);

        // The abandoned command is recorded with no exit code
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "long-cmd");
        assert_eq!(s.commands[0].output, "partial output");
        assert_eq!(s.commands[0].exit_code, None);
    }

    #[test]
    fn d_without_c_bumps_completion_seq() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        // D without C — syntax error, history expansion failure, etc.
        marker(&mut p, b'D', Some("1"), &mut s);
        assert_eq!(s.completion_seq, 1);

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert!(s.commands.is_empty());
        assert_eq!(p.phase, Osc133Phase::Input);
    }

    #[test]
    fn a_without_d_abandons_executing_command() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        p.append_output("$ bad-syntax", &mut s);
        marker_c(&mut p, None, &mut s, "bad-syntax");
        // A arrives without D — command abandoned
        marker(&mut p, b'A', None, &mut s);
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "bad-syntax");
        assert_eq!(s.commands[0].exit_code, None);

        marker(&mut p, b'B', None, &mut s);
        assert_eq!(s.commands.len(), 1);
    }

    // --- Recovery: duplicate markers ---

    #[test]
    fn duplicate_c_ignored() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        p.append_output("$ cmd", &mut s);
        marker_c(&mut p, None, &mut s, "cmd");
        p.append_output("out1", &mut s);
        // Duplicate C — should be ignored, output continues
        marker(&mut p, b'C', None, &mut s);
        p.append_output("out2", &mut s);

        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "cmd");
        } else {
            panic!("Expected Executing");
        }
        assert_eq!(p.capture, "out1out2");
    }

    #[test]
    fn duplicate_a_resets_cleanly() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'A', None, &mut s);
        assert_eq!(p.phase, Osc133Phase::Prompt);
    }

    #[test]
    fn duplicate_b_resets_to_input() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        assert_eq!(p.phase, Osc133Phase::Input);
    }

    // --- Recovery: unexpected states ---

    #[test]
    fn d_from_idle_recovers() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        p.append_output("some output", &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);
        assert_eq!(p.phase, Osc133Phase::Idle);
        assert_eq!(s.completion_seq, 1);
    }

    #[test]
    fn c_from_idle_creates_executing() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        p.append_output("$ surprise", &mut s);
        marker_c(&mut p, None, &mut s, "surprise");
        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "surprise");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn reset_clears_everything() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'C', None, &mut s);
        p.append_output("partial", &mut s);

        p.reset(&mut s);

        assert_eq!(p.phase, Osc133Phase::Idle);
        assert_eq!(s.activity, Activity::Unknown);
        assert!(p.capture.is_empty());
    }

    // --- Activity tracking ---

    #[test]
    fn activity_transitions() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        assert_eq!(s.activity, Activity::Unknown);

        marker(&mut p, b'A', None, &mut s);
        assert_eq!(s.activity, Activity::Idle);

        marker(&mut p, b'B', None, &mut s);
        assert_eq!(s.activity, Activity::Idle);

        marker(&mut p, b'C', None, &mut s);
        // C doesn't set activity — processor sets Busy

        marker(&mut p, b'D', Some("0"), &mut s);
        assert_eq!(s.activity, Activity::Idle);
    }

    // --- Rapid commands ---

    #[test]
    fn rapid_back_to_back_commands() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        // Command 1
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some("echo 1"), &mut s);
        marker(&mut p, b'C', None, &mut s);
        p.append_output("1\n", &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);

        // Command 2 (B finalizes command 1)
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "echo 1");

        marker(&mut p, b'E', Some("echo 2"), &mut s);
        marker(&mut p, b'C', None, &mut s);
        p.append_output("2\n", &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);

        // Command 3 (B finalizes command 2)
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        assert_eq!(s.commands.len(), 2);
        assert_eq!(s.commands[0].command, "echo 2");
        assert_eq!(s.commands[1].command, "echo 1");
    }

    // --- Empty command ---

    #[test]
    fn empty_command_just_enter() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some(""), &mut s);
        marker(&mut p, b'C', None, &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        // Empty command still creates a record (pushed on C)
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "");
        assert!(s.commands[0].completed);
    }

    // --- Error exit codes ---

    #[test]
    fn nonzero_exit_code() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some("false"), &mut s);
        marker(&mut p, b'C', None, &mut s);
        marker(&mut p, b'D', Some("1"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].exit_code, Some(1));
    }

    #[test]
    fn signal_exit_code() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some("sleep 100"), &mut s);
        marker(&mut p, b'C', None, &mut s);
        marker(&mut p, b'D', Some("130"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(s.commands[0].exit_code, Some(130));
    }

    // --- Output accumulation ---

    #[test]
    fn output_accumulates_between_c_and_d() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some("ls"), &mut s);
        marker(&mut p, b'C', None, &mut s);

        p.append_output("file1\n", &mut s);
        p.append_output("file2\n", &mut s);
        p.append_output("file3\n", &mut s);

        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(s.commands[0].output, "file1\nfile2\nfile3\n");
    }

    #[test]
    fn capture_cleared_by_b() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        p.append_output("stray output", &mut s);
        marker(&mut p, b'A', None, &mut s);
        p.append_output("prompt text", &mut s);
        marker(&mut p, b'B', None, &mut s);

        // B clears the capture buffer
        assert!(p.capture.is_empty());
    }

    #[test]
    fn capture_cleared_by_c() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        p.append_output("command echo text", &mut s);
        marker(&mut p, b'C', None, &mut s);

        // C clears capture — post-C output starts fresh
        assert!(p.capture.is_empty());
        p.append_output("real output", &mut s);
        assert_eq!(p.capture, "real output");
    }

    // --- Recovery from arbitrary start position ---

    #[test]
    fn recover_from_missed_start_then_next_command_works() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(p.phase, Osc133Phase::Input);

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        p.append_output("$ echo hello", &mut s);
        marker_c(&mut p, None, &mut s, "echo hello");
        p.append_output("hello\n", &mut s);
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert!(
            s.commands.iter().any(|c| c.command == "echo hello"),
            "expected 'echo hello' in history, got: {:?}",
            s.commands,
        );
    }

    #[test]
    fn command_without_c_marker_bumps_completion_seq() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        assert_eq!(s.completion_seq, 0);

        marker(&mut p, b'D', Some("0"), &mut s);
        assert_eq!(s.completion_seq, 1, "D must bump completion_seq");

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert!(s.commands.is_empty());
        assert_eq!(p.phase, Osc133Phase::Input);
    }

    #[test]
    fn d_a_b_flow_pending_survives_through_a() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        marker(&mut p, b'E', Some("ls"), &mut s);
        marker(&mut p, b'C', None, &mut s);
        p.append_output("file1\n", &mut s);

        marker(&mut p, b'D', Some("0"), &mut s);
        assert_eq!(s.commands.len(), 1, "record pushed on C");
        assert!(!s.commands[0].completed, "D should not complete yet");

        marker(&mut p, b'A', None, &mut s);
        assert!(!s.commands[0].completed, "A should not complete (that's B's job)");

        marker(&mut p, b'B', None, &mut s);
        assert!(s.commands[0].completed, "B should complete the command");
        assert_eq!(s.commands[0].command, "ls");
        assert_eq!(s.commands[0].exit_code, Some(0));
    }

    #[test]
    fn multiple_commands_without_c_bump_completion_seq() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        for (i, exit_code) in ["0", "1", "0"].iter().enumerate() {
            marker(&mut p, b'D', Some(exit_code), &mut s);
            assert_eq!(s.completion_seq, (i + 1) as u64);
            marker(&mut p, b'A', None, &mut s);
            marker(&mut p, b'B', None, &mut s);
        }

        assert_eq!(p.phase, Osc133Phase::Input);
        assert_eq!(s.completion_seq, 3);
    }

    #[test]
    fn state_is_input_after_recovery() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(p.phase, Osc133Phase::Input, "should be in Input after D→A→B recovery");
    }

    // --- Helpers ---

    #[test]
    fn parse_cmdline_url_basic() {
        assert_eq!(parse_cmdline_url("cmdline_url=echo%20hello"), Some("echo hello".into()));
    }

    #[test]
    fn parse_cmdline_url_special_chars() {
        assert_eq!(
            parse_cmdline_url("cmdline_url=ls%20-la%20%2Ftmp"),
            Some("ls -la /tmp".into()),
        );
    }

    #[test]
    fn parse_cmdline_url_no_prefix() {
        assert_eq!(parse_cmdline_url("something_else=foo"), None);
    }

    #[test]
    fn percent_decode_plus_as_space() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }

    // --- Osc133Phase ---

    #[test]
    fn executing_command_returns_text() {
        assert_eq!(Osc133Phase::Idle.executing_command(), None);
        assert_eq!(Osc133Phase::Prompt.executing_command(), None);
        assert_eq!(Osc133Phase::Input.executing_command(), None);
        assert_eq!(
            Osc133Phase::Executing { command: "ls".into() }.executing_command(),
            Some("ls"),
        );
    }
}
