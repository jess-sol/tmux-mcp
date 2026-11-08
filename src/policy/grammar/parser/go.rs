//! The `go` parser: Go's `flag` package, as terraform and its kin read it.
//! An option is a single-dash word, read with two dashes too; `-name=value`
//! attaches a value, and a flag takes `=false`; the first operand ends the
//! options, as under `posix`.

use super::Scan;
use crate::policy::grammar::Opt;
use crate::policy::grammar::run::{Read, Run, set_flag, take_value};
use crate::policy::parse::Word;
use crate::policy::value::TriVal;

pub(super) fn is_option(text: &str) -> bool {
    text.starts_with('-') && text.len() > 1
}

pub(super) fn option(
    text: &str, rest: &[Word], active: &[&Opt], scan: &mut Scan, run: &mut Run,
) -> Read {
    if text == "--" {
        scan.options_end = true;
        return Read::took(0);
    }
    let word = match text.strip_prefix("--") {
        Some(w) => format!("-{w}"),
        None => text.to_string(),
    };
    let (spelling, attached) = match word.split_once('=') {
        Some((s, v)) => (s, Some(v.to_string())),
        None => (word.as_str(), None),
    };
    match active.iter().find(|o| o.single.iter().any(|s| s == spelling)) {
        Some(o) if o.value => Read::took(take_value(o, attached, rest, run)),
        Some(o) => Read::took(set_bool(o, attached, run)),
        None => Read::undeclared(attached.is_some(), rest),
    }
}

/// A flag written `-name=false` stays unset; under doubt, the word may be
/// another option's value, so what it says reads Unknown.
fn set_bool(opt: &Opt, attached: Option<String>, run: &mut Run) -> usize {
    match attached {
        Some(v) if is_false(&v) => {
            if run.doubt {
                run.parts.insert(opt.name.clone(), TriVal::Unknown);
            }
            0
        }
        _ => set_flag(opt, run),
    }
}

fn is_false(value: &str) -> bool {
    matches!(value, "0" | "f" | "F" | "false" | "FALSE" | "False")
}
