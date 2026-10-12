//! The context a command runs in: the one its *Inner command*s get from its
//! `with` keys, and its paths resolved against its cwd.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use cel_parser::ast as cel;

use crate::policy::grammar::With;
use crate::policy::parse::Context;
use crate::policy::rules::{eval_with, resolve};
use crate::policy::value::TriVal;

/// The context of a command's *Inner command*s: its own, with the fields
/// that its `with` keys set, evaluated in the order host, user, cwd. Each key
/// reads the command's args, its own cwd, user and host, and the fields
/// already evaluated. A string sets the field, null keeps it, and anything
/// else makes it Unknown. A host or user that changes leaves the home Unknown,
/// and a user that changes ends the shell's own user.
pub(super) fn inner_context(
    outer: &Context,
    cwd: &TriVal,
    args: &BTreeMap<String, TriVal>,
    with: &With,
) -> Context {
    let args = TriVal::Map(args.clone().into_iter().collect());
    // The value a key gives its field; None keeps the field.
    let value =
        |key: &Option<cel::IdedExpr>, evaluated: &HashMap<String, TriVal>, home: &TriVal| {
            let vars = HashMap::from([
                ("args".to_string(), args.clone()),
                ("cwd".to_string(), cwd.clone()),
                ("user".to_string(), outer.user.clone()),
                ("host".to_string(), outer.host.clone()),
                ("with".to_string(), TriVal::Map(evaluated.clone())),
            ]);
            match eval_with(key.as_ref()?, vars, home.clone()) {
                TriVal::Null => None,
                v @ TriVal::String(_) => Some(v),
                _ => Some(TriVal::Unknown),
            }
        };
    let same = |new: &TriVal, old: &TriVal| matches!(new, TriVal::String(_)) && new == old;

    let mut inner = outer.clone();
    let mut evaluated = HashMap::new();
    if let Some(host) = value(&with.host, &evaluated, &inner.home) {
        if !same(&host, &inner.host) {
            inner.home = TriVal::Unknown;
        }
        inner.host = host;
    }
    if with.host.is_some() {
        evaluated.insert("host".to_string(), inner.host.clone());
    }
    if let Some(user) = value(&with.user, &evaluated, &inner.home) {
        if !same(&user, &inner.user) {
            inner.home = TriVal::Unknown;
            inner.own_user = false;
        }
        inner.user = user;
    }
    if with.user.is_some() {
        evaluated.insert("user".to_string(), inner.user.clone());
    }
    if let Some(dir) = value(&with.cwd, &evaluated, &inner.home) {
        inner.cwd = join(cwd, &dir);
    }
    inner
}

/// `dir` from the directory `base`: `base` itself when `dir` is Null.
pub(super) fn join(base: &TriVal, dir: &TriVal) -> TriVal {
    match dir {
        TriVal::Null => base.clone(),
        TriVal::String(d) => resolve(d, as_dir(base)),
        _ => TriVal::Unknown,
    }
}

/// Each path of a list, from the directory `cwd`.
pub(super) fn resolved(paths: &TriVal, cwd: &TriVal) -> TriVal {
    match paths {
        TriVal::List {
            elements,
            exhaustive,
        } => TriVal::List {
            elements: elements.iter().map(|p| join(cwd, p)).collect(),
            exhaustive: *exhaustive,
        },
        _ => TriVal::Unknown,
    }
}

pub(super) fn as_dir(cwd: &TriVal) -> Option<&Path> {
    match cwd {
        TriVal::String(d) => Some(Path::new(d)),
        _ => None,
    }
}
