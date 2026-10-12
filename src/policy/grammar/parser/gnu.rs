//! The `gnu` parser: getopt_long as GNU tools use it. Options stand anywhere
//! until `--`; `-rf` clusters; `-n10` and `--lines=10` attach a value; a
//! `dash` option reads `-5` and `-KILL`; the parser setting reads `-e=x`.

use super::{Parser, Scan};
use crate::policy::grammar::Opt;
use crate::policy::grammar::run::{Read, Run, push_value, set_flag, take_value};
use crate::policy::parse::Word;

pub(super) fn is_option(text: &str, at_start: bool, active: &[&Opt]) -> bool {
    (text.starts_with('-') && text.len() > 1)
        || (at_start && is_plus_number(text) && active.iter().any(|o| o.dash.is_some()))
}

pub(super) fn option(
    p: &Parser,
    text: &str,
    rest: &[Word],
    at_start: bool,
    active: &[&Opt],
    scan: &mut Scan,
    run: &mut Run,
) -> Read {
    if text == "--" {
        scan.options_end = true;
        return Read::took(0);
    }
    if let Some((o, value)) = dash_value(text, at_start, active) {
        push_value(run, &o.name, Word::Known(value));
        return Read::took(0);
    }
    if let Some(long) = text.strip_prefix("--") {
        let (spelling, attached) = match long.split_once('=') {
            Some((s, v)) => (s, Some(v.to_string())),
            None => (long, None),
        };
        // A declared flag written `--color=auto` takes that value and nothing more.
        return match active.iter().find(|o| o.long.iter().any(|l| l == spelling)) {
            Some(o) if o.value => Read::took(take_value(o, attached, rest, run)),
            Some(o) => Read::took(set_flag(o, run)),
            None => Read::undeclared(attached.is_some(), rest),
        };
    }
    let chars: Vec<char> = text[1..].chars().collect();
    let mut read = Read::took(0);
    for (j, c) in chars.iter().enumerate() {
        match active.iter().find(|o| o.short.contains(c)) {
            Some(o) if o.value => {
                let attached: String = chars[j + 1..].iter().collect();
                let attached = match attached.strip_prefix('=') {
                    Some(v) if p.short_equals => v.to_string(),
                    _ => attached,
                };
                read.taken = take_value(o, (!attached.is_empty()).then_some(attached), rest, run);
                return read;
            }
            Some(o) => _ = set_flag(o, run),
            // The rest of the cluster may be this letter's value.
            None if j + 1 < chars.len() => {
                run.doubt = true;
                read.undeclared = true;
            }
            None => read.undeclared |= !rest.is_empty(),
        }
    }
    read
}

/// `-5`, and with `plus` also `+5`.
/// The `dash` option that `text` is a value of, and the value: `-5` or
/// `-KILL` (read before a cluster of flags), or a first word `+5`, which
/// keeps its sign.
fn dash_value<'a>(text: &str, at_start: bool, active: &[&'a Opt]) -> Option<(&'a Opt, String)> {
    if at_start && is_plus_number(text) {
        let o = active.iter().find(|o| o.dash.is_some())?;
        return Some((o, text.to_string()));
    }
    let value = text.strip_prefix('-').filter(|v| !v.starts_with('-'))?;
    let o = active.iter().find(|o| {
        o.dash
            .as_ref()
            .is_some_and(|words| is_digits(value) || words.contains(&value.to_uppercase()))
    })?;
    Some((o, value.to_string()))
}

fn is_plus_number(text: &str) -> bool {
    text.strip_prefix('+').is_some_and(is_digits)
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}
