/// Parser for tmux layout strings.
///
/// Tmux encodes window layouts as compact strings in `%layout-change` notifications:
///
///   `4a06,204x51,0,0{102x51,0,0,3,101x51,103,0,4}`
///
/// The grammar is:
///   layout  = checksum "," cell
///   cell    = WxH "," X "," Y "," pane_number       (leaf)
///           | WxH "," X "," Y "{" cell_list "}"      (horizontal split)
///           | WxH "," X "," Y "[" cell_list "]"      (vertical split)
///   cell_list = cell ("," cell)*
///
/// We extract leaf geometry: (pane_number, width, height, x, y).

/// A leaf pane extracted from a tmux layout string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutPane {
    pub pane_number: u32,
    pub width: usize,
    pub height: usize,
    pub x: usize,
    pub y: usize,
}

/// Parse a tmux layout string, extracting leaf pane geometry.
///
/// Returns an empty vec on malformed input (never panics).
pub fn parse_layout(layout: &str) -> Vec<LayoutPane> {
    let bytes = layout.as_bytes();

    // Skip checksum: everything up to and including the first ','
    let Some(comma) = memchr(b',', bytes) else {
        return vec![];
    };
    let mut pos = comma + 1;

    let mut result = Vec::new();
    parse_cell(bytes, &mut pos, &mut result);
    result
}

fn parse_cell(bytes: &[u8], pos: &mut usize, result: &mut Vec<LayoutPane>) {
    // Parse WxH
    let Some(width) = parse_number(bytes, pos) else { return };
    if !expect(bytes, pos, b'x') { return }
    let Some(height) = parse_number(bytes, pos) else { return };

    // Parse ,X,Y
    if !expect(bytes, pos, b',') { return }
    let Some(x) = parse_number(bytes, pos) else { return };
    if !expect(bytes, pos, b',') { return }
    let Some(y) = parse_number(bytes, pos) else { return };

    if *pos >= bytes.len() {
        // Bare root node with no pane number — single pane layouts still
        // have a trailing pane number, so this is likely malformed.
        return;
    }

    match bytes[*pos] {
        b'{' | b'[' => {
            let close = if bytes[*pos] == b'{' { b'}' } else { b']' };
            *pos += 1;
            parse_cell(bytes, pos, result);
            while *pos < bytes.len() && bytes[*pos] == b',' {
                *pos += 1;
                parse_cell(bytes, pos, result);
            }
            if *pos < bytes.len() && bytes[*pos] == close {
                *pos += 1;
            }
        }
        b',' => {
            // Leaf: ,PANE_NUMBER
            *pos += 1;
            let Some(pane_number) = parse_number(bytes, pos) else { return };
            result.push(LayoutPane {
                pane_number: pane_number as u32,
                width,
                height,
                x,
                y,
            });
        }
        _ => {
            // Unexpected character — bail out of this cell
        }
    }
}

fn parse_number(bytes: &[u8], pos: &mut usize) -> Option<usize> {
    let start = *pos;
    while *pos < bytes.len() && bytes[*pos].is_ascii_digit() {
        *pos += 1;
    }
    if *pos == start {
        return None;
    }
    // SAFETY: we only advanced over ASCII digits
    let s = std::str::from_utf8(&bytes[start..*pos]).ok()?;
    s.parse().ok()
}

fn expect(bytes: &[u8], pos: &mut usize, ch: u8) -> bool {
    if *pos < bytes.len() && bytes[*pos] == ch {
        *pos += 1;
        true
    } else {
        false
    }
}

fn memchr(needle: u8, haystack: &[u8]) -> Option<usize> {
    haystack.iter().position(|&b| b == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_pane() {
        let panes = parse_layout("a1b2,80x24,0,0,0");
        assert_eq!(panes, vec![LayoutPane {
            pane_number: 0, width: 80, height: 24, x: 0, y: 0,
        }]);
    }

    #[test]
    fn horizontal_split() {
        // Two panes side by side
        let panes = parse_layout("d5a0,200x50,0,0{100x50,0,0,1,99x50,101,0,2}");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0], LayoutPane {
            pane_number: 1, width: 100, height: 50, x: 0, y: 0,
        });
        assert_eq!(panes[1], LayoutPane {
            pane_number: 2, width: 99, height: 50, x: 101, y: 0,
        });
    }

    #[test]
    fn vertical_split() {
        // Two panes stacked
        let panes = parse_layout("e3f1,80x50,0,0[80x25,0,0,3,80x24,0,26,4]");
        assert_eq!(panes.len(), 2);
        assert_eq!(panes[0], LayoutPane {
            pane_number: 3, width: 80, height: 25, x: 0, y: 0,
        });
        assert_eq!(panes[1], LayoutPane {
            pane_number: 4, width: 80, height: 24, x: 0, y: 26,
        });
    }

    #[test]
    fn nested_split() {
        // Left pane + right side split vertically into two
        let panes = parse_layout(
            "abcd,200x50,0,0{100x50,0,0,1,99x50,101,0[99x25,101,0,2,99x24,101,26,3]}"
        );
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0], LayoutPane {
            pane_number: 1, width: 100, height: 50, x: 0, y: 0,
        });
        assert_eq!(panes[1], LayoutPane {
            pane_number: 2, width: 99, height: 25, x: 101, y: 0,
        });
        assert_eq!(panes[2], LayoutPane {
            pane_number: 3, width: 99, height: 24, x: 101, y: 26,
        });
    }

    #[test]
    fn three_way_horizontal() {
        let panes = parse_layout(
            "1234,300x50,0,0{100x50,0,0,0,100x50,101,0,1,98x50,202,0,2}"
        );
        assert_eq!(panes.len(), 3);
        assert_eq!(panes[0].pane_number, 0);
        assert_eq!(panes[1].pane_number, 1);
        assert_eq!(panes[2].pane_number, 2);
        assert_eq!(panes[2].x, 202);
    }

    #[test]
    fn deeply_nested() {
        // {leaf, [leaf, {leaf, leaf}]}
        let panes = parse_layout(
            "ffff,200x50,0,0{100x50,0,0,1,99x50,101,0[99x25,101,0,2,99x24,101,26{49x24,101,26,3,49x24,151,26,4}]}"
        );
        assert_eq!(panes.len(), 4);
        assert_eq!(panes[0].pane_number, 1);
        assert_eq!(panes[1].pane_number, 2);
        assert_eq!(panes[2].pane_number, 3);
        assert_eq!(panes[3].pane_number, 4);
    }

    #[test]
    fn high_pane_numbers() {
        let panes = parse_layout("ab12,80x24,0,0,42");
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0].pane_number, 42);
    }

    #[test]
    fn malformed_empty() {
        assert!(parse_layout("").is_empty());
    }

    #[test]
    fn malformed_no_comma() {
        assert!(parse_layout("abcd").is_empty());
    }

    #[test]
    fn malformed_truncated() {
        assert!(parse_layout("abcd,80x").is_empty());
    }

    #[test]
    fn malformed_no_pane_id() {
        // Root dims but no leaf/split — malformed
        assert!(parse_layout("abcd,80x24,0,0").is_empty());
    }
}
