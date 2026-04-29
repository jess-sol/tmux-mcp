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
/// - E is the source of truth for command text (screen query is fallback)

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
        /// Command text (from E marker or screen query).
        command: String,
        /// Accumulated output between C and D.
        output: String,
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
}

impl Osc133Parser {
    pub fn new() -> Self {
        Self {
            phase: Osc133Phase::Idle,
            pending_command_text: None,
            pending_exit_code: None,
            pending_command: None,
            pending_output: None,
        }
    }

    /// Current phase (for external queries).
    pub fn phase(&self) -> &Osc133Phase {
        &self.phase
    }

    /// Handle an OSC 133 marker event.
    ///
    /// `marker` is the uppercase letter (A-E).
    /// `param` is the optional parameter (exit code for D, command text for E).
    /// `screen_command_text` is a closure that queries the processor's screen
    /// for the resolved command text at the current cursor line. Called only
    /// when needed (on C marker, as fallback for missing E).
    pub fn handle_marker(
        &mut self,
        marker: u8,
        param: Option<&str>,
        state: &mut PaneState,
        screen_command_text: impl FnOnce() -> String,
    ) {
        match marker {
            b'A' => self.handle_a(state),
            b'B' => self.handle_b(state),
            b'C' => self.handle_c(screen_command_text),
            b'D' => self.handle_d(param, state),
            b'E' => self.handle_e(param),
            _ => tracing::debug!("Unknown OSC 133 marker: {}", marker as char),
        }
    }

    /// Append raw output text (bytes fed to terminal between OSC events).
    /// Called by the processor for bytes between C and D boundaries.
    pub fn append_output(&mut self, text: &str) {
        if let Osc133Phase::Executing { output, .. } = &mut self.phase {
            output.push_str(text);
        }
    }

    /// Force reset to Idle (for mode switches or external reset).
    pub fn reset(&mut self, state: &mut PaneState) {
        self.abandon_in_flight(state);
        self.phase = Osc133Phase::Idle;
        self.pending_command_text = None;
        self.pending_exit_code = None;
        self.pending_command = None;
        self.pending_output = None;
        state.activity = Activity::Unknown;
    }

    // --- Marker Handlers ---

    /// A (Prompt Start): hard reset from any state.
    /// Shell has returned to prompt — any in-flight command is abandoned.
    fn handle_a(&mut self, state: &mut PaneState) {
        // If we were Executing without a D marker, the command is lost.
        // This happens when: Ctrl+C during output, reset, etc.
        self.abandon_in_flight(state);
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
        if let Osc133Phase::Executing { command, output } = &self.phase {
            if !command.is_empty() || !output.is_empty() {
                state.push_command(command.clone(), output.clone(), Some(1));
            }
        }

        self.phase = Osc133Phase::Input;
        state.activity = Activity::Idle;
    }

    /// C (Command Executing): a command has started.
    /// From any state except Executing: create new Executing state.
    /// If already Executing: ignore (duplicate C).
    fn handle_c(&mut self, screen_command_text: impl FnOnce() -> String) {
        if matches!(self.phase, Osc133Phase::Executing { .. }) {
            // Duplicate C — ignore
            tracing::debug!("Duplicate OSC 133;C — ignoring");
            return;
        }

        // Get command text: prefer buffered E text, fall back to screen query
        let command = self
            .pending_command_text
            .take()
            .unwrap_or_else(screen_command_text);

        self.phase = Osc133Phase::Executing {
            command,
            output: String::new(),
        };
    }

    /// D (Command Done): command finished with optional exit code.
    /// Moves command data to pending state — not finalized until B arrives.
    fn handle_d(&mut self, param: Option<&str>, state: &mut PaneState) {
        let exit_code = param.and_then(|p| p.parse::<i32>().ok());

        match &self.phase {
            Osc133Phase::Executing { command, output } => {
                // Normal case: C→D. Hold command data for B to finalize.
                self.pending_command = Some(command.clone());
                self.pending_output = Some(output.clone());
                self.pending_exit_code = exit_code;
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
            Osc133Phase::Input => {
                // B→D pattern: command with no output (cd, export, etc.)
                // Use E text if available, otherwise empty
                let command = self.pending_command_text.take().unwrap_or_default();
                self.pending_command = Some(command);
                self.pending_output = Some(String::new());
                self.pending_exit_code = exit_code;
                self.phase = Osc133Phase::Idle;
                state.activity = Activity::Idle;
            }
            _ => {
                // D from unexpected state (subshell, race condition, etc.)
                // Record exit code, accept lost command text.
                tracing::debug!(
                    "OSC 133;D from unexpected state {:?} — recovering",
                    self.phase
                );
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
            Osc133Phase::Executing { command, .. } => {
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
        if let Osc133Phase::Executing { command, output } = &self.phase {
            if !command.is_empty() || !output.is_empty() {
                // Record as incomplete (no exit code)
                tracing::debug!(
                    "Abandoning in-flight command on reset: {:?}",
                    command
                );
                state.push_command(command.clone(), output.clone(), None);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn noop_screen() -> String {
        String::new()
    }

    fn screen(text: &str) -> impl FnOnce() -> String + '_ {
        move || text.to_string()
    }

    // --- Normal flows ---

    #[test]
    fn full_cycle_with_e_marker() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        assert_eq!(parser.phase, Osc133Phase::Prompt);

        parser.handle_marker(b'B', None, &mut state, noop_screen);
        assert_eq!(parser.phase, Osc133Phase::Input);

        parser.handle_marker(b'C', None, &mut state, noop_screen);
        parser.handle_marker(b'E', Some("ls -la"), &mut state, noop_screen);
        assert!(matches!(parser.phase, Osc133Phase::Executing { .. }));

        // Accumulate some output
        parser.append_output("file1.txt\nfile2.txt\n");

        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);
        assert_eq!(parser.phase, Osc133Phase::Idle);

        // Command not finalized yet — need B (the latch)
        assert!(state.commands.is_empty());

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        // NOW the command is finalized
        assert_eq!(state.commands.len(), 1);
        let cmd = &state.commands[0];
        assert_eq!(cmd.command, "ls -la");
        assert_eq!(cmd.output, "file1.txt\nfile2.txt\n");
        assert_eq!(cmd.exit_code, Some(0));
    }

    #[test]
    fn full_cycle_without_e_uses_screen() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        // C marker — no E, so screen query provides command text
        parser.handle_marker(b'C', None, &mut state, screen("$ echo hello"));
        if let Osc133Phase::Executing { command, .. } = &parser.phase {
            assert_eq!(command, "$ echo hello");
        } else {
            panic!("Expected Executing");
        }

        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        assert_eq!(state.commands.len(), 1);
        assert_eq!(state.commands[0].command, "$ echo hello");
    }

    #[test]
    fn e_before_c() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        // E arrives before C (some shells do this)
        parser.handle_marker(b'E', Some("pwd"), &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, noop_screen);

        if let Osc133Phase::Executing { command, .. } = &parser.phase {
            assert_eq!(command, "pwd");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn e_overrides_screen_query() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        // C gets screen text initially
        parser.handle_marker(b'C', None, &mut state, screen("$ wrong"));
        // E arrives and overrides
        parser.handle_marker(b'E', Some("correct"), &mut state, noop_screen);

        if let Osc133Phase::Executing { command, .. } = &parser.phase {
            assert_eq!(command, "correct");
        } else {
            panic!("Expected Executing");
        }
    }

    // --- No-output commands ---

    #[test]
    fn no_output_command_bd_pattern() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'E', Some("cd /tmp"), &mut state, noop_screen);
        // B→D pattern: D without C (cd has no output phase)
        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        assert_eq!(state.commands.len(), 1);
        assert_eq!(state.commands[0].command, "cd /tmp");
        assert_eq!(state.commands[0].exit_code, Some(0));
        assert!(state.commands[0].output.is_empty());
    }

    // --- Recovery: missing markers ---

    #[test]
    fn missing_d_abandoned_on_a() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, screen("$ long-cmd"));
        parser.append_output("partial output");

        // No D marker — A arrives (Ctrl+C, reset, etc.)
        parser.handle_marker(b'A', None, &mut state, noop_screen);

        // The abandoned command is recorded with no exit code
        assert_eq!(state.commands.len(), 1);
        assert_eq!(state.commands[0].command, "$ long-cmd");
        assert_eq!(state.commands[0].exit_code, None); // abandoned
    }

    #[test]
    fn missing_c_pre_exec_failure() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        // D without C — syntax error, history expansion failure, etc.
        // D creates a pending command (B-Latch holds it)
        parser.handle_marker(b'D', Some("1"), &mut state, noop_screen);
        assert!(state.commands.is_empty()); // not finalized yet

        // A flushes the pending command (abandon_in_flight)
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        assert_eq!(state.commands.len(), 1);
        assert_eq!(state.commands[0].exit_code, Some(1));

        parser.handle_marker(b'B', None, &mut state, noop_screen);
        assert_eq!(state.commands.len(), 1); // no new command
    }

    #[test]
    fn b_without_d_pre_exec_failure() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, screen("$ bad-syntax"));
        // A arrives without D — command abandoned (recorded with no exit code)
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        assert_eq!(state.commands.len(), 1);
        assert_eq!(state.commands[0].command, "$ bad-syntax");
        assert_eq!(state.commands[0].exit_code, None); // abandoned, no D seen

        // B arrives — no pending command from D, so just transitions to Input
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        assert_eq!(state.commands.len(), 1); // no new command added
    }

    // --- Recovery: duplicate markers ---

    #[test]
    fn duplicate_c_ignored() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, screen("$ cmd"));
        parser.append_output("out1");
        // Duplicate C — should be ignored
        parser.handle_marker(b'C', None, &mut state, screen("$ wrong"));
        parser.append_output("out2");

        if let Osc133Phase::Executing { command, output } = &parser.phase {
            assert_eq!(command, "$ cmd");
            assert_eq!(output, "out1out2"); // output continues accumulating
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn duplicate_a_resets_cleanly() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        assert_eq!(parser.phase, Osc133Phase::Prompt);
    }

    #[test]
    fn duplicate_b_resets_to_input() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        assert_eq!(parser.phase, Osc133Phase::Input);
    }

    // --- Recovery: unexpected states ---

    #[test]
    fn d_from_idle_ignored() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);
        // Should not panic or corrupt state
        assert_eq!(parser.phase, Osc133Phase::Idle);
        assert!(state.commands.is_empty()); // nothing to finalize yet
    }

    #[test]
    fn c_from_idle_creates_executing() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        // C without prior A/B (e.g., we started monitoring mid-session)
        parser.handle_marker(b'C', None, &mut state, screen("$ surprise"));
        assert!(matches!(parser.phase, Osc133Phase::Executing { .. }));
    }

    #[test]
    fn reset_clears_everything() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, screen("$ cmd"));
        parser.append_output("partial");

        parser.reset(&mut state);

        assert_eq!(parser.phase, Osc133Phase::Idle);
        assert_eq!(state.activity, Activity::Unknown);
        // Abandoned command is recorded
        assert_eq!(state.commands.len(), 1);
    }

    // --- Activity tracking ---

    #[test]
    fn activity_transitions() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        assert_eq!(state.activity, Activity::Unknown);

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        assert_eq!(state.activity, Activity::Idle);

        parser.handle_marker(b'B', None, &mut state, noop_screen);
        assert_eq!(state.activity, Activity::Idle);

        parser.handle_marker(b'C', None, &mut state, noop_screen);
        // C doesn't set activity — the processor should set Busy
        // when it starts feeding output bytes

        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);
        assert_eq!(state.activity, Activity::Idle);
    }

    // --- Rapid commands ---

    #[test]
    fn rapid_back_to_back_commands() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        // Command 1
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'E', Some("echo 1"), &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, noop_screen);
        parser.append_output("1\n");
        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);

        // Command 2 (B finalizes command 1)
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        assert_eq!(state.commands.len(), 1);
        assert_eq!(state.commands[0].command, "echo 1");

        parser.handle_marker(b'E', Some("echo 2"), &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, noop_screen);
        parser.append_output("2\n");
        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);

        // Command 3 (B finalizes command 2)
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        assert_eq!(state.commands.len(), 2);
        assert_eq!(state.commands[0].command, "echo 2");
        assert_eq!(state.commands[1].command, "echo 1");
    }

    // --- Empty command ---

    #[test]
    fn empty_command_just_enter() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'E', Some(""), &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, noop_screen);
        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        // Empty commands are not recorded
        assert!(state.commands.is_empty());
    }

    // --- Error exit codes ---

    #[test]
    fn nonzero_exit_code() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'E', Some("false"), &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, noop_screen);
        parser.handle_marker(b'D', Some("1"), &mut state, noop_screen);
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        assert_eq!(state.commands.len(), 1);
        assert_eq!(state.commands[0].exit_code, Some(1));
    }

    #[test]
    fn signal_exit_code() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'E', Some("sleep 100"), &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, noop_screen);
        parser.handle_marker(b'D', Some("130"), &mut state, noop_screen);
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        assert_eq!(state.commands[0].exit_code, Some(130)); // SIGINT
    }

    // --- Output accumulation ---

    #[test]
    fn output_accumulates_between_c_and_d() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);
        parser.handle_marker(b'E', Some("ls"), &mut state, noop_screen);
        parser.handle_marker(b'C', None, &mut state, noop_screen);

        parser.append_output("file1\n");
        parser.append_output("file2\n");
        parser.append_output("file3\n");

        parser.handle_marker(b'D', Some("0"), &mut state, noop_screen);
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.handle_marker(b'B', None, &mut state, noop_screen);

        assert_eq!(state.commands[0].output, "file1\nfile2\nfile3\n");
    }

    #[test]
    fn output_ignored_outside_executing() {
        let mut parser = Osc133Parser::new();
        let mut state = PaneState::new();

        // Output before C — should be silently ignored
        parser.append_output("stray output");
        parser.handle_marker(b'A', None, &mut state, noop_screen);
        parser.append_output("prompt text");
        // No crash, no corruption
    }
}
