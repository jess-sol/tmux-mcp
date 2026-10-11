//! Quote removal: a word as the command receives it, or Unknown when the
//! shell expands it.

use brush_parser::word::{BraceExpressionOrText, WordPiece, WordPieceWithSource};

use super::Word;
use crate::policy::value::TriVal;

/// A word as the command receives it: quotes removed, `~` expanded with
/// `home`, or Unknown when the shell expands it otherwise. Glob and `~user`
/// words stay as written; a brace word is Unknown, since it can become
/// several words.
pub(crate) fn word(raw: &str, home: &TriVal) -> Word {
    let options = brush_parser::ParserOptions::default();
    if let Ok(Some(pieces)) = brush_parser::word::parse_brace_expansions(raw, &options)
        && pieces
            .iter()
            .any(|p| matches!(p, BraceExpressionOrText::Expr(_)))
    {
        return Word::Unknown { splits: true };
    }
    let Ok(pieces) = brush_parser::word::parse(raw, &options) else {
        return Word::Unknown { splits: true };
    };
    let mut text = String::new();
    let mut unknown: Option<bool> = None;
    for piece in &pieces {
        append_piece(&piece.piece, false, raw, home, &mut text, &mut unknown);
    }
    match unknown {
        Some(splits) => Word::Unknown { splits },
        None => Word::Known(text),
    }
}

/// The word expands to a fixed string: no parameter, command or arithmetic
/// expansion, no tilde, and no glob or brace character outside quotes. A
/// word brush cannot parse is not constant.
pub(crate) fn constant(raw: &str) -> bool {
    let options = brush_parser::ParserOptions::default();
    brush_parser::word::parse(raw, &options)
        .is_ok_and(|pieces| pieces.iter().all(|p| constant_piece(&p.piece, false)))
}

fn constant_piece(piece: &WordPiece, quoted: bool) -> bool {
    match piece {
        WordPiece::Text(s) => quoted || !s.contains(['*', '?', '[', ']', '{', '}']),
        WordPiece::SingleQuotedText(_)
        | WordPiece::AnsiCQuotedText(_)
        | WordPiece::EscapeSequence(_) => true,
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            inner.iter().all(|p| constant_piece(&p.piece, true))
        }
        _ => false,
    }
}

/// The names of the parameters the word expands, in order: `?` for `$?`,
/// `x` for `${#x}`.
pub(crate) fn expanded(raw: &str) -> Vec<String> {
    let options = brush_parser::ParserOptions::default();
    let mut names = Vec::new();
    if let Ok(pieces) = brush_parser::word::parse(raw, &options) {
        for piece in &pieces {
            expanded_piece(piece, raw, &mut names);
        }
    }
    names
}

fn expanded_piece(piece: &WordPieceWithSource, raw: &str, names: &mut Vec<String>) {
    match &piece.piece {
        WordPiece::ParameterExpansion(_) => {
            if let Some(name) = raw
                .get(piece.start_index..piece.end_index)
                .and_then(parameter_name)
            {
                names.push(name);
            }
        }
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            for p in inner {
                expanded_piece(p, raw, names);
            }
        }
        _ => {}
    }
}

/// `$?` → `?`, `${HOME}` → `HOME`, `${#x}` and `${!x}` → `x`, `${#}` → `#`.
fn parameter_name(source: &str) -> Option<String> {
    let s = source.strip_prefix('$')?;
    let s = s.strip_prefix('{').unwrap_or(s);
    let is_name = |c: char| c.is_ascii_alphanumeric() || c == '_';
    let s = match s.strip_prefix(['#', '!']) {
        Some(rest) if rest.starts_with(is_name) => rest,
        _ => s,
    };
    let first = s.chars().next()?;
    if first.is_ascii_alphabetic() || first == '_' {
        Some(s.chars().take_while(|c| is_name(*c)).collect())
    } else {
        Some(first.to_string())
    }
}

/// Appends what `piece` contributes to a word's text. An expansion makes the
/// word Unknown: one that may split when unquoted, or when it is `$@`-like.
fn append_piece(
    piece: &WordPiece,
    quoted: bool,
    raw: &str,
    home: &TriVal,
    text: &mut String,
    unknown: &mut Option<bool>,
) {
    match piece {
        WordPiece::Text(s) | WordPiece::SingleQuotedText(s) => text.push_str(s),
        WordPiece::AnsiCQuotedText(s) if !s.contains('\\') => text.push_str(s),
        WordPiece::AnsiCQuotedText(_) => *unknown = Some(unknown.unwrap_or(false)),
        WordPiece::TildePrefix(user) if user.is_empty() => match home {
            TriVal::String(h) => text.push_str(h),
            _ => *unknown = Some(unknown.unwrap_or(false)),
        },
        WordPiece::TildePrefix(user) => {
            text.push('~');
            text.push_str(user);
        }
        WordPiece::EscapeSequence(s) => text.push_str(s.strip_prefix('\\').unwrap_or(s)),
        WordPiece::DoubleQuotedSequence(inner) | WordPiece::GettextDoubleQuotedSequence(inner) => {
            for ip in inner {
                append_piece(&ip.piece, true, raw, home, text, unknown);
            }
        }
        WordPiece::ParameterExpansion(_)
        | WordPiece::CommandSubstitution(_)
        | WordPiece::BackquotedCommandSubstitution(_)
        | WordPiece::ArithmeticExpression(_) => {
            let splits = !quoted || raw.contains('@') || raw.contains('*');
            *unknown = Some(unknown.unwrap_or(false) || splits);
        }
    }
}
