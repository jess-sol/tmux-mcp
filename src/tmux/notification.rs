/// Parsed notification from tmux control mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Notification {
    /// %output %<pane_id> <escaped-data>
    Output { pane_id: String, data: String },
    /// %window-close @<window_id>
    WindowClose { window_id: String },
    /// %session-closed $<session_id>
    SessionClose { session_id: String },
    /// %exit [reason]
    Exit { reason: Option<String> },
    /// Anything else (window-add, layout-change, pane-mode-changed, etc.)
    Other { line: String },
}

/// Response from a tmux control mode command (%begin/%end block).
#[derive(Debug, Clone)]
pub struct CommandResponse {
    pub output: String,
    pub is_error: bool,
}

/// Internal message type for the reader task: either a command response
/// or a notification that arrived between/during response blocks.
#[derive(Debug)]
pub(crate) enum ReaderMessage {
    Response(CommandResponse),
    Notification(Notification),
}

/// Parse a single line from tmux control mode stdout.
///
/// Tmux control mode output consists of:
/// - Response blocks: `%begin <ts> <id>` ... lines ... `%end <ts> <id>` (or `%error`)
/// - Notifications: `%output`, `%window-close`, `%exit`, etc.
///
/// This function is stateful via `current_block`: it accumulates lines between
/// %begin and %end/%error markers.
///
/// Returns `Some(message)` when a complete response or notification is ready.
pub(crate) fn parse_line(
    line: &str,
    current_block: &mut Option<Vec<String>>,
) -> Option<ReaderMessage> {
    // %begin starts a new response block
    if line.starts_with("%begin ") {
        *current_block = Some(Vec::new());
        return None;
    }

    // %end or %error terminates a response block
    if line.starts_with("%end ") || line.starts_with("%error ") {
        let is_error = line.starts_with("%error ");
        if let Some(lines) = current_block.take() {
            return Some(ReaderMessage::Response(CommandResponse {
                output: lines.join("\n"),
                is_error,
            }));
        }
        tracing::warn!("Orphan %end/%error without %begin: {:?}", line);
        return None;
    }

    // Notifications can appear even inside response blocks.
    // Check for known notification prefixes.
    if is_notification_line(line) {
        return Some(ReaderMessage::Notification(parse_notification(line)));
    }

    // Inside a response block: accumulate the line
    if let Some(lines) = current_block {
        lines.push(line.to_string());
        return None;
    }

    // Outside a block and not a known notification — treat as unknown notification
    Some(ReaderMessage::Notification(parse_notification(line)))
}

fn is_notification_line(line: &str) -> bool {
    line.starts_with('%')
        && (line.starts_with("%output ")
            || line.starts_with("%session-changed ")
            || line.starts_with("%window-add ")
            || line.starts_with("%window-close ")
            || line.starts_with("%window-pane-changed ")
            || line.starts_with("%pane-mode-changed ")
            || line.starts_with("%exit")
            || line.starts_with("%layout-change ")
            || line.starts_with("%subscription-changed ")
            || line.starts_with("%message "))
}

/// Parse a notification line into a typed Notification.
pub fn parse_notification(line: &str) -> Notification {
    // %output %<pane_id> <data>
    if let Some(rest) = line.strip_prefix("%output ") {
        if let Some(space_idx) = rest.find(' ') {
            let pane_id = rest[..space_idx].to_string();
            let data = rest[space_idx + 1..].to_string();
            return Notification::Output { pane_id, data };
        }
    }

    let parts: Vec<&str> = line.splitn(3, ' ').collect();

    match parts.first().copied() {
        Some("%window-close") if parts.len() >= 2 => {
            Notification::WindowClose { window_id: parts[1].to_string() }
        }
        Some("%session-closed") if parts.len() >= 2 => {
            Notification::SessionClose { session_id: parts[1].to_string() }
        }
        Some("%exit") => Notification::Exit { reason: parts.get(1).map(|s| s.to_string()) },
        _ => Notification::Other { line: line.to_string() },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_notification ---

    #[test]
    fn parse_output_notification() {
        let n = parse_notification("%output %0 \\033]133;A\\007hello");
        assert_eq!(
            n,
            Notification::Output {
                pane_id: "%0".to_string(),
                data: "\\033]133;A\\007hello".to_string(),
            }
        );
    }

    #[test]
    fn parse_output_with_high_pane_id() {
        let n = parse_notification("%output %42 data here");
        assert_eq!(
            n,
            Notification::Output {
                pane_id: "%42".to_string(),
                data: "data here".to_string(),
            }
        );
    }

    #[test]
    fn parse_output_empty_data() {
        // %output with pane_id but no data after the space
        let n = parse_notification("%output %0 ");
        assert_eq!(
            n,
            Notification::Output {
                pane_id: "%0".to_string(),
                data: "".to_string(),
            }
        );
    }

    #[test]
    fn parse_window_close() {
        let n = parse_notification("%window-close @1");
        assert_eq!(n, Notification::WindowClose { window_id: "@1".to_string() });
    }

    #[test]
    fn parse_session_closed() {
        let n = parse_notification("%session-closed $0");
        assert_eq!(n, Notification::SessionClose { session_id: "$0".to_string() });
    }

    #[test]
    fn parse_exit_no_reason() {
        let n = parse_notification("%exit");
        assert_eq!(n, Notification::Exit { reason: None });
    }

    #[test]
    fn parse_exit_with_reason() {
        let n = parse_notification("%exit server-exited");
        assert_eq!(n, Notification::Exit { reason: Some("server-exited".to_string()) });
    }

    #[test]
    fn parse_unknown_notification() {
        let n = parse_notification("%window-add @5");
        assert_eq!(n, Notification::Other { line: "%window-add @5".to_string() });
    }

    #[test]
    fn parse_layout_change() {
        let n = parse_notification("%layout-change @1 abc123");
        assert_eq!(
            n,
            Notification::Other {
                line: "%layout-change @1 abc123".to_string(),
            }
        );
    }

    // --- parse_line ---

    #[test]
    fn parse_line_simple_response() {
        let mut block = None;
        assert!(parse_line("%begin 123 0", &mut block).is_none());
        assert!(block.is_some());
        assert!(parse_line("response line 1", &mut block).is_none());
        assert!(parse_line("response line 2", &mut block).is_none());
        let msg = parse_line("%end 123 0", &mut block).unwrap();
        match msg {
            ReaderMessage::Response(r) => {
                assert_eq!(r.output, "response line 1\nresponse line 2");
                assert!(!r.is_error);
            }
            _ => panic!("expected Response"),
        }
        assert!(block.is_none());
    }

    #[test]
    fn parse_line_error_response() {
        let mut block = None;
        parse_line("%begin 123 0", &mut block);
        parse_line("error message", &mut block);
        let msg = parse_line("%error 123 0", &mut block).unwrap();
        match msg {
            ReaderMessage::Response(r) => {
                assert_eq!(r.output, "error message");
                assert!(r.is_error);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_line_empty_response() {
        let mut block = None;
        parse_line("%begin 123 0", &mut block);
        let msg = parse_line("%end 123 0", &mut block).unwrap();
        match msg {
            ReaderMessage::Response(r) => {
                assert_eq!(r.output, "");
                assert!(!r.is_error);
            }
            _ => panic!("expected Response"),
        }
    }

    #[test]
    fn parse_line_notification_outside_block() {
        let mut block = None;
        let msg = parse_line("%output %0 hello", &mut block).unwrap();
        match msg {
            ReaderMessage::Notification(Notification::Output { pane_id, data }) => {
                assert_eq!(pane_id, "%0");
                assert_eq!(data, "hello");
            }
            _ => panic!("expected Output notification"),
        }
    }

    #[test]
    fn parse_line_notification_inside_block() {
        // Notifications can arrive inside response blocks — they should
        // be returned immediately, not accumulated into the block.
        let mut block = None;
        parse_line("%begin 123 0", &mut block);
        let msg = parse_line("%output %0 data", &mut block).unwrap();
        assert!(matches!(msg, ReaderMessage::Notification(Notification::Output { .. })));
        // Block is still open
        assert!(block.is_some());
        let msg = parse_line("%end 123 0", &mut block).unwrap();
        assert!(matches!(msg, ReaderMessage::Response(_)));
    }

    #[test]
    fn parse_line_orphan_end() {
        // %end without matching %begin should be ignored
        let mut block = None;
        assert!(parse_line("%end 123 0", &mut block).is_none());
    }

    #[test]
    fn parse_line_unknown_outside_block() {
        let mut block = None;
        let msg = parse_line("some random line", &mut block).unwrap();
        match msg {
            ReaderMessage::Notification(Notification::Other { line }) => {
                assert_eq!(line, "some random line");
            }
            _ => panic!("expected Other notification"),
        }
    }

    #[test]
    fn parse_line_exit_notification() {
        let mut block = None;
        let msg = parse_line("%exit", &mut block).unwrap();
        assert!(matches!(msg, ReaderMessage::Notification(Notification::Exit { reason: None })));
    }

    #[test]
    fn parse_line_window_close_inside_block() {
        let mut block = None;
        parse_line("%begin 1 0", &mut block);
        let msg = parse_line("%window-close @3", &mut block).unwrap();
        assert!(matches!(
            msg,
            ReaderMessage::Notification(Notification::WindowClose { .. })
        ));
        assert!(block.is_some()); // block still open
    }
}
