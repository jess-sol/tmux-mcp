//! Quote removal: a word as the command receives it, or Unknown when the
//! shell expands it.

use brush_parser::word::{BraceExpressionOrText, WordPiece};

use super::Word;
use crate::policy::value::TriVal;

/// A word as the command receives it: quotes removed, `~` expanded with
/// `home`, or Unknown when the shell expands it otherwise. Glob and `~user`
/// words stay as written; a brace word is Unknown, since it can become
/// several words.
pub(crate) fn word(raw: &str, home: &TriVal) -> Word {
    let options = brush_parser::ParserOptions::default();
    if let Ok(Some(pieces)) = brush_parser::word::parse_brace_expansions(raw, &options)
        && pieces.iter().any(|p| matches!(p, BraceExpressionOrText::Expr(_)))
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

/// Appends what `piece` contributes to a word's text. An expansion makes the
/// word Unknown: one that may split when unquoted, or when it is `$@`-like.
fn append_piece(piece: &WordPiece, quoted: bool, raw: &str, home: &TriVal, text: &mut String, unknown: &mut Option<bool>) {
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
