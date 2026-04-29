/// Pane processor: owns a headless alacritty terminal instance and
/// processes byte chunks with OSC boundary splitting for synchronized
/// metadata extraction. Integrates the OSC 133 state machine for
/// structured command tracking.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Config;
use alacritty_terminal::Term;
use vte::ansi::Processor;

use crate::pane::osc::{OscEvent, OscMatch, find_next_osc_from};
use crate::pane::osc133::{Osc133Parser, Osc133Phase};
use crate::pane::state::PaneState;

// --- Terminal dimensions ---

struct PaneDimensions {
    lines: usize,
    columns: usize,
}

impl Dimensions for PaneDimensions {
    fn screen_lines(&self) -> usize {
        self.lines
    }

    fn columns(&self) -> usize {
        self.columns
    }

    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
}

// --- Pane Processor ---

pub struct PaneProcessor {
    term: Term<VoidListener>,
    processor: Processor,
    osc133: Osc133Parser,
    state: PaneState,
}

impl PaneProcessor {
    /// Create a new pane processor with the given dimensions.
    pub fn new(lines: usize, columns: usize) -> Self {
        let dims = PaneDimensions { lines, columns };
        let config = Config::default();
        let term = Term::new(config, &dims, VoidListener);
        let processor = Processor::new();
        Self {
            term,
            processor,
            osc133: Osc133Parser::new(),
            state: PaneState::new(),
        }
    }

    /// Process a chunk of raw terminal bytes (already unescaped from tmux octal).
    ///
    /// Splits the chunk at OSC 133/7/9 boundaries, feeding each segment
    /// to the terminal emulator before handling the OSC event. This
    /// guarantees the screen model is at exactly the right state when
    /// we process each marker.
    ///
    /// Returns the OSC events found in this chunk, in order.
    pub fn process_chunk(&mut self, bytes: &[u8]) -> Vec<OscMatch> {
        self.state.last_data = Some(std::time::Instant::now());
        let mut events = Vec::new();
        let mut pos = 0;

        while pos < bytes.len() {
            match find_next_osc_from(bytes, pos) {
                Some(osc_match) => {
                    // Feed bytes before the OSC to the terminal emulator
                    let segment = &bytes[pos..osc_match.start];
                    if !segment.is_empty() {
                        self.processor.advance(&mut self.term, segment);
                        // If we're executing, this is command output
                        if let Ok(text) = std::str::from_utf8(segment) {
                            self.osc133.append_output(text, &mut self.state);
                        }
                    }

                    // Handle the OSC event (screen is synchronized)
                    self.handle_osc_event(&osc_match.event);

                    events.push(osc_match.clone());
                    pos = osc_match.end;
                }
                None => {
                    // No more OSC events — feed remaining bytes to terminal
                    let segment = &bytes[pos..];
                    if !segment.is_empty() {
                        self.processor.advance(&mut self.term, segment);
                        if let Ok(text) = std::str::from_utf8(segment) {
                            self.osc133.append_output(text, &mut self.state);
                        }
                    }
                    pos = bytes.len();
                }
            }
        }

        events
    }

    /// Access the structured pane state (command history, activity, cwd).
    pub fn state(&self) -> &PaneState {
        &self.state
    }

    /// Mutable access to pane state (for OSC 133 cache updates).
    pub fn state_mut(&mut self) -> &mut PaneState {
        &mut self.state
    }

    /// Access the OSC 133 parser state.
    pub fn osc133_phase(&self) -> &Osc133Phase {
        self.osc133.phase()
    }

    /// Read user input from the terminal grid, starting from the cursor
    /// position snapshotted at the last B marker (input_start).
    ///
    /// Reads from input_start forward to end of content (last non-space cell),
    /// across wrapped lines. Returns empty string if no input_start is set or
    /// no content has been typed.
    pub fn read_input(&self) -> String {
        let Some((start_line, start_col)) = self.state.input_start else {
            return String::new();
        };

        let mut text = String::new();
        for line_idx in start_line..self.screen_lines() {
            let line_text = self.screen_line_text(line_idx);
            if line_idx == start_line {
                // First line: skip prompt (everything before start_col).
                // chars().skip() works here because screen_line_text() returns one char
                // per terminal column (wide char spacer cells become ' ').
                let input: String = line_text.chars().skip(start_col).collect();
                text.push_str(&input);
            } else {
                if line_text.is_empty() {
                    break;
                }
                text.push_str(&line_text);
            }
        }
        text.trim().to_string()
    }

    /// Check whether the user appears to be typing at the prompt.
    /// Returns true when input_start is set (B marker seen) and
    /// the terminal grid has non-empty content after the prompt.
    pub fn has_input_content(&self) -> bool {
        self.state.input_start.is_some() && !self.read_input().is_empty()
    }

    /// Resize the headless terminal to new dimensions.
    /// Called when tmux reports changed pane dimensions via `%layout-change`.
    pub fn resize(&mut self, lines: usize, columns: usize) {
        let dims = PaneDimensions { lines, columns };
        self.term.resize(dims);
    }

    // --- Screen queries ---

    /// Read text from a specific line of the screen.
    /// Line 0 is the topmost visible line.
    pub fn screen_line_text(&self, line: usize) -> String {
        let grid = self.term.grid();
        if line >= grid.screen_lines() {
            return String::new();
        }

        let row = &grid[alacritty_terminal::index::Line(line as i32)];
        let mut text = String::new();
        for cell in row {
            text.push(cell.c);
        }
        text.trim_end().to_string()
    }

    /// Read text from the line where the cursor currently sits.
    pub fn cursor_line_text(&self) -> String {
        let cursor_line = self.term.grid().cursor.point.line.0 as usize;
        self.screen_line_text(cursor_line)
    }

    /// Get the cursor position as (line, column).
    pub fn cursor_position(&self) -> (usize, usize) {
        let point = self.term.grid().cursor.point;
        (point.line.0 as usize, point.column.0)
    }

    /// Get the total number of visible screen lines.
    pub fn screen_lines(&self) -> usize {
        self.term.grid().screen_lines()
    }

    /// Get the number of columns.
    pub fn columns(&self) -> usize {
        self.term.grid().columns()
    }

    /// Read all visible screen text as a vector of lines.
    pub fn screen_text(&self) -> Vec<String> {
        (0..self.screen_lines())
            .map(|line| self.screen_line_text(line))
            .collect()
    }

    // --- Internal ---

    fn handle_osc_event(&mut self, event: &OscEvent) {
        match event {
            OscEvent::Osc133 { marker, param } => {
                // For C marker, read command text from the terminal grid
                // before the parser clears state. This replaces strip_escapes.
                let screen_input = if *marker == b'C' {
                    let input = self.read_input();
                    if input.is_empty() { None } else { Some(input) }
                } else {
                    None
                };

                self.osc133.handle_marker(
                    *marker,
                    param.as_deref(),
                    &mut self.state,
                    screen_input.as_deref(),
                );

                // Snapshot cursor position at B marker — marks the start of
                // user input (right after the prompt).
                if *marker == b'B' {
                    self.state.input_start = Some(self.cursor_position());
                }

                // Clear input_start when leaving Input phase (command started
                // or prompt reset).
                if *marker == b'C' || *marker == b'A' {
                    self.state.input_start = None;
                }

                if matches!(self.osc133.phase(), Osc133Phase::Executing { .. }) {
                    self.state.activity = crate::pane::state::Activity::Busy;
                }
            }
            OscEvent::Osc7 { uri } => {
                self.state.update_cwd_from_osc7(uri);
            }
            OscEvent::Osc9 { text } => {
                tracing::debug!("OSC 9 notification: {}", text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Dependency contract: alacritty_terminal behavior we rely on ---

    #[test]
    fn empty_processor() {
        let p = PaneProcessor::new(24, 80);
        assert_eq!(p.screen_lines(), 24);
        assert_eq!(p.columns(), 80);
        assert_eq!(p.cursor_position(), (0, 0));
    }

    #[test]
    fn resize_updates_dimensions() {
        let mut p = PaneProcessor::new(24, 80);
        assert_eq!(p.screen_lines(), 24);
        assert_eq!(p.columns(), 80);
        p.resize(40, 120);
        assert_eq!(p.screen_lines(), 40);
        assert_eq!(p.columns(), 120);
    }

    #[test]
    fn simple_text() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"hello world");
        assert_eq!(p.screen_line_text(0), "hello world");
        assert_eq!(p.cursor_position(), (0, 11));
    }

    #[test]
    fn text_with_newline() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"line1\r\nline2");
        assert_eq!(p.screen_line_text(0), "line1");
        assert_eq!(p.screen_line_text(1), "line2");
    }

    #[test]
    fn backspace_editing() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"abcd\x08\x08ef");
        assert_eq!(p.screen_line_text(0), "abef");
    }

    #[test]
    fn cursor_movement_editing() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"hello\x1b[3DX");
        assert_eq!(p.screen_line_text(0), "heXlo");
    }

    #[test]
    fn ansi_colors_dont_affect_text() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b[31mred\x1b[0m normal");
        assert_eq!(p.screen_line_text(0), "red normal");
    }

    // --- OSC boundary splitting ---

    #[test]
    fn osc133_events_extracted() {
        let mut p = PaneProcessor::new(24, 80);
        let events = p.process_chunk(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, OscEvent::Osc133 { marker: b'A', param: None });
        assert_eq!(events[1].event, OscEvent::Osc133 { marker: b'B', param: None });
    }

    #[test]
    fn osc133_c_captures_command_text() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let events = p.process_chunk(b"ls -la\x1b]133;C\x07");
        assert_eq!(p.screen_line_text(0), "$ ls -la");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, OscEvent::Osc133 { marker: b'C', param: None });
    }

    #[test]
    fn osc133_c_with_readline_editing() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let events = p.process_chunk(b"shwo\x08\x08ow\x1b]133;C\x07");
        assert_eq!(p.screen_line_text(0), "$ show");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn osc7_extracted() {
        let mut p = PaneProcessor::new(24, 80);
        let events = p.process_chunk(b"\x1b]7;file://myhost/home/user\x07");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event,
            OscEvent::Osc7 { uri: "file://myhost/home/user".to_string() }
        );
        // State should be updated
        assert_eq!(p.state().cwd.as_deref(), Some("/home/user"));
        assert_eq!(p.state().hostname.as_deref(), Some("myhost"));
    }

    #[test]
    fn non_intercepted_osc_passes_through() {
        let mut p = PaneProcessor::new(24, 80);
        let events = p.process_chunk(b"\x1b]0;My Title\x07hello");
        assert!(events.is_empty());
        assert_eq!(p.screen_line_text(0), "hello");
    }

    #[test]
    fn mixed_text_and_osc() {
        let mut p = PaneProcessor::new(24, 80);
        let events = p.process_chunk(b"before\x1b]133;C\x07after\r\nline2");
        assert_eq!(events.len(), 1);
        assert_eq!(p.screen_line_text(0), "beforeafter");
        assert_eq!(p.screen_line_text(1), "line2");
    }

    // --- Command text sources and end-to-end cycles ---

    /// C with cmdline_url parameter — command text from URL-encoded param.
    #[test]
    fn c_cmdline_url_provides_command_text() {
        let mut p = PaneProcessor::new(24, 80);

        // Prompt
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        // C with cmdline_url (no E)
        p.process_chunk(b"echo hi\x1b]133;C;cmdline_url=echo%20hi\x1b\\");
        assert!(matches!(p.osc133_phase(), Osc133Phase::Executing { .. }));

        // Output + D + next prompt
        p.process_chunk(b"\r\nhi\r\n\x1b]133;D;0\x1b\\");
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        let cmds = p.state().recent_commands(10);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "echo hi");
        assert!(cmds[0].output.contains("hi"));
    }

    /// E supersedes C cmdline_url when both present.
    #[test]
    fn e_supersedes_c_cmdline_url() {
        let mut p = PaneProcessor::new(24, 80);

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        // E arrives first, then C with cmdline_url — E wins
        p.process_chunk(
            b"echo correct\x1b]133;E;echo correct\x1b\\\x1b]133;C;cmdline_url=wrong\x1b\\"
        );
        p.process_chunk(b"\r\ncorrect\r\n\x1b]133;D;0\x1b\\");
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        let cmds = p.state().recent_commands(10);
        assert_eq!(cmds[0].command, "echo correct");
    }

    /// No C, no E — just D/A/B. Output captured from buffer.
    #[test]
    fn no_c_no_e_output_from_capture_buffer() {
        let mut p = PaneProcessor::new(24, 80);

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        // Command output arrives without C or E
        p.process_chunk(b"sub output\r\n");
        p.process_chunk(b"\x1b]133;D;0\x1b\\");
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        let cmds = p.state().recent_commands(10);
        assert_eq!(cmds.len(), 1);
        assert!(cmds[0].output.contains("sub output"));
        assert_eq!(cmds[0].exit_code, Some(0));
    }

    /// C with no param, no E — screen read provides command text, ANSI escapes
    /// are naturally absent because the terminal renders them as attributes.
    #[test]
    fn screen_read_fallback_has_no_escapes() {
        let mut p = PaneProcessor::new(24, 80);

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        // Echoed text with ANSI colors before C
        p.process_chunk(b"\x1b[1mecho hello\x1b[0m\x1b]133;C\x1b\\");

        if let Osc133Phase::Executing { command } = p.osc133_phase() {
            assert!(command.contains("echo hello"), "command: {:?}", command);
            assert!(!command.contains("\x1b"), "should not contain raw escapes: {:?}", command);
        } else {
            panic!("Expected Executing");
        }
    }

    /// OSC 7 mid-command doesn't break capture.
    #[test]
    fn osc7_mid_command_doesnt_break_capture() {
        let mut p = PaneProcessor::new(24, 80);

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"\x1b]133;E;cd /tmp && ls\x1b\\\x1b]133;C\x1b\\");

        // Output, then OSC 7 (cwd update), then more output
        p.process_chunk(b"file1\r\n\x1b]7;file://localhost/tmp\x1b\\file2\r\n");
        p.process_chunk(b"\x1b]133;D;0\x1b\\");
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        let cmds = p.state().recent_commands(10);
        assert_eq!(cmds[0].command, "cd /tmp && ls");
        assert!(cmds[0].output.contains("file1"));
        assert!(cmds[0].output.contains("file2"));
        assert_eq!(p.state().cwd.as_deref(), Some("/tmp"));
    }

    /// Large output spanning multiple process_chunk calls.
    #[test]
    fn large_output_across_multiple_chunks() {
        let mut p = PaneProcessor::new(24, 80);

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"\x1b]133;E;seq\x1b\\\x1b]133;C\x1b\\");

        // Simulate many separate chunks of output
        for i in 1..=100 {
            let line = format!("{}\r\n", i);
            p.process_chunk(line.as_bytes());
        }

        p.process_chunk(b"\x1b]133;D;0\x1b\\");
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        let cmds = p.state().recent_commands(10);
        assert!(cmds[0].output.contains("1\r\n"));
        assert!(cmds[0].output.contains("100\r\n"));
    }

    /// Non-133 OSC (like OSC 9 notification) in output doesn't corrupt capture.
    #[test]
    fn osc9_in_output_doesnt_corrupt_capture() {
        let mut p = PaneProcessor::new(24, 80);

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"\x1b]133;E;cmd\x1b\\\x1b]133;C\x1b\\");

        // Output with embedded OSC 9 notification
        p.process_chunk(b"before\x1b]9;alert\x07after\r\n");
        p.process_chunk(b"\x1b]133;D;0\x1b\\");
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        let cmds = p.state().recent_commands(10);
        assert!(cmds[0].output.contains("before"));
        assert!(cmds[0].output.contains("after"));
    }

    #[test]
    fn full_command_cycle_with_state() {
        let mut p = PaneProcessor::new(24, 80);

        // D;0 from previous command + A + OSC7 + prompt text + B
        p.process_chunk(
            b"\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\\x1b]7;file://localhost/home/user\x1b\\$ \x1b]133;B\x1b\\"
        );

        // User types command + C + E
        p.process_chunk(b"echo hello\x1b]133;C\x1b\\\x1b]133;E;echo hello\x1b\\");
        assert_eq!(p.screen_line_text(0), "$ echo hello");
        assert!(matches!(p.osc133_phase(), Osc133Phase::Executing { .. }));

        // Command output + D
        p.process_chunk(b"\r\nhello\r\n\x1b]133;D;0\x1b\\");
        assert_eq!(p.screen_line_text(1), "hello");

        // Next prompt cycle (B finalizes the command)
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        // Command should be in history now
        let cmds = p.state().recent_commands(10);
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].command, "echo hello");
        assert_eq!(cmds[0].exit_code, Some(0));
        assert_eq!(p.state().cwd.as_deref(), Some("/home/user"));
    }

    // --- read_input / has_input_content ---

    #[test]
    fn read_input_empty_before_b_marker() {
        let p = PaneProcessor::new(24, 80);
        assert_eq!(p.read_input(), "");
        assert!(!p.has_input_content());
    }

    #[test]
    fn read_input_empty_at_prompt_with_no_typing() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        assert_eq!(p.read_input(), "");
        assert!(!p.has_input_content());
    }

    #[test]
    fn read_input_detects_typed_text() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"ls -la");
        assert_eq!(p.read_input(), "ls -la");
        assert!(p.has_input_content());
    }

    #[test]
    fn read_input_handles_backspace() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        // Type "lss", backspace (BS+space+BS erases 's'), then " -la"
        p.process_chunk(b"lss\x08 \x08 -la");
        assert_eq!(p.read_input(), "ls -la");
        assert!(p.has_input_content());
    }

    #[test]
    fn read_input_empty_after_full_backspace() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        // Type "hi", then backspace twice (BS+space+BS × 2)
        p.process_chunk(b"hi\x08 \x08\x08 \x08");
        assert_eq!(p.read_input(), "");
        assert!(!p.has_input_content());
    }

    #[test]
    fn read_input_cleared_after_c_marker() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo hello");
        assert!(p.has_input_content());

        // C marker — command starts executing, input_start is cleared
        p.process_chunk(b"\x1b]133;C\x1b\\");
        assert!(!p.has_input_content());
        assert!(p.state().input_start.is_none());
    }

    #[test]
    fn read_input_cleared_after_a_marker() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"some text");
        assert!(p.has_input_content());

        // A marker — prompt reset
        p.process_chunk(b"\x1b]133;A\x1b\\");
        assert!(!p.has_input_content());
    }

    #[test]
    fn read_input_provides_command_text_at_c() {
        let mut p = PaneProcessor::new(24, 80);

        // Full cycle: prompt → type → execute
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo hello\x1b]133;C\x1b\\");

        // Command text should come from screen read
        if let Osc133Phase::Executing { command } = p.osc133_phase() {
            assert_eq!(command, "echo hello");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn input_start_set_at_b_marker() {
        let mut p = PaneProcessor::new(24, 80);
        assert!(p.state().input_start.is_none());

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        // Cursor should be at (0, 2) — after "$ "
        assert_eq!(p.state().input_start, Some((0, 2)));
    }

    // --- read_input edge cases: cursor movement ---

    #[test]
    fn read_input_after_cursor_home() {
        // Ctrl+A / Home: cursor moves to start of input, text still on screen
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo hello");
        // Move cursor to column 2 (start of input, after prompt "$ ")
        // CSI <col>G = cursor horizontal absolute (1-indexed)
        p.process_chunk(b"\x1b[3G");
        assert_eq!(p.cursor_position(), (0, 2));
        // Text is still on screen — read_input reads content, not cursor position
        assert_eq!(p.read_input(), "echo hello");
        assert!(p.has_input_content());
    }

    #[test]
    fn read_input_after_cursor_mid_command() {
        // Arrow left into middle of command
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo hello");
        // Move cursor back 5 positions (into "hello")
        p.process_chunk(b"\x1b[5D");
        assert_eq!(p.cursor_position(), (0, 7));
        assert_eq!(p.read_input(), "echo hello");
    }

    #[test]
    fn read_input_after_insert_in_middle() {
        // Type "hllo", move back 3, type "e" + redraw rest
        // Simulates readline insert: overwrite from cursor, reposition
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"hllo");
        // Move cursor back 3 (to between 'h' and first 'l')
        p.process_chunk(b"\x1b[3D");
        // Readline redraws: "ello" from cursor position, then moves back 3
        p.process_chunk(b"ello\x1b[3D");
        assert_eq!(p.read_input(), "hello");
    }

    // --- read_input edge cases: line editing ---

    #[test]
    fn read_input_after_ctrl_u_kill_line() {
        // Ctrl+U: move to start, erase to end of line, redraw prompt
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"hello world");
        // Simulate Ctrl+U: cursor to start of input, erase to end
        p.process_chunk(b"\x1b[3G\x1b[K");
        assert_eq!(p.read_input(), "");
        assert!(!p.has_input_content());
    }

    #[test]
    fn read_input_after_ctrl_k_kill_to_end() {
        // Ctrl+K: erase from cursor to end of line
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo hello world");
        // Move cursor back to after "echo " (5 chars), then erase to end
        p.process_chunk(b"\x1b[11D\x1b[K");
        assert_eq!(p.read_input(), "echo");
    }

    // --- read_input edge cases: wrapping ---

    #[test]
    fn read_input_wrapped_command() {
        // Narrow terminal: 20 cols. Prompt "$ " = 2 chars, 18 chars per line.
        let mut p = PaneProcessor::new(24, 20);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        assert_eq!(p.state().input_start, Some((0, 2)));

        // Type a command that wraps: 25 chars total
        p.process_chunk(b"echo this-is-a-long-cmd-x");
        // First line: "$ echo this-is-a-lo" (2 prompt + 18 chars)
        // Second line: "ng-cmd-x" (remaining 7 chars)
        let input = p.read_input();
        assert!(
            input.contains("echo this-is-a-long-cmd-x"),
            "wrapped command should be fully readable: {:?}",
            input
        );
        assert!(p.has_input_content());
    }

    #[test]
    fn read_input_wrapped_then_erased() {
        // Narrow terminal, type long command, then erase all input.
        // Simulates what readline does for Ctrl+U on a wrapped line:
        // redraw prompt on current cursor line, erase from prompt to end,
        // erase the wrapped continuation line.
        let mut p = PaneProcessor::new(24, 20);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo this-is-a-long-cmd-x");

        // Move cursor to line 0, col 2 (after prompt), erase to end of line,
        // then move to line 1, erase entire line, then back to line 0 col 2.
        p.process_chunk(b"\x1b[1;3H\x1b[K\x1b[2;1H\x1b[2K\x1b[1;3H");
        assert_eq!(p.read_input(), "");
        assert!(!p.has_input_content());
    }

    // --- read_input edge cases: multiline prompt ---

    #[test]
    fn read_input_with_multiline_prompt() {
        // Two-line prompt: "user@host\n$ "
        let mut p = PaneProcessor::new(24, 80);
        // Line 0: prompt part 1 + A marker
        p.process_chunk(b"\x1b]133;A\x1b\\user@host\r\n$ \x1b]133;B\x1b\\");
        // input_start should be on line 1, column 2
        assert_eq!(p.state().input_start, Some((1, 2)));

        p.process_chunk(b"ls -la");
        assert_eq!(p.read_input(), "ls -la");
        assert!(p.has_input_content());
    }

    #[test]
    fn read_input_multiline_prompt_no_typing() {
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\user@host\r\n$ \x1b]133;B\x1b\\");
        assert_eq!(p.read_input(), "");
        assert!(!p.has_input_content());
    }

    // --- read_input: command text at C marker ---

    #[test]
    fn screen_read_command_text_with_editing() {
        // Type with backspace editing, verify C marker gets correct command
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        // Type "ecoh", backspace twice (BS+space+BS), type "ho"
        p.process_chunk(b"ecoh\x08 \x08\x08 \x08ho");
        assert_eq!(p.read_input(), "echo");

        // C marker — command text should come from screen read
        p.process_chunk(b"\x1b]133;C\x1b\\");
        if let Osc133Phase::Executing { command } = p.osc133_phase() {
            assert_eq!(command, "echo");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn screen_read_command_text_with_cursor_home_then_enter() {
        // Type "echo hello", Ctrl+A (cursor home), then Enter
        // Cursor is at start of input when C fires, but text is still on screen
        let mut p = PaneProcessor::new(24, 80);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo hello");
        // Ctrl+A: cursor to column 2 (after prompt)
        p.process_chunk(b"\x1b[3G");
        assert_eq!(p.cursor_position(), (0, 2));
        // Enter triggers C marker — command text should still be "echo hello"
        p.process_chunk(b"\x1b]133;C\x1b\\");
        if let Osc133Phase::Executing { command } = p.osc133_phase() {
            assert_eq!(command, "echo hello");
        } else {
            panic!("Expected Executing");
        }
    }

    #[test]
    fn screen_read_wrapped_command_at_c_marker() {
        // Narrow terminal, long command, verify C gets full text
        let mut p = PaneProcessor::new(24, 20);
        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");
        p.process_chunk(b"echo long-command-here");
        // C marker
        p.process_chunk(b"\x1b]133;C\x1b\\");
        if let Osc133Phase::Executing { command } = p.osc133_phase() {
            assert!(
                command.contains("echo long-command-here"),
                "wrapped command text: {:?}",
                command
            );
        } else {
            panic!("Expected Executing");
        }
    }

}
