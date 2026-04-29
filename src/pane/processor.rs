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
                self.osc133.handle_marker(
                    *marker,
                    param.as_deref(),
                    &mut self.state,
                );
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

    // --- OSC 133 implementation variants ---

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

    /// C with no param, no E — capture fallback strips escapes from B-to-C content.
    #[test]
    fn capture_fallback_strips_ansi_escapes() {
        let mut p = PaneProcessor::new(24, 80);

        p.process_chunk(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\");

        // Echoed text with ANSI colors before C
        p.process_chunk(b"\x1b[1mecho hello\x1b[0m\x1b]133;C\x1b\\");

        if let Osc133Phase::Executing { command } = p.osc133_phase() {
            // Bold escape stripped, just "echo hello" with prompt prefix
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

    // --- Existing tests ---

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

    #[test]
    fn full_command_cycle() {
        let mut p = PaneProcessor::new(24, 80);

        let e1 = p.process_chunk(
            b"\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\\x1b]7;file://localhost/home/user\x1b\\$ \x1b]133;B\x1b\\"
        );
        assert_eq!(e1.len(), 4);

        let e2 = p.process_chunk(b"echo hello\x1b]133;C\x1b\\\x1b]133;E;echo hello\x1b\\");
        assert_eq!(e2.len(), 2);
        assert_eq!(p.screen_line_text(0), "$ echo hello");

        let e3 = p.process_chunk(b"\r\nhello\r\n\x1b]133;D;0\x1b\\");
        assert_eq!(e3.len(), 1);
        assert_eq!(p.screen_line_text(1), "hello");
    }
}
