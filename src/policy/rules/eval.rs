//! The three-valued CEL evaluator: operators, comprehensions and functions
//! over `TriVal`, with Kleene logic.

use std::collections::HashMap;
use std::path::Path;

use cel_parser::ast as cel;
use cel_parser::reference::Val;

use super::identity::resolve;
use super::{Env, Host};
use crate::policy::programs;
use crate::policy::value::{TriBool, TriVal};

/// The variables of one CEL evaluation, where its paths resolve, and the
/// home that `home()` gives the command's own user.
#[derive(Clone)]
pub(super) struct Ctx<'a> {
    pub(super) vars: HashMap<String, TriVal>,
    pub(super) site: Option<Site<'a>>,
    pub(super) home: TriVal,
}

/// The command a rule judges: its directory and the machine it runs on.
#[derive(Clone, Copy)]
pub(super) struct Site<'a> {
    pub(super) env: &'a Env,
    pub(super) dir: Option<&'a Path>,
    pub(super) host: &'a Host,
}

/// The identifier that CEL reads as the Unknown value.
pub(super) const UNKNOWN: &str = "unknown";

pub(super) fn eval_expr(expr: &cel::Expr, ctx: &Ctx) -> TriVal {
    match expr {
        // `unknown` is a value, as `null` is.
        cel::Expr::Ident(name) if name == UNKNOWN => TriVal::Unknown,
        cel::Expr::Ident(name) => ctx.vars.get(name).cloned().unwrap_or(TriVal::Unknown),
        cel::Expr::Literal(val) => literal_to_trival(val),
        cel::Expr::Select(select) => {
            let obj = eval_expr(&select.operand.expr, ctx);
            eval_field_access(&obj, &select.field)
        }
        cel::Expr::List(list) => {
            let elements: Vec<TriVal> = list.elements.iter().map(|e| eval_expr(&e.expr, ctx)).collect();
            TriVal::List { elements, exhaustive: true }
        }
        cel::Expr::Call(call) => eval_call(call, ctx),
        cel::Expr::Comprehension(comp) => eval_comprehension(comp, ctx),
        _ => TriVal::Unknown,
    }
}

pub(super) fn tri_eq(a: &TriVal, b: &TriVal) -> TriBool {
    match (a, b) {
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriBool::Unknown,
        (TriVal::List { elements: xs, exhaustive: x_ex }, TriVal::List { elements: ys, exhaustive: y_ex }) => {
            list_eq(xs, *x_ex, ys, *y_ex)
        }
        // A list holds more than one value, so it equals no single one, and
        // a rule that compares them is unsure what it means.
        (TriVal::List { .. }, _) | (_, TriVal::List { .. }) => TriBool::Unknown,
        (TriVal::Null, TriVal::Null) => TriBool::True,
        (TriVal::Null, _) | (_, TriVal::Null) => TriBool::False,
        (TriVal::String(a), TriVal::String(b)) => TriBool::of(a == b),
        (TriVal::Int(a), TriVal::Int(b)) => TriBool::of(a == b),
        (TriVal::Bool(a), TriVal::Bool(b)) => TriBool::of(a == b),
        _ => TriBool::False,
    }
}

fn literal_to_trival(val: &Val) -> TriVal {
    match val {
        Val::String(s) => TriVal::String(s.clone()),
        Val::Int(n) => TriVal::Int(*n),
        Val::Boolean(b) => TriVal::Bool(*b),
        Val::Null => TriVal::Null,
        _ => TriVal::Unknown,
    }
}

fn eval_field_access(obj: &TriVal, field: &str) -> TriVal {
    match obj {
        TriVal::Map(map) => map.get(field).cloned().unwrap_or(TriVal::Unknown),
        TriVal::Null => TriVal::Null,
        _ => TriVal::Unknown,
    }
}

fn eval_call(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    match call.func_name.as_str() {
        "_==_" => eval_eq(&call.args, ctx),
        "_!=_" => eval_ne(&call.args, ctx),
        "_>_" => eval_cmp(&call.args, ctx, |a, b| a > b),
        "_>=_" => eval_cmp(&call.args, ctx, |a, b| a >= b),
        "_<_" => eval_cmp(&call.args, ctx, |a, b| a < b),
        "_<=_" => eval_cmp(&call.args, ctx, |a, b| a <= b),
        "_&&_" => eval_and(&call.args, ctx),
        "_||_" => eval_or(&call.args, ctx),
        "!_" => eval_not(&call.args, ctx),
        "@in" => eval_in(&call.args, ctx),
        "glob" => eval_glob(call, ctx),
        "contains" => eval_contains(call, ctx),
        "startsWith" => eval_starts_with(call, ctx),
        "matches" => eval_matches(call, ctx),
        "or" => eval_or_func(&call.args, ctx),
        "_[_]" => eval_index(&call.args, ctx),
        "_+_" => eval_add(&call.args, ctx),
        "_?_:_" => eval_ternary(&call.args, ctx),
        "size" => eval_size(call, ctx),
        "in_project" => eval_path_fn(call, ctx, in_project),
        "is_device" => eval_path_fn(call, ctx, |path, _| TriBool::of(path.starts_with("/dev")).to_trival()),
        "path" => eval_path_fn(call, ctx, |path, _| TriVal::String(path.to_string_lossy().into_owned())),
        "awk_is_plain" => eval_string_test(call, ctx, programs::awk_is_plain),
        "sed_is_plain" => eval_string_test(call, ctx, programs::sed_is_plain),
        "last" => eval_last(call, ctx),
        "host_of" => eval_destination(call, ctx, |(_, host)| Some(host)),
        "user_of" => eval_destination(call, ctx, |(user, _)| user),
        "home" => eval_home(call, ctx),
        _ => TriVal::Unknown,
    }
}

// --- Comparison operators ---

/// Element by element; a non-exhaustive list may hold more than it shows.
fn list_eq(xs: &[TriVal], x_ex: bool, ys: &[TriVal], y_ex: bool) -> TriBool {
    let mut truth = TriBool::True;
    for (x, y) in xs.iter().zip(ys) {
        truth = truth.and(tri_eq(x, y));
        if truth == TriBool::False {
            return truth;
        }
    }
    let shorter_is_complete = match xs.len().cmp(&ys.len()) {
        std::cmp::Ordering::Less => x_ex,
        std::cmp::Ordering::Greater => y_ex,
        std::cmp::Ordering::Equal => x_ex && y_ex,
    };
    match (xs.len() == ys.len(), shorter_is_complete) {
        (false, true) => TriBool::False,
        (true, true) => truth,
        _ => TriBool::Unknown.and(truth),
    }
}

fn eval_eq(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [a, b] = args else { return TriVal::Unknown };
    tri_eq(&eval_expr(&a.expr, ctx), &eval_expr(&b.expr, ctx)).to_trival()
}

fn eval_ne(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [a, b] = args else { return TriVal::Unknown };
    tri_eq(&eval_expr(&a.expr, ctx), &eval_expr(&b.expr, ctx)).not().to_trival()
}

fn eval_cmp(args: &[cel::IdedExpr], ctx: &Ctx, op: fn(i64, i64) -> bool) -> TriVal {
    let [a, b] = args else { return TriVal::Unknown };
    match (eval_expr(&a.expr, ctx), eval_expr(&b.expr, ctx)) {
        (TriVal::Int(a), TriVal::Int(b)) => TriVal::Bool(op(a, b)),
        _ => TriVal::Unknown,
    }
}

// --- Boolean operators ---

fn eval_and(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [a, b] = args else { return TriVal::Unknown };
    let a = eval_expr(&a.expr, ctx).is_truthy();
    if a == TriBool::False {
        return TriVal::Bool(false);
    }
    a.and(eval_expr(&b.expr, ctx).is_truthy()).to_trival()
}

fn eval_or(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [a, b] = args else { return TriVal::Unknown };
    let a = eval_expr(&a.expr, ctx).is_truthy();
    if a == TriBool::True {
        return TriVal::Bool(true);
    }
    a.or(eval_expr(&b.expr, ctx).is_truthy()).to_trival()
}

fn eval_not(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [a] = args else { return TriVal::Unknown };
    eval_expr(&a.expr, ctx).is_truthy().not().to_trival()
}

// --- Membership operator ---

fn eval_in(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [needle, haystack] = args else { return TriVal::Unknown };
    let needle = eval_expr(&needle.expr, ctx);
    match (&needle, eval_expr(&haystack.expr, ctx)) {
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        (_, TriVal::List { elements, exhaustive }) => {
            let mut truth = TriBool::False;
            for elem in &elements {
                truth = truth.or(tri_eq(&needle, elem));
            }
            if truth == TriBool::False && !exhaustive { TriVal::Unknown } else { truth.to_trival() }
        }
        (TriVal::String(key), TriVal::Map(map)) => TriVal::Bool(map.contains_key(key.as_str())),
        _ => TriVal::Unknown,
    }
}

// --- Comprehension (.exists(), .all(), .map(), .filter()) ---

fn eval_comprehension(comp: &cel::ComprehensionExpr, ctx: &Ctx) -> TriVal {
    let (elements, exhaustive) = match eval_expr(&comp.iter_range.expr, ctx) {
        TriVal::List { elements, exhaustive } => (elements, exhaustive),
        _ => return TriVal::Unknown,
    };

    let mut accu = eval_expr(&comp.accu_init.expr, ctx);
    for elem in &elements {
        let mut inner = ctx.clone();
        inner.vars.insert(comp.accu_var.clone(), accu.clone());
        if eval_expr(&comp.loop_cond.expr, &inner).is_truthy() == TriBool::False {
            break;
        }
        inner.vars.insert(comp.iter_var.clone(), elem.clone());
        accu = eval_expr(&comp.loop_step.expr, &inner);
    }

    // Over a non-exhaustive list only a definitive conclusion stands: True
    // for exists (accu_init false), False for all (accu_init true).
    if !exhaustive {
        let definitive = match eval_expr(&comp.accu_init.expr, ctx).is_truthy() {
            TriBool::False => TriBool::True,
            _ => TriBool::False,
        };
        if accu.is_truthy() != definitive {
            accu = TriVal::Unknown;
        }
    }

    let mut result = ctx.clone();
    result.vars.insert(comp.accu_var.clone(), accu);
    eval_expr(&comp.result.expr, &result)
}

// --- Functions ---

/// The operands of `f(a, b)` or `a.f(b)`.
fn two_operands(call: &cel::CallExpr, ctx: &Ctx) -> (TriVal, TriVal) {
    match (&call.target, &call.args[..]) {
        (Some(target), [b]) => (eval_expr(&target.expr, ctx), eval_expr(&b.expr, ctx)),
        (None, [a, b]) => (eval_expr(&a.expr, ctx), eval_expr(&b.expr, ctx)),
        _ => (TriVal::Unknown, TriVal::Unknown),
    }
}

/// The operand of `f(a)` or `a.f()`.
fn one_operand(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    match (&call.target, &call.args[..]) {
        (Some(target), []) => eval_expr(&target.expr, ctx),
        (None, [a]) => eval_expr(&a.expr, ctx),
        _ => TriVal::Unknown,
    }
}

fn string_test(a: TriVal, b: TriVal, test: impl Fn(&str, &str) -> Option<bool>) -> TriVal {
    match (a, b) {
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        (TriVal::String(a), TriVal::String(b)) => test(&a, &b).map_or(TriVal::Unknown, TriVal::Bool),
        _ => TriVal::Bool(false),
    }
}

/// `glob(pattern, text)` or `pattern.glob(text)`.
fn eval_glob(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    let (pattern, text) = two_operands(call, ctx);
    string_test(pattern, text, |p, t| Some(globset::Glob::new(p).is_ok_and(|g| g.compile_matcher().is_match(t))))
}

fn eval_contains(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    let (haystack, needle) = two_operands(call, ctx);
    string_test(haystack, needle, |h, n| Some(h.contains(n)))
}

fn eval_starts_with(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    let (s, prefix) = two_operands(call, ctx);
    string_test(s, prefix, |s, p| Some(s.starts_with(p)))
}

/// `matches(text, regex)` or `text.matches(regex)`; an invalid regex is Unknown.
fn eval_matches(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    let (text, regex) = two_operands(call, ctx);
    string_test(text, regex, |t, r| regex::Regex::new(r).ok().map(|re| re.is_match(t)))
}

fn eval_string_test(call: &cel::CallExpr, ctx: &Ctx, test: fn(&str) -> bool) -> TriVal {
    match one_operand(call, ctx) {
        TriVal::String(s) => TriVal::Bool(test(&s)),
        _ => TriVal::Unknown,
    }
}

/// A function of one resolved path. Null stays Null; a path that cannot be
/// resolved, or a call with no paths to resolve against, is Unknown.
fn eval_path_fn(call: &cel::CallExpr, ctx: &Ctx, f: fn(&Path, &Site) -> TriVal) -> TriVal {
    let Some(site) = ctx.site else { return TriVal::Unknown };
    match one_operand(call, ctx) {
        TriVal::String(word) => match resolve(&word, site.dir) {
            TriVal::String(p) => f(Path::new(&p), &site),
            other => other,
        },
        TriVal::Null => TriVal::Null,
        _ => TriVal::Unknown,
    }
}

/// Under a project directory of this machine, by whole path components.
fn in_project(path: &Path, site: &Site) -> TriVal {
    match site.host {
        Host::Here => TriVal::Bool(site.env.project_dirs.iter().any(|d| path.starts_with(d))),
        Host::There(_) => TriVal::Bool(false),
        Host::Unknown => TriVal::Unknown,
    }
}

/// `last(list)`: null past the end of an exhaustive list, Unknown past the
/// end of one that may hold more.
fn eval_last(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    match one_operand(call, ctx) {
        TriVal::List { exhaustive: false, .. } => TriVal::Unknown,
        TriVal::List { elements, .. } => elements.last().cloned().unwrap_or(TriVal::Null),
        TriVal::Null => TriVal::Null,
        _ => TriVal::Unknown,
    }
}

/// A part of an ssh destination, picked from its user and its host; null
/// when that part is absent.
fn eval_destination(
    call: &cel::CallExpr,
    ctx: &Ctx,
    pick: for<'a> fn((Option<&'a str>, &'a str)) -> Option<&'a str>,
) -> TriVal {
    match one_operand(call, ctx) {
        TriVal::String(d) => pick(destination(&d)).map_or(TriVal::Null, |s| TriVal::String(s.to_string())),
        TriVal::Null => TriVal::Null,
        _ => TriVal::Unknown,
    }
}

/// The user and the host of `host`, `user@host` or `ssh://user@host:port`.
fn destination(d: &str) -> (Option<&str>, &str) {
    let (url, rest) = match d.strip_prefix("ssh://") {
        Some(rest) => (true, rest),
        None => (false, d),
    };
    let (user, host) = match rest.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, rest),
    };
    match host.rsplit_once(':') {
        Some((h, port)) if url && port.chars().all(|c| c.is_ascii_digit()) => (user, h),
        _ => (user, host),
    }
}

/// `home(u)`: the context's home when `u` is the command's own user.
fn eval_home(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    let u = one_operand(call, ctx);
    let own = ctx.vars.get("user").cloned().unwrap_or(TriVal::Unknown);
    match (&u, tri_eq(&u, &own)) {
        (TriVal::String(_), TriBool::True) => ctx.home.clone(),
        _ => TriVal::Unknown,
    }
}

fn eval_size(call: &cel::CallExpr, ctx: &Ctx) -> TriVal {
    match one_operand(call, ctx) {
        TriVal::List { elements, exhaustive: true } => TriVal::Int(elements.len() as i64),
        TriVal::String(s) => TriVal::Int(s.len() as i64),
        _ => TriVal::Unknown,
    }
}

/// `cond ? then : else`
fn eval_ternary(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [cond, then, otherwise] = args else { return TriVal::Unknown };
    match eval_expr(&cond.expr, ctx).is_truthy() {
        TriBool::True => eval_expr(&then.expr, ctx),
        TriBool::False => eval_expr(&otherwise.expr, ctx),
        TriBool::Unknown => TriVal::Unknown,
    }
}

/// `a + b` — integer addition or list concatenation
fn eval_add(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [a, b] = args else { return TriVal::Unknown };
    match (eval_expr(&a.expr, ctx), eval_expr(&b.expr, ctx)) {
        (TriVal::Int(a), TriVal::Int(b)) => TriVal::Int(a + b),
        (TriVal::List { elements: mut a, exhaustive: a_ex }, TriVal::List { elements: b, exhaustive: b_ex }) => {
            a.extend(b);
            TriVal::List { elements: a, exhaustive: a_ex && b_ex }
        }
        _ => TriVal::Unknown,
    }
}

/// `list[index]`: past the end, Null on an exhaustive list and Unknown on a
/// non-exhaustive one.
fn eval_index(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [list, index] = args else { return TriVal::Unknown };
    match (eval_expr(&list.expr, ctx), eval_expr(&index.expr, ctx)) {
        (TriVal::List { elements, exhaustive }, TriVal::Int(i)) => match usize::try_from(i) {
            Ok(i) if i < elements.len() => elements[i].clone(),
            Ok(_) if exhaustive => TriVal::Null,
            _ => TriVal::Unknown,
        },
        (TriVal::Null, _) => TriVal::Null,
        _ => TriVal::Unknown,
    }
}

/// `or(val, fallback)` — null coalescing
fn eval_or_func(args: &[cel::IdedExpr], ctx: &Ctx) -> TriVal {
    let [val, fallback] = args else { return TriVal::Unknown };
    match eval_expr(&val.expr, ctx) {
        TriVal::Null => eval_expr(&fallback.expr, ctx),
        val => val,
    }
}
