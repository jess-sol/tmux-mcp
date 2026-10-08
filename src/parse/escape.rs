/// Unescape tmux control mode %output data.
///
/// Tmux encodes non-printable bytes as octal escapes in its control mode output:
///   \033 → ESC (0x1b)
///   \007 → BEL (0x07)
///   \134 → \   (0x5c)
///   \015 → CR  (0x0d)
///   \012 → LF  (0x0a)
///
/// Every other byte, a fragment of a multibyte character included, passes
/// through unchanged.
pub fn unescape_tmux_output(escaped: &[u8]) -> Vec<u8> {
    let is_octal = |b: u8| (b'0'..=b'7').contains(&b);
    let mut result = Vec::with_capacity(escaped.len());
    let mut bytes = escaped.iter().copied().peekable();

    while let Some(b) = bytes.next() {
        if b != b'\\' {
            result.push(b);
            continue;
        }
        match bytes.peek() {
            Some(&d) if is_octal(d) => {
                // Octal escape: up to 3 octal digits
                let mut value: u32 = 0;
                for _ in 0..3 {
                    match bytes.peek() {
                        Some(&d) if is_octal(d) => {
                            value = value * 8 + u32::from(d - b'0');
                            bytes.next();
                        }
                        _ => break,
                    }
                }
                if let Ok(byte) = u8::try_from(value) {
                    result.push(byte);
                }
            }
            Some(&b'\\') => {
                bytes.next();
                result.push(b'\\');
            }
            _ => result.push(b'\\'),
        }
    }
    result
}

/// Escape a string for tmux send-keys in control mode.
///
/// Wraps in single quotes, escaping any internal single quotes as `'\''`.
pub fn shell_escape(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- unescape_tmux_output ---

    #[test]
    fn unescape_plain_ascii() {
        assert_eq!(unescape_tmux_output(b"hello world"), b"hello world");
    }

    #[test]
    fn unescape_empty() {
        assert_eq!(unescape_tmux_output(b""), b"");
    }

    #[test]
    fn unescape_esc_033() {
        let result = unescape_tmux_output(b"\\033[31m");
        assert_eq!(result, b"\x1b[31m");
    }

    #[test]
    fn unescape_bel_007() {
        let result = unescape_tmux_output(b"\\007");
        assert_eq!(result, b"\x07");
    }

    #[test]
    fn unescape_backslash_134() {
        let result = unescape_tmux_output(b"\\134");
        assert_eq!(result, b"\\");
    }

    #[test]
    fn unescape_cr_015() {
        let result = unescape_tmux_output(b"\\015");
        assert_eq!(result, b"\r");
    }

    #[test]
    fn unescape_lf_012() {
        let result = unescape_tmux_output(b"\\012");
        assert_eq!(result, b"\n");
    }

    #[test]
    fn unescape_double_backslash() {
        let result = unescape_tmux_output(b"a\\\\b");
        assert_eq!(result, b"a\\b");
    }

    #[test]
    fn unescape_trailing_backslash() {
        // Backslash at end of input with nothing after it
        let result = unescape_tmux_output(b"abc\\");
        assert_eq!(result, b"abc\\");
    }

    #[test]
    fn unescape_osc133_marker() {
        // Full OSC 133;A marker as tmux would encode it
        let result = unescape_tmux_output(b"\\033]133;A\\007");
        assert_eq!(result, b"\x1b]133;A\x07");
    }

    #[test]
    fn unescape_osc133_with_st_terminator() {
        // OSC 133;D;0 with ST terminator (ESC \)
        let result = unescape_tmux_output(b"\\033]133;D;0\\033\\134");
        assert_eq!(result, b"\x1b]133;D;0\x1b\\");
    }

    #[test]
    fn unescape_osc7() {
        let result = unescape_tmux_output(b"\\033]7;file://myhost/home/user\\007");
        assert_eq!(result, b"\x1b]7;file://myhost/home/user\x07");
    }

    #[test]
    fn unescape_mixed_text_and_escapes() {
        let result = unescape_tmux_output(b"hello\\033[32m world\\033[0m");
        assert_eq!(result, b"hello\x1b[32m world\x1b[0m");
    }

    #[test]
    fn unescape_utf8_passthrough() {
        // UTF-8 characters should pass through unchanged
        let result = unescape_tmux_output("héllo wörld 日本語".as_bytes());
        assert_eq!(result, "héllo wörld 日本語".as_bytes());
    }

    #[test]
    fn unescape_utf8_mixed_with_escapes() {
        let result = unescape_tmux_output("\\033[1m日本語\\033[0m".as_bytes());
        let mut expected = vec![0x1b, b'[', b'1', b'm'];
        expected.extend_from_slice("日本語".as_bytes());
        expected.extend_from_slice(&[0x1b, b'[', b'0', b'm']);
        assert_eq!(result, expected);
    }

    #[test]
    fn unescape_all_octal_digits() {
        // Test boundary octal values
        assert_eq!(unescape_tmux_output(b"\\000"), b"\x00"); // NUL
        assert_eq!(unescape_tmux_output(b"\\001"), b"\x01"); // SOH
        assert_eq!(unescape_tmux_output(b"\\177"), b"\x7f"); // DEL
        assert_eq!(unescape_tmux_output(b"\\377"), b"\xff"); // 255
    }

    #[test]
    fn unescape_short_octal() {
        // 1-digit and 2-digit octals
        assert_eq!(unescape_tmux_output(b"\\0"), b"\x00");
        assert_eq!(unescape_tmux_output(b"\\7"), b"\x07");
        assert_eq!(unescape_tmux_output(b"\\77"), b"\x3f"); // '?'
    }

    #[test]
    fn unescape_octal_followed_by_digit() {
        // Octal escape followed by a non-octal digit (8 or 9)
        let result = unescape_tmux_output(b"\\0339"); // \033 then '9'
        assert_eq!(result, b"\x1b9");
    }

    #[test]
    fn unescape_consecutive_escapes() {
        let result = unescape_tmux_output(b"\\033\\033\\007\\007");
        assert_eq!(result, b"\x1b\x1b\x07\x07");
    }

    #[test]
    fn unescape_backslash_before_non_octal() {
        // Backslash followed by non-octal, non-backslash
        let result = unescape_tmux_output(b"\\n\\t");
        assert_eq!(result, b"\\n\\t");
    }

    #[test]
    fn unescape_large_input() {
        // Ensure it handles large input without issues
        let large = "hello ".repeat(10_000);
        let result = unescape_tmux_output(large.as_bytes());
        assert_eq!(result.len(), 60_000);
    }

    #[test]
    fn unescape_realistic_output_line() {
        // Realistic %output content: prompt with OSC 133 markers + colors
        let input = "\\033]133;D;0\\033\\134\\033]133;A\\033\\134\\033]7;file://localhost/home/user\\033\\134\\033[01;32muser@host\\033[00m:\\033[01;34m~\\033[00m$ \\033]133;B\\033\\134";
        let result = unescape_tmux_output(input.as_bytes());
        // Should start with ESC]133;D;0
        assert_eq!(result[0], 0x1b);
        assert_eq!(result[1], b']');
        assert_eq!(&result[2..6], b"133;");
        // And contain valid bytes throughout
        assert!(result.len() > 50);
    }

    // --- shell_escape ---

    #[test]
    fn shell_escape_simple() {
        assert_eq!(shell_escape("hello"), "'hello'");
    }

    #[test]
    fn shell_escape_with_spaces() {
        assert_eq!(shell_escape("hello world"), "'hello world'");
    }

    #[test]
    fn shell_escape_with_single_quotes() {
        assert_eq!(shell_escape("it's"), "'it'\\''s'");
    }

    #[test]
    fn shell_escape_empty() {
        assert_eq!(shell_escape(""), "''");
    }

    #[test]
    fn shell_escape_special_chars() {
        assert_eq!(shell_escape("$HOME"), "'$HOME'");
        assert_eq!(shell_escape("a;b"), "'a;b'");
        assert_eq!(shell_escape("a&b"), "'a&b'");
    }
}
