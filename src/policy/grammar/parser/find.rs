//! The `find` parser: options are single-dash words, operators join them,
//! and the start points come first. `-H`, `-L` and `-P` stand before the
//! start points; any other option begins the expression.

use super::Scan;
use crate::policy::grammar::run::{Read, Run, set_flag, take_value};
use crate::policy::grammar::{Opt, list};
use crate::policy::parse::Word;
use crate::policy::value::TriVal;

pub(super) fn is_option(text: &str) -> bool {
    (text.starts_with('-') && text.len() > 1) || is_operator(text)
}

pub(super) fn option(
    text: &str,
    rest: &[Word],
    active: &[&Opt],
    scan: &mut Scan,
    run: &mut Run,
) -> Read {
    if is_operator(text) {
        scan.expression = true;
        return Read::took(0);
    }
    if !matches!(text, "-H" | "-L" | "-P") || scan.expression {
        scan.expression = true;
    }
    match active.iter().find(|o| o.single.iter().any(|s| s == text)) {
        Some(o) if o.command => Read::took(take_command(o, rest, run)),
        Some(o) if o.value => Read::took(take_value(o, None, rest, run)),
        Some(o) => Read::took(set_flag(o, run)),
        None => Read::undeclared(false, rest),
    }
}

/// A start point is an operand; a word in the expression that is no
/// option's value is not.
pub(super) fn operand(scan: &mut Scan) -> bool {
    !scan.expression
}

/// `-exec` and its kin: a command up to `;`, or up to `{}` and `+`. `{}` is
/// a file path that cannot be known, and so is a word that holds it. A word
/// that may split may hide the `;`: what follows cannot be known.
fn take_command(opt: &Opt, rest: &[Word], run: &mut Run) -> usize {
    let mut words = Vec::new();
    let mut taken = rest.len();
    for (i, word) in rest.iter().enumerate() {
        match word {
            Word::Known(w) if w == ";" => {
                taken = i + 1;
                break;
            }
            Word::Known(w) if w == "+" && i > 0 && rest[i - 1] == Word::Known("{}".into()) => {
                taken = i + 1;
                break;
            }
            Word::Known(w) if !w.contains("{}") => words.push(TriVal::String(w.clone())),
            Word::Unknown { splits: true } => {
                words.push(TriVal::Unknown);
                run.unsure = true;
                taken = i + 1;
                break;
            }
            _ => words.push(TriVal::Unknown),
        }
    }
    if let Some(TriVal::List { elements, .. }) = run.parts.get_mut(&opt.name) {
        elements.push(list(words, true));
    }
    taken
}

fn is_operator(text: &str) -> bool {
    matches!(
        text,
        "(" | ")" | "!" | "," | "-a" | "-o" | "-not" | "-and" | "-or"
    )
}
