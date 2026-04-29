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
    pub fn handle_marker(
        &mut self,
        marker: u8,
        param: Option<&str>,
        state: &mut PaneState,
    ) {
        state.last_osc133_marker = Some(std::time::Instant::now());
        match marker {
            b'A' => self.handle_a(state),
            b'B' => self.handle_b(state),
            b'C' => self.handle_c(param),
            b'D' => self.handle_d(param, state),
            b'E' => self.handle_e(param),
            _ => tracing::debug!("Unknown OSC 133 marker: {}", marker as char),
        }
    }

    /// Append raw output text (bytes fed to terminal between OSC events).
    /// Always accumulates into the capture buffer regardless of phase.
    pub fn append_output(&mut self, text: &str) {
        self.capture.push_str(text);
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
        // If we were Executing without a D marker, record as abandoned.
        if let Osc133Phase::Executing { command } = &self.phase {
            let output = std::mem::take(&mut self.capture);
            if !command.is_empty() || !output.is_empty() {
                tracing::debug!("Abandoning in-flight command on A: {:?}", command);
                state.push_command(command.clone(), output, None);
            }
        }
        // NOTE: pending_command/output/exit_code from D are NOT cleared here.
        // D→A→B is the normal completion flow — B finalizes.
        self.phase = Osc133Phase::Prompt;
        self.pending_command_text = None;
        state.activity = Activity::Idle;
    }

    /// B (Input Ready / THE LATCH): shell is ready for input.
    /// From any state: transition to Input.
    /// If there's a pending command from D, finalize it now.
    fn handle_b(&mut self, state: &mut PaneState) {
        // B finalizes any pending command (from D marker)
        if self.pending_exit_code.is_some() || self.pending_command.is_some() {
            let command = self.pending_command.take().unwrap_or_default();
            let output = self.pending_output.take().unwrap_or_default();
            let exit_code = self.pending_exit_code.take();

            if !command.is_empty() || !output.is_empty() {
                state.push_command(command, output, exit_code);
            }
        }

        // If we're in Executing and get B without D (pre-exec failure),
        // finalize with exit code 1
        if let Osc133Phase::Executing { command } = &self.phase {
            let output = std::mem::take(&mut self.capture);
            if !command.is_empty() || !output.is_empty() {
                state.push_command(command.clone(), output, Some(1));
            }
        }

        self.phase = Osc133Phase::Input;
        self.capture.clear();
        state.activity = Activity::Idle;
    }

    /// C (Command Executing): a command has started.
    /// From any state except Executing: create new Executing state.
    /// If already Executing: ignore (duplicate C).
    ///
    /// Command text priority: E (pending_command_text) > C cmdline_url > capture fallback.
    /// The capture buffer (B-to-C content) is the echoed command text; we strip
    /// escape sequences to extract it, then clear the buffer (output starts here).
    fn handle_c(&mut self, param: Option<&str>) {
        if matches!(self.phase, Osc133Phase::Executing { .. }) {
            tracing::debug!("Duplicate OSC 133;C — ignoring");
            return;
        }

        // Command text: E > C cmdline_url > B-to-C capture (escape-stripped)
        let command = self
            .pending_command_text
            .take()
            .or_else(|| param.and_then(parse_cmdline_url))
            .unwrap_or_else(|| strip_escapes(&self.capture));

        // C is the delimiter — everything after is command output.
        self.capture.clear();

        self.phase = Osc133Phase::Executing { command };
    }

    /// D (Command Done): command finished with optional exit code.
    /// Moves command data to pending state — not finalized until B arrives.
    /// Always increments completion_seq so command_run can detect completion.
    /// Consumes the capture buffer as output in all cases.
    fn handle_d(&mut self, param: Option<&str>, state: &mut PaneState) {
        let exit_code = param.and_then(|p| p.parse::<i32>().ok());
        state.completion_seq += 1;
        state.last_exit_code = exit_code;
        let output = std::mem::take(&mut self.capture);

        match &self.phase {
            Osc133Phase::Executing { command } => {
                // Normal case: C→D. Hold command data for B to finalize.
                self.pending_command = Some(command.clone());
                self.pending_output = Some(output);
                self.pending_exit_code = exit_code;
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
            Osc133Phase::Input => {
                // B→D pattern: no C marker (subshell, cd, export, etc.)
                // Use E text if available, otherwise empty.
                let command = self.pending_command_text.take().unwrap_or_default();
                self.pending_command = Some(command);
                self.pending_output = Some(output);
                self.pending_exit_code = exit_code;
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
            _ => {
                // D from unexpected state (race condition, mid-join, etc.)
                tracing::debug!(
                    "OSC 133;D from unexpected state {:?} — recovering",
                    self.phase
                );
                self.pending_command = None;
                self.pending_output = Some(output);
                self.pending_exit_code = exit_code;
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
        }
    }

    /// E (Explicit Command Text): authoritative command text from the shell.
    fn handle_e(&mut self, param: Option<&str>) {
        let Some(text) = param else { return };

        match &mut self.phase {
            Osc133Phase::Executing { command } => {
                // E during Executing: update command text (authoritative override)
                *command = text.to_string();
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
        if let Osc133Phase::Executing { command } = &self.phase {
            let output = std::mem::take(&mut self.capture);
            if !command.is_empty() || !output.is_empty() {
                // Record as incomplete (no exit code)
                tracing::debug!(
                    "Abandoning in-flight command on reset: {:?}",
                    command
                );
                state.push_command(command.clone(), output, None);
            }
        }

        // Also flush any pending D that wasn't finalized by B
        if self.pending_command.is_some() || self.pending_exit_code.is_some() {
            let command = self.pending_command.take().unwrap_or_default();
            let output = self.pending_output.take().unwrap_or_default();
            let exit_code = self.pending_exit_code.take();
            // Record if there's content, or a non-zero exit code (failures are worth tracking)
            let worth_recording = !command.is_empty()
                || !output.is_empty()
                || exit_code.is_some_and(|c| c != 0);
            if worth_recording {
                state.push_command(command, output, exit_code);
            }
        }
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

/// Strip ANSI escape sequences from raw terminal text, returning only visible characters.
/// Uses vte's parser with a minimal Perform impl that collects only printable chars.
fn strip_escapes(raw: &str) -> String {
    struct TextCollector(String);
    impl vte::Perform for TextCollector {
        fn print(&mut self, c: char) {
            self.0.push(c);
        }
    }
    let mut parser = vte::Parser::new();
    let mut collector = TextCollector(String::with_capacity(raw.len()));
    parser.advance(&mut collector, raw.as_bytes());
    collector.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shorthand: call handle_marker without the old screen_command_text closure.
    fn marker(parser: &mut Osc133Parser, m: u8, param: Option<&str>, state: &mut PaneState) {
        parser.handle_marker(m, param, state);
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

        p.append_output("file1.txt\nfile2.txt\n");

        marker(&mut p, b'D', Some("0"), &mut s);
        assert_eq!(p.phase, Osc133Phase::Idle);

        // Command not finalized yet — need B (the latch)
        assert!(s.commands.is_empty());

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        // NOW the command is finalized
        assert_eq!(s.commands.len(), 1);
        let cmd = &s.commands[0];
        assert_eq!(cmd.command, "ls -la");
        assert_eq!(cmd.output, "file1.txt\nfile2.txt\n");
        assert_eq!(cmd.exit_code, Some(0));
    }

    #[test]
    fn full_cycle_without_e_uses_capture_fallback() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        // Simulate echoed command text in capture (B-to-C content)
        p.append_output("$ echo hello");
        // C marker — no E, so capture is escape-stripped for command text
        marker(&mut p, b'C', None, &mut s);
        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "$ echo hello");
        } else {
            panic!("Expected Executing");
        }

        p.append_output("hello\n");
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "$ echo hello");
        assert_eq!(s.commands[0].output, "hello\n");
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
        p.append_output("$ wrong");
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
        p.append_output("sub\n");
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
        p.append_output("partial output\n");
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
        p.append_output("$ long-cmd");
        marker(&mut p, b'C', None, &mut s);
        p.append_output("partial output");

        // No D marker — A arrives (Ctrl+C, reset, etc.)
        marker(&mut p, b'A', None, &mut s);

        // The abandoned command is recorded with no exit code
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "$ long-cmd");
        assert_eq!(s.commands[0].output, "partial output");
        assert_eq!(s.commands[0].exit_code, None);
    }

    #[test]
    fn missing_c_pre_exec_failure() {
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
    fn b_without_d_pre_exec_failure() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        p.append_output("$ bad-syntax");
        marker(&mut p, b'C', None, &mut s);
        // A arrives without D — command abandoned
        marker(&mut p, b'A', None, &mut s);
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "$ bad-syntax");
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
        p.append_output("$ cmd");
        marker(&mut p, b'C', None, &mut s);
        p.append_output("out1");
        // Duplicate C — should be ignored, output continues
        marker(&mut p, b'C', None, &mut s);
        p.append_output("out2");

        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "$ cmd");
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

        p.append_output("some output");
        marker(&mut p, b'D', Some("0"), &mut s);
        assert_eq!(p.phase, Osc133Phase::Idle);
        assert_eq!(s.completion_seq, 1);
    }

    #[test]
    fn c_from_idle_creates_executing() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        p.append_output("$ surprise");
        marker(&mut p, b'C', None, &mut s);
        if let Osc133Phase::Executing { command } = &p.phase {
            assert_eq!(command, "$ surprise");
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
        p.append_output("partial");

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
        p.append_output("1\n");
        marker(&mut p, b'D', Some("0"), &mut s);

        // Command 2 (B finalizes command 1)
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);
        assert_eq!(s.commands.len(), 1);
        assert_eq!(s.commands[0].command, "echo 1");

        marker(&mut p, b'E', Some("echo 2"), &mut s);
        marker(&mut p, b'C', None, &mut s);
        p.append_output("2\n");
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

        assert!(s.commands.is_empty());
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

        p.append_output("file1\n");
        p.append_output("file2\n");
        p.append_output("file3\n");

        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert_eq!(s.commands[0].output, "file1\nfile2\nfile3\n");
    }

    #[test]
    fn capture_cleared_by_b() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        p.append_output("stray output");
        marker(&mut p, b'A', None, &mut s);
        p.append_output("prompt text");
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
        p.append_output("command echo text");
        marker(&mut p, b'C', None, &mut s);

        // C clears capture — post-C output starts fresh
        assert!(p.capture.is_empty());
        p.append_output("real output");
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
        p.append_output("$ echo hello");
        marker(&mut p, b'C', None, &mut s);
        p.append_output("hello\n");
        marker(&mut p, b'D', Some("0"), &mut s);
        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        assert!(
            s.commands.iter().any(|c| c.command == "$ echo hello"),
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
    fn failed_command_without_c_marker_bumps_completion_seq() {
        let mut p = Osc133Parser::new();
        let mut s = PaneState::new();

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

        marker(&mut p, b'D', Some("1"), &mut s);
        assert_eq!(s.completion_seq, 1);

        marker(&mut p, b'A', None, &mut s);
        marker(&mut p, b'B', None, &mut s);

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
        p.append_output("file1\n");

        marker(&mut p, b'D', Some("0"), &mut s);
        assert!(s.commands.is_empty(), "D should not finalize yet");

        marker(&mut p, b'A', None, &mut s);
        assert!(s.commands.is_empty(), "A should not finalize (that's B's job)");

        marker(&mut p, b'B', None, &mut s);
        assert_eq!(s.commands.len(), 1, "B should finalize the command");
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
    fn strip_escapes_plain() {
        assert_eq!(strip_escapes("hello world"), "hello world");
    }

    #[test]
    fn strip_escapes_ansi_colors() {
        assert_eq!(strip_escapes("\x1b[32mgreen\x1b[0m text"), "green text");
    }

    #[test]
    fn strip_escapes_prompt() {
        assert_eq!(strip_escapes("$ \x1b[1mecho hello\x1b[0m"), "$ echo hello");
    }

    #[test]
    fn percent_decode_plus_as_space() {
        assert_eq!(percent_decode("hello+world"), "hello world");
    }
}
