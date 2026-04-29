/// Pane processor: owns a headless alacritty terminal instance and
/// processes byte chunks with OSC boundary splitting for synchronized
/// metadata extraction.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::Config;
use alacritty_terminal::Term;
use vte::ansi::Processor;

use crate::pane::osc::{find_next_osc_from, OscMatch};

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
}

impl PaneProcessor {
    /// Create a new pane processor with the given dimensions.
    pub fn new(lines: usize, columns: usize) -> Self {
        let dims = PaneDimensions { lines, columns };
        let config = Config::default();
        let term = Term::new(config, &dims, VoidListener);
        let processor = Processor::new();
        Self { term, processor }
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
        let mut events = Vec::new();
        let mut pos = 0;

        while pos < bytes.len() {
            match find_next_osc_from(bytes, pos) {
                Some(osc_match) => {
                    // Feed bytes before the OSC to the terminal emulator
                    if osc_match.start > pos {
                        self.processor.advance(&mut self.term, &bytes[pos..osc_match.start]);
                    }

                    // Record the event (screen is at exactly the right state here)
                    events.push(osc_match.clone());

                    // Advance past the OSC sequence
                    pos = osc_match.end;
                }
                None => {
                    // No more OSC events — feed remaining bytes to terminal
                    self.processor.advance(&mut self.term, &bytes[pos..]);
                    pos = bytes.len();
                }
            }
        }

        events
    }

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
        // Trim trailing spaces
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::osc::OscEvent;

    #[test]
    fn empty_processor() {
        let p = PaneProcessor::new(24, 80);
        assert_eq!(p.screen_lines(), 24);
        assert_eq!(p.columns(), 80);
        assert_eq!(p.cursor_position(), (0, 0));
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
        // Type "abcd", backspace twice, type "ef"
        p.process_chunk(b"abcd\x08\x08ef");
        assert_eq!(p.screen_line_text(0), "abef");
    }

    #[test]
    fn cursor_movement_editing() {
        let mut p = PaneProcessor::new(24, 80);
        // Type "hello", move cursor left 3, type "X"
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
        // Simulate: prompt, user types "ls -la", then C marker
        p.process_chunk(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        let events = p.process_chunk(b"ls -la\x1b]133;C\x07");
        // At the moment of C, the screen should show "$ ls -la"
        assert_eq!(p.screen_line_text(0), "$ ls -la");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event, OscEvent::Osc133 { marker: b'C', param: None });
    }

    #[test]
    fn osc133_c_with_readline_editing() {
        let mut p = PaneProcessor::new(24, 80);
        // Prompt
        p.process_chunk(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        // User types "shwo", backspaces twice, types "ow" → "show"
        // Then C marker
        let events = p.process_chunk(b"shwo\x08\x08ow\x1b]133;C\x07");
        // Screen should show "$ show" at the moment of C
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
    }

    #[test]
    fn non_intercepted_osc_passes_through() {
        let mut p = PaneProcessor::new(24, 80);
        // OSC 0 (window title) should pass through to alacritty, not be intercepted
        let events = p.process_chunk(b"\x1b]0;My Title\x07hello");
        assert!(events.is_empty());
        assert_eq!(p.screen_line_text(0), "hello");
    }

    #[test]
    fn mixed_text_and_osc() {
        let mut p = PaneProcessor::new(24, 80);
        let events = p.process_chunk(b"before\x1b]133;C\x07after\r\nline2");
        assert_eq!(events.len(), 1);
        // "before" was fed to terminal before C, "after" after
        assert_eq!(p.screen_line_text(0), "beforeafter");
        assert_eq!(p.screen_line_text(1), "line2");
    }

    #[test]
    fn full_command_cycle() {
        let mut p = PaneProcessor::new(24, 80);

        // D;0 from previous command + A + OSC7 + prompt text + B
        let e1 = p.process_chunk(
            b"\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\\x1b]7;file://localhost/home/user\x1b\\$ \x1b]133;B\x1b\\"
        );
        assert_eq!(e1.len(), 4); // D, A, 7, B

        // User types command + C + E
        let e2 = p.process_chunk(b"echo hello\x1b]133;C\x1b\\\x1b]133;E;echo hello\x1b\\");
        assert_eq!(e2.len(), 2); // C, E
        assert_eq!(p.screen_line_text(0), "$ echo hello");

        // Command output + D
        let e3 = p.process_chunk(b"\r\nhello\r\n\x1b]133;D;0\x1b\\");
        assert_eq!(e3.len(), 1); // D
        assert_eq!(p.screen_line_text(1), "hello");
    }
}
