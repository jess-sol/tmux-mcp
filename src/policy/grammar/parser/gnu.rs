//! The `gnu` parser: getopt_long as GNU tools use it. Options stand anywhere
//! until `--`; `-rf` clusters; `-n10` and `--lines=10` attach a value; the
//! parser settings read a tool's number options and `-e=x`.

use super::{Parser, Scan};
use crate::policy::grammar::Opt;
use crate::policy::grammar::run::{Read, Run, push_value, set_flag, take_value};
use crate::policy::parse::Word;

pub(super) fn is_option(p: &Parser, text: &str, at_start: bool) -> bool {
    (text.starts_with('-') && text.len() > 1) || (at_start && p.leading_number.is_some() && is_number(text, true))
}

pub(super) fn option(p: &Parser, text: &str, rest: &[Word], at_start: bool, active: &[&Opt], scan: &mut Scan, run: &mut Run) -> Read {
    if text == "--" {
        scan.options_end = true;
        return Read::took(0);
    }
    if let Some(name) = &p.leading_number
        && at_start
        && is_number(text, true)
    {
        push_value(run, name, Word::Known(text.trim_start_matches('-').to_string()));
        return Read::took(0);
    }
    if let Some(name) = &p.number
        && is_number(text, false)
    {
        push_value(run, name, Word::Known(text[1..].to_string()));
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
fn is_number(text: &str, plus: bool) -> bool {
    let digits = match text.strip_prefix('-') {
        Some(d) => d,
        None if plus => match text.strip_prefix('+') {
            Some(d) => d,
            None => return false,
        },
        None => return false,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}
