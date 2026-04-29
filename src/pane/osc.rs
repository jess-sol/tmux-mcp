/// OSC boundary scanner for the raw byte stream.
///
/// Scans for OSC sequences we want to intercept (133, 7, 9) and returns
/// their position and parsed content. Other OSC sequences (window title,
/// colors, clipboard, etc.) are NOT matched — they pass through to
/// alacritty_terminal's handler.

/// An intercepted OSC sequence found in the byte stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OscMatch {
    /// Byte offset where the OSC sequence starts (the ESC byte).
    pub start: usize,
    /// Byte offset one past the end of the OSC sequence (past the terminator).
    pub end: usize,
    /// Parsed OSC event.
    pub event: OscEvent,
}

/// Parsed OSC events we intercept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OscEvent {
    /// OSC 133 — Shell integration (FinalTerm/iTerm2 protocol).
    /// Marker is the single uppercase letter (A, B, C, D, E).
    /// Param is the optional parameter after the marker (e.g., exit code for D, command text for E).
    Osc133 { marker: u8, param: Option<String> },

    /// OSC 7 — Current working directory reporting.
    /// Format: file://hostname/path
    Osc7 { uri: String },

    /// OSC 9 — Desktop notification (ConEmu/iTerm2 style).
    Osc9 { text: String },
}

/// Find the next intercepted OSC sequence in `bytes`.
///
/// Returns `None` if no intercepted OSC is found (including if there are
/// partial/incomplete sequences at the end of the buffer).
///
/// OSC format: ESC ] <number> ; <data> <terminator>
/// Terminator: BEL (0x07) or ST (ESC \)
pub fn find_next_osc(bytes: &[u8]) -> Option<OscMatch> {
    let mut pos = 0;

    while pos + 1 < bytes.len() {
        // Scan for ESC ]
        if bytes[pos] != 0x1b || bytes[pos + 1] != b']' {
            pos += 1;
            continue;
        }

        let seq_start = pos;
        let content_start = pos + 2; // past ESC ]

        // Find the semicolon separating the OSC number from the data
        let Some(semi_pos) = find_byte(bytes, content_start, b';') else {
            // No semicolon found — might be incomplete or not our format.
            // Skip this ESC ] and continue scanning.
            pos += 2;
            continue;
        };

        // Extract the OSC number
        let osc_number = &bytes[content_start..semi_pos];
        let data_start = semi_pos + 1;

        // Only intercept 133, 7, 9
        let intercepted = matches!(osc_number, b"133" | b"7" | b"9");
        if !intercepted {
            pos += 2;
            continue;
        }

        // Find the terminator: BEL (0x07) or ST (ESC \)
        let Some((term_end, data_end)) = find_osc_terminator(bytes, data_start) else {
            // Incomplete sequence at end of buffer — don't consume
            return None;
        };

        let data = &bytes[data_start..data_end];
        let event = parse_osc_event(osc_number, data)?;

        return Some(OscMatch {
            start: seq_start,
            end: term_end,
            event,
        });
    }

    None
}

/// Find the next intercepted OSC sequence starting at or after `offset`.
///
/// Convenience wrapper that adjusts the returned offsets relative to
/// the start of `bytes`.
pub fn find_next_osc_from(bytes: &[u8], offset: usize) -> Option<OscMatch> {
    if offset >= bytes.len() {
        return None;
    }
    find_next_osc(&bytes[offset..]).map(|mut m| {
        m.start += offset;
        m.end += offset;
        m
    })
}

// --- Internal helpers ---

fn find_byte(bytes: &[u8], from: usize, needle: u8) -> Option<usize> {
    for i in from..bytes.len() {
        if bytes[i] == needle {
            return Some(i);
        }
        // Stop if we hit a terminator or another ESC before finding the semicolon
        if bytes[i] == 0x07 || bytes[i] == 0x1b {
            return None;
        }
    }
    None
}

/// Find OSC terminator (BEL or ST) starting from `from`.
/// Returns (end_of_sequence, end_of_data) — end_of_sequence includes
/// the terminator bytes, end_of_data is where the data payload ends.
fn find_osc_terminator(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i < bytes.len() {
        if bytes[i] == 0x07 {
            // BEL terminator
            return Some((i + 1, i));
        }
        if bytes[i] == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
            // ST terminator (ESC \)
            return Some((i + 2, i));
        }
        if bytes[i] == 0x1b {
            // ESC but not followed by \ — might be incomplete ST or new sequence
            // If at end of buffer, treat as incomplete
            if i + 1 >= bytes.len() {
                return None;
            }
            // ESC followed by something else — malformed, stop
            return None;
        }
        i += 1;
    }
    None // No terminator found — incomplete
}

fn parse_osc_event(osc_number: &[u8], data: &[u8]) -> Option<OscEvent> {
    match osc_number {
        b"133" => {
            // OSC 133 format: <marker>[;<param>]
            // marker is a single uppercase letter (A-E)
            if data.is_empty() {
                return None;
            }
            let marker = data[0];
            if !marker.is_ascii_uppercase() {
                return None;
            }
            let param = if data.len() > 1 && data[1] == b';' {
                std::str::from_utf8(&data[2..]).ok().map(|s| s.to_string())
            } else {
                None
            };
            Some(OscEvent::Osc133 { marker, param })
        }
        b"7" => {
            let uri = std::str::from_utf8(data).ok()?.to_string();
            Some(OscEvent::Osc7 { uri })
        }
        b"9" => {
            let text = std::str::from_utf8(data).ok()?.to_string();
            Some(OscEvent::Osc9 { text })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Helper ---

    fn osc133(marker: u8, param: Option<&str>) -> OscEvent {
        OscEvent::Osc133 { marker, param: param.map(|s| s.to_string()) }
    }

    fn osc7(uri: &str) -> OscEvent {
        OscEvent::Osc7 { uri: uri.to_string() }
    }

    fn osc9(text: &str) -> OscEvent {
        OscEvent::Osc9 { text: text.to_string() }
    }

    // --- OSC 133 with BEL terminator ---

    #[test]
    fn osc133_a_bel() {
        let bytes = b"\x1b]133;A\x07";
        let m = find_next_osc(bytes).unwrap();
        assert_eq!(m.start, 0);
        assert_eq!(m.end, bytes.len());
        assert_eq!(m.event, osc133(b'A', None));
    }

    #[test]
    fn osc133_b_bel() {
        let m = find_next_osc(b"\x1b]133;B\x07").unwrap();
        assert_eq!(m.event, osc133(b'B', None));
    }

    #[test]
    fn osc133_c_bel() {
        let m = find_next_osc(b"\x1b]133;C\x07").unwrap();
        assert_eq!(m.event, osc133(b'C', None));
    }

    #[test]
    fn osc133_d_with_exit_code_bel() {
        let m = find_next_osc(b"\x1b]133;D;0\x07").unwrap();
        assert_eq!(m.event, osc133(b'D', Some("0")));
    }

    #[test]
    fn osc133_d_with_nonzero_exit_bel() {
        let m = find_next_osc(b"\x1b]133;D;127\x07").unwrap();
        assert_eq!(m.event, osc133(b'D', Some("127")));
    }

    #[test]
    fn osc133_e_with_command_bel() {
        let m = find_next_osc(b"\x1b]133;E;ls -la\x07").unwrap();
        assert_eq!(m.event, osc133(b'E', Some("ls -la")));
    }

    // --- OSC 133 with ST terminator ---

    #[test]
    fn osc133_a_st() {
        let bytes = b"\x1b]133;A\x1b\\";
        let m = find_next_osc(bytes).unwrap();
        assert_eq!(m.start, 0);
        assert_eq!(m.end, bytes.len());
        assert_eq!(m.event, osc133(b'A', None));
    }

    #[test]
    fn osc133_d_with_exit_code_st() {
        let m = find_next_osc(b"\x1b]133;D;0\x1b\\").unwrap();
        assert_eq!(m.event, osc133(b'D', Some("0")));
    }

    #[test]
    fn osc133_e_with_command_st() {
        let m = find_next_osc(b"\x1b]133;E;echo hello\x1b\\").unwrap();
        assert_eq!(m.event, osc133(b'E', Some("echo hello")));
    }

    // --- OSC 7 ---

    #[test]
    fn osc7_with_hostname() {
        let m = find_next_osc(b"\x1b]7;file://myhost/home/user\x07").unwrap();
        assert_eq!(m.event, osc7("file://myhost/home/user"));
    }

    #[test]
    fn osc7_localhost() {
        let m = find_next_osc(b"\x1b]7;file://localhost/tmp\x1b\\").unwrap();
        assert_eq!(m.event, osc7("file://localhost/tmp"));
    }

    #[test]
    fn osc7_no_hostname() {
        let m = find_next_osc(b"\x1b]7;file:///home/user\x07").unwrap();
        assert_eq!(m.event, osc7("file:///home/user"));
    }

    // --- OSC 9 ---

    #[test]
    fn osc9_notification() {
        let m = find_next_osc(b"\x1b]9;Build complete\x07").unwrap();
        assert_eq!(m.event, osc9("Build complete"));
    }

    // --- Embedded in other content ---

    #[test]
    fn osc_after_text() {
        let bytes = b"hello world\x1b]133;A\x07more text";
        let m = find_next_osc(bytes).unwrap();
        assert_eq!(m.start, 11);
        assert_eq!(m.end, 19); // ESC ] 1 3 3 ; A BEL = 8 bytes, starts at 11
        assert_eq!(m.event, osc133(b'A', None));
    }

    #[test]
    fn osc_between_text() {
        let bytes = b"before\x1b]133;C\x07after";
        let m = find_next_osc(bytes).unwrap();
        assert_eq!(m.start, 6);
        assert_eq!(m.end, 14); // ESC ] 1 3 3 ; C BEL = 8 bytes
        // Verify the boundaries
        assert_eq!(&bytes[..m.start], b"before");
        assert_eq!(&bytes[m.end..], b"after");
    }

    #[test]
    fn multiple_oscs_finds_first() {
        let bytes = b"\x1b]133;A\x07\x1b]133;B\x07";
        let m = find_next_osc(bytes).unwrap();
        assert_eq!(m.event, osc133(b'A', None));
        assert_eq!(m.end, 8); // ESC ] 1 3 3 ; A BEL = 8 bytes
    }

    #[test]
    fn find_next_osc_from_skips_first() {
        let bytes = b"\x1b]133;A\x07\x1b]133;B\x07";
        let first = find_next_osc(bytes).unwrap();
        let second = find_next_osc_from(bytes, first.end).unwrap();
        assert_eq!(second.event, osc133(b'B', None));
    }

    // --- Non-intercepted OSCs pass through ---

    #[test]
    fn non_intercepted_osc_types_ignored() {
        for bytes in [
            &b"\x1b]0;Window Title\x07"[..],       // window title
            &b"\x1b]4;1;rgb:ff/00/00\x07"[..],     // color palette
            &b"\x1b]52;c;SGVsbG8=\x07"[..],        // clipboard
            &b"\x1b]10;rgb:ff/ff/ff\x07"[..],      // foreground color
            &b"\x1b]8;;https://example.com\x07"[..], // hyperlink
        ] {
            assert!(find_next_osc(bytes).is_none(), "should ignore: {:?}", bytes);
        }
    }

    #[test]
    fn non_osc_escapes_skipped() {
        // CSI sequence (not OSC) should be skipped
        let bytes = b"\x1b[31mred\x1b[0m\x1b]133;A\x07";
        let m = find_next_osc(bytes).unwrap();
        assert_eq!(m.event, osc133(b'A', None));
    }

    // --- Edge cases ---

    #[test]
    fn empty_input() {
        assert!(find_next_osc(b"").is_none());
    }

    #[test]
    fn no_osc_in_input() {
        assert!(find_next_osc(b"hello world").is_none());
    }

    #[test]
    fn lone_esc() {
        assert!(find_next_osc(b"\x1b").is_none());
    }

    #[test]
    fn esc_bracket_not_osc() {
        assert!(find_next_osc(b"\x1b[31m").is_none());
    }

    #[test]
    fn incomplete_osc_at_end() {
        // OSC started but no terminator — should return None (incomplete)
        assert!(find_next_osc(b"\x1b]133;A").is_none());
    }

    #[test]
    fn incomplete_st_at_end() {
        // ESC at end could be start of ST — should return None (incomplete)
        assert!(find_next_osc(b"\x1b]133;A\x1b").is_none());
    }

    #[test]
    fn osc133_no_marker() {
        // OSC 133 with empty data (no marker letter)
        assert!(find_next_osc(b"\x1b]133;\x07").is_none());
    }

    #[test]
    fn osc133_lowercase_marker_rejected() {
        assert!(find_next_osc(b"\x1b]133;a\x07").is_none());
    }

    #[test]
    fn osc_with_non_utf8_data() {
        // OSC 7 with non-UTF8 data should be skipped (parse_osc_event returns None)
        let bytes = b"\x1b]7;\xff\xfe\x07".to_vec();
        assert!(find_next_osc(&bytes).is_none());
    }

    // --- Realistic sequences ---

    #[test]
    fn realistic_prompt_cycle() {
        // D;0 (previous command done) + A (prompt start) + OSC 7 (cwd) + B (input ready)
        let bytes = b"\x1b]133;D;0\x1b\\\x1b]133;A\x1b\\\x1b]7;file://localhost/home/user\x1b\\\x1b]133;B\x1b\\";

        let m1 = find_next_osc(bytes).unwrap();
        assert_eq!(m1.event, osc133(b'D', Some("0")));

        let m2 = find_next_osc_from(bytes, m1.end).unwrap();
        assert_eq!(m2.event, osc133(b'A', None));

        let m3 = find_next_osc_from(bytes, m2.end).unwrap();
        assert_eq!(m3.event, osc7("file://localhost/home/user"));

        let m4 = find_next_osc_from(bytes, m3.end).unwrap();
        assert_eq!(m4.event, osc133(b'B', None));

        assert!(find_next_osc_from(bytes, m4.end).is_none());
    }

    #[test]
    fn mixed_intercepted_and_passthrough() {
        // Window title (pass through) then OSC 133;A (intercept)
        let bytes = b"\x1b]0;bash\x07prompt$ \x1b]133;B\x07";
        let m = find_next_osc(bytes).unwrap();
        // Should skip the OSC 0 and find the OSC 133
        assert_eq!(m.event, osc133(b'B', None));
    }
}
