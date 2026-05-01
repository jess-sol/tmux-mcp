//! Three-valued CEL evaluator for policy rules.
//!
//! Uses cel-parser for parsing CEL syntax into an AST, then evaluates with
//! three-valued logic (True/False/Unknown). This handles unknowable values
//! correctly: `Unknown == "root"` → Unknown, `Unknown != "root"` → Unknown.
//!
//! Two special values:
//! - Null: definite absence (e.g., no parent). `Null == X` → False, `Null != X` → True.
//! - Unknown: unknowable value (e.g., sudo -u $(expr)). All comparisons → Unknown.

use std::collections::HashMap;

use cel_parser::ast as cel;
use cel_parser::reference::Val;

use super::config::TaggedRule;
use super::parse::{CommandInfo, Effective, InnerExtraction};
use super::{Decision, PaneContext, PolicyResult};

// --- Three-valued types ---

/// A value in the three-valued type system.
#[derive(Debug, Clone, PartialEq)]
pub enum TriVal {
    String(String),
    Int(i64),
    Bool(bool),
    List { elements: Vec<TriVal>, exhaustive: bool },
    Map(HashMap<String, TriVal>),
    Null,
    Unknown,
}

impl TriVal {
    pub(super) fn is_truthy(&self) -> TriBool {
        match self {
            TriVal::Bool(true) => TriBool::True,
            TriVal::Bool(false) => TriBool::False,
            TriVal::Unknown => TriBool::Unknown,
            _ => TriBool::False,
        }
    }
}

/// Three-valued boolean for intermediate computation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TriBool {
    True,
    False,
    Unknown,
}

impl TriBool {
    fn and(self, other: TriBool) -> TriBool {
        match (self, other) {
            (TriBool::False, _) | (_, TriBool::False) => TriBool::False,
            (TriBool::True, TriBool::True) => TriBool::True,
            _ => TriBool::Unknown,
        }
    }

    fn or(self, other: TriBool) -> TriBool {
        match (self, other) {
            (TriBool::True, _) | (_, TriBool::True) => TriBool::True,
            (TriBool::False, TriBool::False) => TriBool::False,
            _ => TriBool::Unknown,
        }
    }

    fn not(self) -> TriBool {
        match self {
            TriBool::True => TriBool::False,
            TriBool::False => TriBool::True,
            TriBool::Unknown => TriBool::Unknown,
        }
    }

    fn to_trival(self) -> TriVal {
        match self {
            TriBool::True => TriVal::Bool(true),
            TriBool::False => TriVal::Bool(false),
            TriBool::Unknown => TriVal::Unknown,
        }
    }
}

// --- Compiled rules ---

pub struct CompiledRule {
    pub description: String,
    pub ast: cel::IdedExpr,
    pub action: Decision,
    pub message: Option<String>,
    /// True if the CEL expression directly references command.effective_user.
    /// When true, the implicit same-user constraint on `allow` is skipped.
    pub privilege_aware: bool,
    /// True if the CEL expression directly references command.effective_host.
    /// When true, the implicit same-host constraint on `allow` is skipped.
    pub host_aware: bool,
}

pub struct RuleSet {
    pub rules: Vec<CompiledRule>,
    pub default: Decision,
}

// --- Public API ---

/// Compile tagged rules into a RuleSet. Parses CEL expressions at load time.
pub fn compile(tagged_rules: &[TaggedRule], default: Decision) -> Result<RuleSet, String> {
    let mut rules = Vec::with_capacity(tagged_rules.len());
    for tagged in tagged_rules {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(&tagged.config.when)
            .map_err(|e| format!("CEL parse error in rule '{}': {:?}", tagged.config.description, e))?;
        let action = match tagged.config.action.as_str() {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            "ask" => Decision::Ask,
            other => return Err(format!(
                "Invalid action '{}' in rule '{}' (must be allow/ask/deny)",
                other, tagged.config.description
            )),
        };
        let privilege_aware = references_command_field(&ast.expr, "effective_user");
        let host_aware = references_command_field(&ast.expr, "effective_host");
        rules.push(CompiledRule {
            description: tagged.config.description.clone(),
            ast,
            action,
            message: tagged.config.message.clone(),
            privilege_aware,
            host_aware,
        });
    }
    Ok(RuleSet { rules, default })
}

/// Check if a CEL expression directly references `command.<field>`.
/// Walks the AST looking for Select nodes where operand is Ident("command")
/// and the field name matches. Does NOT match `command.parent.<field>`.
fn references_command_field(expr: &cel::Expr, field: &str) -> bool {
    match expr {
        cel::Expr::Select(s) => {
            if s.field == field {
                if let cel::Expr::Ident(name) = &s.operand.expr {
                    if name == "command" {
                        return true;
                    }
                }
            }
            references_command_field(&s.operand.expr, field)
        }
        cel::Expr::Call(c) => {
            c.args.iter().any(|a| references_command_field(&a.expr, field))
                || c.target.as_ref().is_some_and(|t| references_command_field(&t.expr, field))
        }
        cel::Expr::List(l) => {
            l.elements.iter().any(|e| references_command_field(&e.expr, field))
        }
        cel::Expr::Comprehension(comp) => {
            [&comp.iter_range, &comp.accu_init, &comp.loop_cond, &comp.loop_step, &comp.result]
                .iter().any(|e| references_command_field(&e.expr, field))
        }
        _ => false,
    }
}

/// Evaluate a single CommandInfo against the rule set.
/// First matching rule wins, or returns default.
///
/// For `allow` rules: an implicit context constraint applies. The rule only
/// matches if effective_user resolves to pane.user AND effective_host resolves
/// to pane.hostname. This is skipped if the rule's CEL expression directly
/// references command.effective_user or command.effective_host (the rule
/// author has explicitly considered the context).
pub fn evaluate(cmd: &CommandInfo, ctx: &PaneContext, rules: &RuleSet) -> PolicyResult {
    let eval_ctx = build_context(cmd, ctx);

    for rule in &rules.rules {
        let result = eval_expr(&rule.ast.expr, &eval_ctx);
        if result.is_truthy() == TriBool::True {
            // For `allow` rules: check implicit context constraint.
            if rule.action == Decision::Allow {
                if !rule.privilege_aware {
                    let user_ok = match &cmd.effective_user {
                        Effective::Unchanged => true,
                        Effective::Known(u) => ctx.user.as_deref() == Some(u.as_str()),
                        Effective::Unknown => false,
                    };
                    if !user_ok { continue; }
                }
                if !rule.host_aware {
                    let host_ok = match &cmd.effective_host {
                        Effective::Unchanged => true,
                        Effective::Known(h) => ctx.hostname.as_deref() == Some(h.as_str()),
                        Effective::Unknown => false,
                    };
                    if !host_ok { continue; }
                }
            }

            let rule_desc = match &rule.message {
                Some(msg) => format!("{}: {}", rule.description, msg),
                None => rule.description.clone(),
            };
            return PolicyResult {
                decision: rule.action.clone(),
                rule: rule_desc,
            };
        }
        // False or Unknown → rule doesn't match, try next
    }

    PolicyResult {
        decision: rules.default.clone(),
        rule: "default".to_string(),
    }
}

// --- Three-valued evaluator ---

pub(super) fn eval_expr(expr: &cel::Expr, ctx: &HashMap<String, TriVal>) -> TriVal {
    match expr {
        cel::Expr::Ident(name) => {
            ctx.get(name).cloned().unwrap_or(TriVal::Unknown)
        }
        cel::Expr::Literal(val) => literal_to_trival(val),
        cel::Expr::Select(select) => {
            let obj = eval_expr(&select.operand.expr, ctx);
            eval_field_access(&obj, &select.field)
        }
        cel::Expr::List(list) => {
            let elements: Vec<TriVal> = list.elements.iter()
                .map(|e| eval_expr(&e.expr, ctx))
                .collect();
            TriVal::List { elements, exhaustive: true }
        }
        cel::Expr::Call(call) => eval_call(call, ctx),
        cel::Expr::Comprehension(comp) => eval_comprehension(comp, ctx),
        cel::Expr::Map(map) => {
            let mut result = HashMap::new();
            for entry in &map.entries {
                if let cel::EntryExpr::MapEntry(me) = &entry.expr {
                    let key = eval_expr(&me.key.expr, ctx);
                    let val = eval_expr(&me.value.expr, ctx);
                    if let TriVal::String(k) = key {
                        result.insert(k, val);
                    }
                }
            }
            TriVal::Map(result)
        }
        _ => TriVal::Unknown,
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
        TriVal::Unknown => TriVal::Unknown,
        _ => TriVal::Unknown,
    }
}

fn eval_call(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    match call.func_name.as_str() {
        "_==_" => eval_eq(&call.args, ctx),
        "_!=_" => eval_ne(&call.args, ctx),
        "_&&_" => eval_and(&call.args, ctx),
        "_||_" => eval_or(&call.args, ctx),
        "!_" => eval_not(&call.args, ctx),
        "@in" => eval_in(&call.args, ctx),
        "glob" => eval_glob(call, ctx),
        "contains" => eval_contains(call, ctx),
        "startsWith" => eval_starts_with(call, ctx),
        "has_short_flag" => eval_has_short_flag(call, ctx),
        "path" => eval_path(call, ctx),
        // Wrapper extraction functions
        "getopt" => eval_getopt(call, ctx),
        "or" => eval_or_func(&call.args, ctx),
        "rsplit" => eval_rsplit(&call.args, ctx),
        "after" => eval_after(&call.args, ctx),
        "dropwhile" => eval_dropwhile(call, ctx),
        // Index access: list[n]
        "_[_]" => eval_index(&call.args, ctx),
        _ => {
            // Check for member calls: obj.method(args) where target is Some
            if let Some(target) = &call.target {
                let obj = eval_expr(&target.expr, ctx);
                // Check if it's a getopt result (Map with "operands" and "flags")
                if let TriVal::Map(ref map) = obj {
                    if map.contains_key("operands") && map.contains_key("flags") {
                        if let Some(result) = eval_getopt_method(map, &call.func_name, &call.args, ctx) {
                            return result;
                        }
                    }
                }
            }
            TriVal::Unknown
        }
    }
}

// --- Comparison operators ---

fn tri_eq(a: &TriVal, b: &TriVal) -> TriBool {
    match (a, b) {
        // Unknown propagation
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriBool::Unknown,
        // Null semantics
        (TriVal::Null, TriVal::Null) => TriBool::True,
        (TriVal::Null, _) | (_, TriVal::Null) => TriBool::False,
        // Concrete comparisons
        (TriVal::String(a), TriVal::String(b)) => if a == b { TriBool::True } else { TriBool::False },
        (TriVal::Int(a), TriVal::Int(b)) => if a == b { TriBool::True } else { TriBool::False },
        (TriVal::Bool(a), TriVal::Bool(b)) => if a == b { TriBool::True } else { TriBool::False },
        _ => TriBool::False,
    }
}

fn eval_eq(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let a = eval_expr(&args[0].expr, ctx);
    let b = eval_expr(&args[1].expr, ctx);
    tri_eq(&a, &b).to_trival()
}

fn eval_ne(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let a = eval_expr(&args[0].expr, ctx);
    let b = eval_expr(&args[1].expr, ctx);
    tri_eq(&a, &b).not().to_trival()
}

// --- Boolean operators ---

fn eval_and(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let a = eval_expr(&args[0].expr, ctx).is_truthy();
    // Short-circuit: if a is False, don't evaluate b
    if a == TriBool::False { return TriVal::Bool(false); }
    let b = eval_expr(&args[1].expr, ctx).is_truthy();
    a.and(b).to_trival()
}

fn eval_or(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let a = eval_expr(&args[0].expr, ctx).is_truthy();
    // Short-circuit: if a is True, don't evaluate b
    if a == TriBool::True { return TriVal::Bool(true); }
    let b = eval_expr(&args[1].expr, ctx).is_truthy();
    a.or(b).to_trival()
}

fn eval_not(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 1 { return TriVal::Unknown; }
    eval_expr(&args[0].expr, ctx).is_truthy().not().to_trival()
}

// --- Membership operator ---

fn eval_in(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let needle = eval_expr(&args[0].expr, ctx);
    let haystack = eval_expr(&args[1].expr, ctx);

    match (&needle, &haystack) {
        (TriVal::Unknown, _) => TriVal::Unknown,
        (_, TriVal::Unknown) => TriVal::Unknown,
        (_, TriVal::List { elements, exhaustive }) => {
            let mut found = false;
            let mut has_unknown = false;
            for elem in elements {
                match tri_eq(&needle, elem) {
                    TriBool::True => { found = true; break; }
                    TriBool::Unknown => { has_unknown = true; }
                    TriBool::False => {}
                }
            }
            if found {
                TriVal::Bool(true)
            } else if has_unknown || !exhaustive {
                TriVal::Unknown
            } else {
                TriVal::Bool(false)
            }
        }
        (_, TriVal::Map(map)) => {
            // "key in map" checks if key exists in map
            if let TriVal::String(key) = &needle {
                TriVal::Bool(map.contains_key(key.as_str()))
            } else {
                TriVal::Unknown
            }
        }
        _ => TriVal::Unknown,
    }
}

// --- Comprehension (.exists(), .all()) ---

fn eval_comprehension(comp: &cel::ComprehensionExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    let collection = eval_expr(&comp.iter_range.expr, ctx);

    let (elements, exhaustive) = match &collection {
        TriVal::List { elements, exhaustive } => (elements.clone(), *exhaustive),
        TriVal::Unknown => return TriVal::Unknown,
        _ => return TriVal::Unknown,
    };

    // Evaluate accu_init (e.g., false for exists, true for all)
    let mut accu = eval_expr(&comp.accu_init.expr, ctx);

    for elem in &elements {
        // Check loop condition
        let mut inner_ctx = ctx.clone();
        inner_ctx.insert(comp.accu_var.clone(), accu.clone());
        let cond = eval_expr(&comp.loop_cond.expr, &inner_ctx);
        if cond.is_truthy() == TriBool::False {
            break; // loop condition says stop (e.g., exists found a match)
        }

        // Execute loop step with iter_var bound
        inner_ctx.insert(comp.iter_var.clone(), elem.clone());
        accu = eval_expr(&comp.loop_step.expr, &inner_ctx);
    }

    // If the list is non-exhaustive and the accumulator hasn't reached a definitive
    // conclusion, the result is Unknown (there could be more elements).
    if !exhaustive {
        match accu.is_truthy() {
            TriBool::True => {} // Already found a match — definitive True
            _ => {
                // Haven't found a match in known elements, but there could be more
                accu = TriVal::Unknown;
            }
        }
    }

    // Apply result expression
    let mut result_ctx = ctx.clone();
    result_ctx.insert(comp.accu_var.clone(), accu);
    eval_expr(&comp.result.expr, &result_ctx)
}

// --- Custom functions ---

fn eval_glob(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    // glob(pattern, text) — can be called as glob(a, b) or a.glob(b)
    let (pattern_expr, text_expr) = if let Some(target) = &call.target {
        if call.args.len() != 1 { return TriVal::Unknown; }
        (&target.expr, &call.args[0].expr)
    } else {
        if call.args.len() != 2 { return TriVal::Unknown; }
        (&call.args[0].expr, &call.args[1].expr)
    };

    let pattern = eval_expr(pattern_expr, ctx);
    let text = eval_expr(text_expr, ctx);

    match (&pattern, &text) {
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        (TriVal::String(p), TriVal::String(t)) => {
            let matched = globset::Glob::new(p)
                .map(|g| g.compile_matcher().is_match(t.as_str()))
                .unwrap_or(false);
            TriVal::Bool(matched)
        }
        _ => TriVal::Bool(false),
    }
}

/// Extract two string args from a call (handles both `f(a, b)` and `a.f(b)` forms).
fn extract_two_args(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> (TriVal, TriVal) {
    if let Some(target) = &call.target {
        if call.args.len() >= 1 {
            return (eval_expr(&target.expr, ctx), eval_expr(&call.args[0].expr, ctx));
        }
    }
    if call.args.len() >= 2 {
        return (eval_expr(&call.args[0].expr, ctx), eval_expr(&call.args[1].expr, ctx));
    }
    (TriVal::Unknown, TriVal::Unknown)
}

fn eval_contains(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    let (haystack, needle) = extract_two_args(call, ctx);
    match (&haystack, &needle) {
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        (TriVal::String(h), TriVal::String(n)) => TriVal::Bool(h.contains(n.as_str())),
        _ => TriVal::Bool(false),
    }
}

fn eval_starts_with(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    let (string, prefix) = extract_two_args(call, ctx);
    match (&string, &prefix) {
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        (TriVal::String(s), TriVal::String(p)) => TriVal::Bool(s.starts_with(p.as_str())),
        _ => TriVal::Bool(false),
    }
}

fn eval_has_short_flag(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    if call.args.len() < 2 { return TriVal::Unknown; }
    let args_val = eval_expr(&call.args[0].expr, ctx);
    let flag_val = eval_expr(&call.args[1].expr, ctx);

    let flag_char = match &flag_val {
        TriVal::String(s) => match s.chars().next() {
            Some(c) => c,
            None => return TriVal::Bool(false),
        },
        TriVal::Unknown => return TriVal::Unknown,
        _ => return TriVal::Bool(false),
    };

    match &args_val {
        TriVal::List { elements, exhaustive } => {
            for elem in elements {
                if let TriVal::String(arg) = elem {
                    if arg.starts_with('-')
                        && !arg.starts_with("--")
                        && arg.len() > 1
                        && arg[1..].contains(flag_char)
                    {
                        return TriVal::Bool(true);
                    }
                }
            }
            if *exhaustive { TriVal::Bool(false) } else { TriVal::Unknown }
        }
        TriVal::Unknown => TriVal::Unknown,
        _ => TriVal::Bool(false),
    }
}

/// Resolve a path string relative to pane.cwd. Pure string manipulation,
/// no filesystem access. Returns Null for flags (starts with -).
fn eval_path(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    // path(arg) — single argument
    if call.args.len() != 1 { return TriVal::Unknown; }
    let arg = eval_expr(&call.args[0].expr, ctx);

    let arg_str = match &arg {
        TriVal::String(s) => s.as_str(),
        TriVal::Unknown => return TriVal::Unknown,
        TriVal::Null => return TriVal::Null,
        _ => return TriVal::Null,
    };

    // Get pane.cwd and pane.user from context
    let pane_cwd = ctx.get("pane")
        .and_then(|p| if let TriVal::Map(m) = p { m.get("cwd") } else { None })
        .and_then(|v| if let TriVal::String(s) = v { Some(s.as_str()) } else { None })
        .unwrap_or("/");

    let pane_user = ctx.get("pane")
        .and_then(|p| if let TriVal::Map(m) = p { m.get("user") } else { None })
        .and_then(|v| if let TriVal::String(s) = v { Some(s.as_str()) } else { None });

    let resolved = resolve_path(arg_str, pane_cwd, pane_user);
    match resolved {
        Some(p) => TriVal::String(p),
        None => TriVal::Null,
    }
}

/// Resolve a path string to absolute, normalizing `.` and `..` segments.
/// Pure string manipulation — no filesystem access, safe for remote systems.
fn resolve_path(arg: &str, cwd: &str, user: Option<&str>) -> Option<String> {
    use std::path::{Component, Path};

    // Flags are not paths
    if arg.starts_with('-') {
        return None;
    }

    let expanded = if let Some(rest) = arg.strip_prefix("~/") {
        // ~/foo → /home/{user}/foo
        let home = user.map(|u| format!("/home/{}", u))?;
        format!("{}/{}", home, rest)
    } else if arg == "~" {
        let home = user.map(|u| format!("/home/{}", u))?;
        home
    } else if arg.starts_with('/') {
        // Already absolute
        arg.to_string()
    } else {
        // Relative — join with cwd
        format!("{}/{}", cwd, arg)
    };

    // Normalize: resolve . and .. segments
    let mut components = Vec::new();
    for comp in Path::new(&expanded).components() {
        match comp {
            Component::RootDir => { components.clear(); components.push("/"); }
            Component::CurDir => {}
            Component::ParentDir => {
                if components.last().is_some_and(|c| *c != "/") {
                    components.pop();
                }
            }
            Component::Normal(s) => {
                components.push(s.to_str()?);
            }
            Component::Prefix(_) => {} // Windows, ignore
        }
    }

    if components.is_empty() {
        return Some("/".to_string());
    }

    let mut result = String::new();
    for (i, comp) in components.iter().enumerate() {
        if i == 0 && *comp == "/" {
            result.push('/');
        } else if i == 1 && components[0] == "/" {
            result.push_str(comp);
        } else {
            result.push('/');
            result.push_str(comp);
        }
    }

    if result.is_empty() {
        Some("/".to_string())
    } else {
        Some(result)
    }
}

// --- Wrapper extraction CEL functions ---

/// `list[index]` — index access on lists
fn eval_index(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let list = eval_expr(&args[0].expr, ctx);
    let index = eval_expr(&args[1].expr, ctx);
    match (&list, &index) {
        (TriVal::List { elements, .. }, TriVal::Int(i)) => {
            let idx = if *i < 0 { return TriVal::Null; } else { *i as usize };
            elements.get(idx).cloned().unwrap_or(TriVal::Null)
        }
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        _ => TriVal::Null,
    }
}

/// `or(val, fallback)` — return val if non-null/non-unknown, else fallback
fn eval_or_func(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let val = eval_expr(&args[0].expr, ctx);
    match &val {
        TriVal::Null => eval_expr(&args[1].expr, ctx),
        TriVal::Unknown => TriVal::Unknown,
        _ => val,
    }
}

/// `rsplit(str, sep)` → List[String] — split string by separator
/// `rsplit(str, sep, n)` → List[String/Null] — split into n parts, null-pad left
fn eval_rsplit(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() < 2 || args.len() > 3 { return TriVal::Unknown; }
    let s = eval_expr(&args[0].expr, ctx);
    let sep = eval_expr(&args[1].expr, ctx);
    let n = if args.len() == 3 {
        match eval_expr(&args[2].expr, ctx) {
            TriVal::Int(n) if n > 0 => Some(n as usize),
            _ => return TriVal::Unknown,
        }
    } else {
        None
    };

    match (&s, &sep) {
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        (TriVal::Null, _) | (_, TriVal::Null) => TriVal::Null,
        (TriVal::String(s), TriVal::String(sep)) => {
            let parts: Vec<&str> = s.split(sep.as_str()).collect();
            match n {
                None => {
                    let elements = parts.iter().map(|p| TriVal::String(p.to_string())).collect();
                    TriVal::List { elements, exhaustive: true }
                }
                Some(n) => {
                    // Null-pad left to n elements
                    let mut elements: Vec<TriVal> = Vec::with_capacity(n);
                    let pad = if n > parts.len() { n - parts.len() } else { 0 };
                    for _ in 0..pad {
                        elements.push(TriVal::Null);
                    }
                    // Take last n parts (or all if fewer)
                    let start = if parts.len() > n { parts.len() - n } else { 0 };
                    for p in &parts[start..] {
                        elements.push(TriVal::String(p.to_string()));
                    }
                    TriVal::List { elements, exhaustive: true }
                }
            }
        }
        _ => TriVal::Unknown,
    }
}

/// `after(list, element)` — everything after first occurrence of element
fn eval_after(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let list = eval_expr(&args[0].expr, ctx);
    let element = eval_expr(&args[1].expr, ctx);
    match (&list, &element) {
        (TriVal::List { elements, .. }, TriVal::String(target)) => {
            for (i, el) in elements.iter().enumerate() {
                if let TriVal::String(s) = el {
                    if s == target {
                        let rest = elements[i + 1..].to_vec();
                        return TriVal::List { elements: rest, exhaustive: true };
                    }
                }
            }
            TriVal::Null
        }
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        _ => TriVal::Null,
    }
}

/// `dropwhile(list, var, predicate)` — skip leading elements matching predicate
fn eval_dropwhile(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    // dropwhile is a comprehension-like macro: dropwhile(list, a, contains(a, "="))
    // We expect: target = list, args[0] = var name (ident), args[1] = predicate
    // But CEL parser may represent this differently. Let's handle the call form:
    // dropwhile(list_expr, ident, predicate_expr) with 3 args
    if call.args.len() != 3 { return TriVal::Unknown; }
    let list = eval_expr(&call.args[0].expr, ctx);
    let var_name = match &call.args[1].expr {
        cel::Expr::Ident(name) => name.clone(),
        _ => return TriVal::Unknown,
    };

    let elements = match &list {
        TriVal::List { elements, .. } => elements,
        TriVal::Unknown => return TriVal::Unknown,
        _ => return TriVal::Null,
    };

    let mut skip_count = 0;
    for el in elements {
        let mut inner_ctx = ctx.clone();
        inner_ctx.insert(var_name.clone(), el.clone());
        let result = eval_expr(&call.args[2].expr, &inner_ctx);
        if result.is_truthy() == TriBool::True {
            skip_count += 1;
        } else {
            break;
        }
    }

    let rest = elements[skip_count..].to_vec();
    TriVal::List { elements: rest, exhaustive: true }
}

/// `getopt(args, valued_flags)` or `getopt(args, valued_flags, terminated_flags)`
/// Returns a Map with "operands" and "flags" fields.
fn eval_getopt(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    if call.args.len() < 2 || call.args.len() > 3 { return TriVal::Unknown; }
    let args_val = eval_expr(&call.args[0].expr, ctx);
    let valued_val = eval_expr(&call.args[1].expr, ctx);
    let terminated_val = if call.args.len() == 3 {
        Some(eval_expr(&call.args[2].expr, ctx))
    } else {
        None
    };

    let (args, input_exhaustive) = match &args_val {
        TriVal::List { elements, exhaustive } => (elements, *exhaustive),
        TriVal::Unknown => return TriVal::Unknown,
        _ => return TriVal::Null,
    };
    let valued: Vec<String> = match &valued_val {
        TriVal::List { elements, .. } => elements.iter().filter_map(|e| {
            if let TriVal::String(s) = e { Some(s.clone()) } else { None }
        }).collect(),
        _ => Vec::new(),
    };
    let terminated: HashMap<String, Vec<String>> = match &terminated_val {
        Some(TriVal::Map(map)) => {
            map.iter().filter_map(|(k, v)| {
                if let TriVal::List { elements, .. } = v {
                    let terms: Vec<String> = elements.iter().filter_map(|e| {
                        if let TriVal::String(s) = e { Some(s.clone()) } else { None }
                    }).collect();
                    Some((k.clone(), terms))
                } else {
                    None
                }
            }).collect()
        }
        _ => HashMap::new(),
    };

    run_getopt(args, &valued, &terminated, input_exhaustive)
}

/// Extract the value for a flag from the next arg position, with expansion detection.
fn consume_next_value(args: &[TriVal], next_i: usize) -> TriVal {
    if next_i >= args.len() {
        return TriVal::Null;
    }
    match &args[next_i] {
        TriVal::String(s) => {
            if super::parse::word_has_expansion(s) {
                TriVal::Unknown
            } else {
                args[next_i].clone()
            }
        }
        other => other.clone(),
    }
}

/// Core getopt algorithm operating on TriVal args.
/// Returns a TriVal::Map with "operands", "flags", "terminated", and "exhaustive" fields.
///
/// `exhaustive` starts true but degrades to false when:
/// - Input args are non-exhaustive (the caller passes false)
/// - Unknown TriVals appear in the arg list
/// - Unrecognized flags are encountered (might consume the next arg)
///
/// When non-exhaustive, absent flags/positionals return Unknown instead of Null.
pub(super) fn run_getopt(
    args: &[TriVal],
    valued: &[String],
    terminated: &HashMap<String, Vec<String>>,
    mut exhaustive: bool,
) -> TriVal {
    let mut operands: Vec<TriVal> = Vec::new();
    let mut flags: HashMap<String, TriVal> = HashMap::new();
    let mut terminated_values: HashMap<String, Vec<Vec<TriVal>>> = HashMap::new();
    let mut past_flags = false;

    let mut i = 0;
    while i < args.len() {
        let arg_str = match &args[i] {
            TriVal::String(s) => s.clone(),
            TriVal::Unknown => {
                // Unknown arg in flag position: we can't tell if it's a flag
                // that consumes the next arg, or an operand. Everything from
                // here is uncertain.
                exhaustive = false;
                operands.push(TriVal::Unknown);
                i += 1;
                continue;
            }
            _ => { i += 1; continue; }
        };

        // Check terminated flags — these are recognized anywhere in the arg list
        // (e.g., find's -exec comes after path operands)
        if let Some(terminators) = terminated.get(&arg_str) {
            i += 1; // skip the flag itself
            let mut block = Vec::new();
            while i < args.len() {
                if let TriVal::String(s) = &args[i] {
                    if terminators.contains(s) {
                        i += 1; // skip terminator
                        break;
                    }
                }
                block.push(args[i].clone());
                i += 1;
            }
            terminated_values.entry(arg_str).or_default().push(block);
            continue;
        }

        // POSIX getopt: options are only processed before the first operand.
        // "-" alone is an operand (stdin convention), not a flag.
        // "--" ends option processing; everything after is operands.
        if !past_flags && arg_str.len() > 1 && arg_str.starts_with('-') {
            // "--" → end of options, advance past it, rest are operands
            if arg_str == "--" {
                i += 1;
                while i < args.len() {
                    operands.push(args[i].clone());
                    i += 1;
                }
                break;
            }

            // Long option (--flag or --flag=value)
            if arg_str.starts_with("--") {
                // Check --flag=value form
                if let Some(eq_pos) = arg_str.find('=') {
                    let flag_part = &arg_str[..eq_pos];
                    if valued.iter().any(|f| f == flag_part) {
                        let val_str = &arg_str[eq_pos + 1..];
                        let val = if super::parse::word_has_expansion(val_str) {
                            TriVal::Unknown
                        } else {
                            TriVal::String(val_str.to_string())
                        };
                        flags.insert(flag_part.to_string(), val);
                    }
                    // Unknown long --flag=val: we don't know if = is part of the
                    // flag syntax or not, but we consumed it as one arg either way,
                    // so operand boundaries are unaffected.
                    i += 1;
                } else if valued.iter().any(|f| f == &arg_str) {
                    // --flag value (separate)
                    flags.insert(arg_str, consume_next_value(args, i + 1));
                    i += 2;
                } else {
                    // Unknown long flag: we don't know if it takes a value.
                    // If it does, our operand boundaries are wrong.
                    exhaustive = false;
                    i += 1;
                }
                continue;
            }

            // Short options. First check if the full string is a known valued
            // flag (handles non-POSIX multi-char flags like find's -name).
            if valued.iter().any(|f| f == &arg_str) {
                flags.insert(arg_str, consume_next_value(args, i + 1));
                i += 2;
                continue;
            }

            // POSIX short option group: -abc
            // Process one character at a time. If a char is a valued option
            // and NOT the last char, rest of string is the option-argument
            // (-fvalue). If it IS the last char, next argv element is the
            // option-argument (-f value).
            let chars: Vec<char> = arg_str[1..].chars().collect();
            let mut ci = 0;
            let mut consumed_next = false;
            let mut found_valued = false;
            while ci < chars.len() {
                let flag_name = format!("-{}", chars[ci]);
                if valued.iter().any(|f| f == &flag_name) {
                    found_valued = true;
                    if ci + 1 < chars.len() {
                        // Attached: -fvalue → value is rest of string
                        let val_str: String = chars[ci + 1..].iter().collect();
                        let val = if super::parse::word_has_expansion(&val_str) {
                            TriVal::Unknown
                        } else {
                            TriVal::String(val_str)
                        };
                        flags.insert(flag_name, val);
                    } else {
                        // Separate: -f value → consume next arg
                        flags.insert(flag_name, consume_next_value(args, i + 1));
                        consumed_next = true;
                    }
                    break; // valued option ends the group
                }
                ci += 1;
            }
            // If we iterated without finding a valued flag, every char was unknown.
            // Any of them could take a value, making operand boundaries uncertain.
            if !found_valued {
                exhaustive = false;
            }
            i += if consumed_next { 2 } else { 1 };
            continue;
        }

        // Non-option arg = operand. After this, no more options (POSIX stop).
        past_flags = true;
        let val = match &args[i] {
            TriVal::String(s) if super::parse::word_has_expansion(s) => TriVal::Unknown,
            other => other.clone(),
        };
        operands.push(val);
        i += 1;
    }

    // Build result map
    let mut result = HashMap::new();
    result.insert("operands".into(), TriVal::List {
        elements: operands,
        exhaustive,
    });
    result.insert("flags".into(), TriVal::Map(flags));
    result.insert("exhaustive".into(), TriVal::Bool(exhaustive));

    // Add terminated flag values
    let mut tvals = HashMap::new();
    for (flag, blocks) in terminated_values {
        let block_vals: Vec<TriVal> = blocks.into_iter()
            .map(|block| TriVal::List { elements: block, exhaustive: true })
            .collect();
        tvals.insert(flag, TriVal::List { elements: block_vals, exhaustive: true });
    }
    result.insert("terminated".into(), TriVal::Map(tvals));

    TriVal::Map(result)
}

/// Member-call methods on GetoptResult: .value(flag), .values(flag),
/// .positional(n), .operands_from(n)
/// These are handled via Select + Call patterns in CEL, which we intercept
/// in eval_field_access when the field is a method name.
pub(super) fn eval_getopt_method(
    getopt_map: &HashMap<String, TriVal>,
    method: &str,
    args: &[cel::IdedExpr],
    ctx: &HashMap<String, TriVal>,
) -> Option<TriVal> {
    let exhaustive = matches!(getopt_map.get("exhaustive"), Some(TriVal::Bool(true)));
    // When non-exhaustive, absence is uncertain (Unknown), not definite (Null).
    let absent = if exhaustive { TriVal::Null } else { TriVal::Unknown };

    match method {
        "value" => {
            if args.len() != 1 { return Some(TriVal::Unknown); }
            let flag = eval_expr(&args[0].expr, ctx);
            if let TriVal::String(flag_name) = &flag {
                let flags = getopt_map.get("flags");
                if let Some(TriVal::Map(flags_map)) = flags {
                    Some(flags_map.get(flag_name).cloned().unwrap_or(absent))
                } else {
                    Some(absent)
                }
            } else {
                Some(TriVal::Unknown)
            }
        }
        "values" => {
            if args.len() != 1 { return Some(TriVal::Unknown); }
            let flag = eval_expr(&args[0].expr, ctx);
            if let TriVal::String(flag_name) = &flag {
                let terminated = getopt_map.get("terminated");
                if let Some(TriVal::Map(tmap)) = terminated {
                    Some(tmap.get(flag_name.as_str()).cloned().unwrap_or(TriVal::List {
                        elements: Vec::new(),
                        exhaustive,
                    }))
                } else {
                    Some(TriVal::List { elements: Vec::new(), exhaustive })
                }
            } else {
                Some(TriVal::Unknown)
            }
        }
        "positional" => {
            if args.len() != 1 { return Some(TriVal::Unknown); }
            let n = eval_expr(&args[0].expr, ctx);
            if let TriVal::Int(idx) = &n {
                let operands = getopt_map.get("operands");
                if let Some(TriVal::List { elements, .. }) = operands {
                    let i = if *idx < 0 { return Some(TriVal::Null); } else { *idx as usize };
                    Some(elements.get(i).cloned().unwrap_or(absent))
                } else {
                    Some(absent)
                }
            } else {
                Some(TriVal::Unknown)
            }
        }
        "operands_from" => {
            if args.len() != 1 { return Some(TriVal::Unknown); }
            let n = eval_expr(&args[0].expr, ctx);
            if let TriVal::Int(idx) = &n {
                let operands = getopt_map.get("operands");
                if let Some(TriVal::List { elements, .. }) = operands {
                    let i = if *idx < 0 { 0 } else { *idx as usize };
                    let rest = if i < elements.len() {
                        elements[i..].to_vec()
                    } else {
                        Vec::new()
                    };
                    Some(TriVal::List { elements: rest, exhaustive })
                } else {
                    Some(absent)
                }
            } else {
                Some(TriVal::Unknown)
            }
        }
        _ => None,
    }
}

// --- Context construction ---

fn build_context(cmd: &CommandInfo, pane: &PaneContext) -> HashMap<String, TriVal> {
    let mut ctx = HashMap::new();
    ctx.insert("command".to_string(), build_command_val(cmd, pane));
    ctx.insert("pane".to_string(), build_pane_val(pane));
    ctx
}

fn build_command_val(cmd: &CommandInfo, pane: &PaneContext) -> TriVal {
    let mut map = HashMap::new();

    map.insert("name".into(), TriVal::String(cmd.name.clone()));

    let args: Vec<TriVal> = cmd.args.iter()
        .map(|a| TriVal::String(a.clone()))
        .collect();
    map.insert("args".into(), TriVal::List { elements: args, exhaustive: cmd.args_complete });

    map.insert("args_complete".into(), TriVal::Bool(cmd.args_complete));
    map.insert("is_pipe_target".into(), TriVal::Bool(cmd.is_pipe_target));
    map.insert("has_inner".into(), TriVal::Bool(cmd.inner != InnerExtraction::None));

    let effective_user = match &cmd.effective_user {
        Effective::Unchanged => option_to_trival(&pane.user),
        Effective::Known(user) => TriVal::String(user.clone()),
        Effective::Unknown => TriVal::Unknown,
    };
    map.insert("effective_user".into(), effective_user);

    let effective_host = match &cmd.effective_host {
        Effective::Unchanged => option_to_trival(&pane.hostname),
        Effective::Known(host) => TriVal::String(host.clone()),
        Effective::Unknown => TriVal::Unknown,
    };
    map.insert("effective_host".into(), effective_host);

    // Redirect targets
    let write_targets: Vec<TriVal> = cmd.redirects.iter()
        .filter(|r| r.is_write)
        .map(|r| {
            if r.has_expansion { TriVal::Unknown } else { TriVal::String(r.target.clone()) }
        })
        .collect();
    map.insert("write_targets".into(), TriVal::List {
        elements: write_targets,
        exhaustive: true,
    });

    let read_targets: Vec<TriVal> = cmd.redirects.iter()
        .filter(|r| !r.is_write)
        .map(|r| {
            if r.has_expansion { TriVal::Unknown } else { TriVal::String(r.target.clone()) }
        })
        .collect();
    map.insert("read_targets".into(), TriVal::List {
        elements: read_targets,
        exhaustive: true,
    });

    let parent = match &cmd.parent {
        Some(parent) => build_command_val(parent, pane),
        None => TriVal::Null,
    };
    map.insert("parent".into(), parent);

    TriVal::Map(map)
}

fn option_to_trival(opt: &Option<String>) -> TriVal {
    match opt {
        Some(s) => TriVal::String(s.clone()),
        None => TriVal::Null,
    }
}

fn build_pane_val(pane: &PaneContext) -> TriVal {
    let mut map = HashMap::new();
    map.insert("hostname".into(), option_to_trival(&pane.hostname));
    map.insert("cwd".into(), option_to_trival(&pane.cwd));
    map.insert("foreground".into(), option_to_trival(&pane.foreground));
    map.insert("user".into(), option_to_trival(&pane.user));
    TriVal::Map(map)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use super::*;
    use crate::policy::parse::Effective;

    fn cmd(name: &str) -> CommandInfo {
        CommandInfo::simple(name)
    }

    fn cmd_with_args(name: &str, args: &[&str]) -> CommandInfo {
        CommandInfo {
            args: args.iter().map(|s| s.to_string()).collect(),
            ..CommandInfo::simple(name)
        }
    }

    fn local_pane() -> PaneContext {
        PaneContext {
            hostname: None,
            cwd: Some("/home/user".into()),
            foreground: Some("bash".into()),
            user: Some("jess".into()),
        }
    }

    fn remote_pane(host: &str) -> PaneContext {
        PaneContext {
            hostname: Some(host.into()),
            ..local_pane()
        }
    }

    fn compile_rules(specs: &[(&str, &str, &str)]) -> RuleSet {
        let tagged: Vec<TaggedRule> = specs.iter().enumerate().map(|(i, (desc, when, action))| {
            TaggedRule {
                config: crate::policy::config::RuleConfig {
                    description: desc.to_string(),
                    when: when.to_string(),
                    action: action.to_string(),
                    order: 0,
                    message: None,
                },
                source: crate::policy::config::RuleSource::Builtin,
                source_index: i,
            }
        }).collect();
        compile(&tagged, Decision::Ask).unwrap()
    }

    // --- Dependency contract: cel-parser ---

    #[test]
    fn parser_handles_equality() {
        let ast = cel_parser::Parser::new().parse(r#"x == "hello""#).unwrap();
        match &ast.expr {
            cel::Expr::Call(c) => assert_eq!(c.func_name, "_==_"),
            _ => panic!("expected CallExpr"),
        }
    }

    #[test]
    fn parser_handles_boolean_and() {
        let ast = cel_parser::Parser::new().parse("a && b").unwrap();
        match &ast.expr {
            cel::Expr::Call(c) => assert_eq!(c.func_name, "_&&_"),
            _ => panic!("expected CallExpr"),
        }
    }

    #[test]
    fn parser_handles_in_operator() {
        let ast = cel_parser::Parser::new().parse(r#"x in ["a","b"]"#).unwrap();
        match &ast.expr {
            cel::Expr::Call(c) => assert_eq!(c.func_name, "@in"),
            _ => panic!("expected CallExpr"),
        }
    }

    #[test]
    fn parser_handles_field_access() {
        let ast = cel_parser::Parser::new().parse("a.b.c").unwrap();
        match &ast.expr {
            cel::Expr::Select(s) => {
                assert_eq!(s.field, "c");
                match &s.operand.expr {
                    cel::Expr::Select(s2) => assert_eq!(s2.field, "b"),
                    _ => panic!("expected nested SelectExpr"),
                }
            }
            _ => panic!("expected SelectExpr"),
        }
    }

    #[test]
    fn parser_handles_exists() {
        let ast = cel_parser::Parser::new().parse(r#"xs.exists(x, x == "a")"#).unwrap();
        assert!(matches!(&ast.expr, cel::Expr::Comprehension(_)));
    }

    #[test]
    fn parser_rejects_invalid_syntax() {
        assert!(cel_parser::Parser::new().parse("invalid @@@ syntax").is_err());
    }

    // --- Basic matching ---

    #[test]
    fn name_equals_matches() {
        let rules = compile_rules(&[("ls rule", r#"command.name == "ls""#, "allow")]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn name_equals_no_match() {
        let rules = compile_rules(&[("ls rule", r#"command.name == "ls""#, "allow")]);
        assert_eq!(evaluate(&cmd("cat"), &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn name_in_list_matches() {
        let rules = compile_rules(&[("tools", r#"command.name in ["ls","cat","grep"]"#, "allow")]);
        assert_eq!(evaluate(&cmd("cat"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn args_exists_matches() {
        let rules = compile_rules(&[
            ("rm -rf", r#"command.name == "rm" && command.args.exists(a, a == "-rf")"#, "ask"),
        ]);
        assert_eq!(evaluate(&cmd_with_args("rm", &["-rf", "/"]), &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn args_exists_no_match() {
        let rules = compile_rules(&[
            ("rm -rf", r#"command.name == "rm" && command.args.exists(a, a == "-rf")"#, "deny"),
            ("rm allow", r#"command.name == "rm""#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd_with_args("rm", &["-v", "file"]), &local_pane(), &rules).decision, Decision::Allow);
    }

    // --- First match wins ---

    #[test]
    fn first_rule_wins_on_match() {
        let rules = compile_rules(&[
            ("allow ls", r#"command.name == "ls""#, "allow"),
            ("deny ls", r#"command.name == "ls""#, "deny"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn later_rule_used_if_first_doesnt_match() {
        let rules = compile_rules(&[
            ("allow cat", r#"command.name == "cat""#, "allow"),
            ("deny ls", r#"command.name == "ls""#, "deny"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Deny);
    }

    #[test]
    fn no_match_returns_default() {
        let rules = compile_rules(&[("allow cat", r#"command.name == "cat""#, "allow")]);
        let r = evaluate(&cmd("ls"), &local_pane(), &rules);
        assert_eq!(r.decision, Decision::Ask);
        assert_eq!(r.rule, "default");
    }

    // --- Unknown (unknowable value) ---

    #[test]
    fn unknown_user_equality_is_unknown() {
        let rules = compile_rules(&[
            ("root check", r#"command.effective_user == "root""#, "deny"),
        ]);
        let mut c = cmd("rm");
        c.effective_user = Effective::Unknown;
        // Unknown == "root" → Unknown → no match → default Ask
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn unknown_user_inequality_is_unknown() {
        let rules = compile_rules(&[
            ("not root", r#"command.effective_user != "root""#, "allow"),
        ]);
        let mut c = cmd("rm");
        c.effective_user = Effective::Unknown;
        // Unknown != "root" → Unknown → no match → default Ask
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn known_user_equals_matches() {
        let rules = compile_rules(&[
            ("root check", r#"command.effective_user == "root""#, "deny"),
        ]);
        let mut c = cmd("rm");
        c.effective_user = Effective::Known("root".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Deny);
    }

    #[test]
    fn unchanged_user_resolves_to_pane_user() {
        let rules = compile_rules(&[
            ("jess check", r#"command.effective_user == "jess""#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn unknown_in_or_with_known_true() {
        // True || Unknown → True (OR short-circuit)
        let rules = compile_rules(&[
            ("or test", r#"command.name == "ls" || command.effective_user == "root""#, "allow"),
        ]);
        let mut c = cmd("ls");
        c.effective_user = Effective::Unknown;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn unknown_in_and_with_known_true() {
        // True && Unknown → Unknown → no match
        let rules = compile_rules(&[
            ("and test", r#"command.name == "rm" && command.effective_user == "root""#, "deny"),
        ]);
        let mut c = cmd("rm");
        c.effective_user = Effective::Unknown;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    // --- Null (definite absence) ---

    #[test]
    fn null_equality_is_false() {
        // parent is Null → parent.name == "sudo" → Null == "sudo" → False
        let rules = compile_rules(&[
            ("sudo parent", r#"command.parent.name == "sudo""#, "deny"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn null_inequality_is_true() {
        // parent is Null → parent.name != "sudo" → Null != "sudo" → True
        let rules = compile_rules(&[
            ("not sudo parent", r#"command.parent.name != "sudo""#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn null_equals_null() {
        // parent is Null → parent == null → Null == Null → True
        let rules = compile_rules(&[
            ("no parent", r#"command.parent == null"#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn parent_not_null_when_present() {
        let rules = compile_rules(&[
            ("has parent", r#"command.parent != null"#, "deny"),
        ]);
        let mut c = cmd("rm");
        c.parent = Some(Arc::new(cmd("sudo")));
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Deny);
    }

    #[test]
    fn null_field_access_propagates() {
        // parent.name.something → Null.name → Null, Null.something → Null
        let rules = compile_rules(&[
            ("deep null", r#"command.parent.name == "test""#, "deny"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Ask);
    }

    // --- Null vs Unknown are distinct ---

    #[test]
    fn null_ne_produces_true() {
        // Null != "x" → True (definite: nothing isn't "x")
        let rules = compile_rules(&[
            ("null ne", r#"command.parent.name != "x""#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn unknown_ne_produces_unknown() {
        // Unknown != "x" → Unknown (uncertain: could be "x")
        let rules = compile_rules(&[
            ("unknown ne", r#"command.effective_user != "root""#, "allow"),
        ]);
        let mut c = cmd("ls");
        c.effective_user = Effective::Unknown;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    // --- Non-exhaustive list ---

    #[test]
    fn nonexhaustive_exists_found_is_true() {
        let rules = compile_rules(&[
            ("has -v", r#"command.args.exists(a, a == "-v")"#, "allow"),
        ]);
        let mut c = cmd_with_args("rm", &["-v"]);
        c.args_complete = false;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn nonexhaustive_exists_not_found_is_unknown() {
        let rules = compile_rules(&[
            ("has -rf", r#"command.args.exists(a, a == "-rf")"#, "deny"),
        ]);
        let mut c = cmd_with_args("rm", &["-v"]);
        c.args_complete = false;
        // exists returns Unknown (not found in known, list non-exhaustive) → no match
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn exhaustive_exists_not_found_is_false() {
        let rules = compile_rules(&[
            ("has -rf", r#"command.args.exists(a, a == "-rf")"#, "deny"),
            ("rm allow", r#"command.name == "rm""#, "allow"),
        ]);
        let c = cmd_with_args("rm", &["-v"]);
        // exhaustive list, -rf not found → False → try next rule → allow
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn nonexhaustive_in_found_is_true() {
        let rules = compile_rules(&[
            ("v in list", r#""-v" in command.args"#, "allow"),
        ]);
        let mut c = cmd_with_args("rm", &["-v"]);
        c.args_complete = false;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn nonexhaustive_in_not_found_is_unknown() {
        let rules = compile_rules(&[
            ("rf in args", r#""-rf" in command.args"#, "deny"),
        ]);
        let mut c = cmd_with_args("rm", &["-v"]);
        c.args_complete = false;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn nonexhaustive_not_exists_is_unknown() {
        let rules = compile_rules(&[
            ("no -rf", r#"!(command.args.exists(a, a == "-rf"))"#, "allow"),
        ]);
        let mut c = cmd_with_args("rm", &["-v"]);
        c.args_complete = false;
        // !Unknown → Unknown → no match
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    // --- Boolean propagation (Kleene) ---

    #[test]
    fn false_and_unknown_is_false() {
        let rules = compile_rules(&[
            ("test", r#"command.name == "cat" && command.effective_user == "root""#, "deny"),
        ]);
        let mut c = cmd("rm");  // name != "cat" → False
        c.effective_user = Effective::Unknown;
        // False && Unknown → False → no match
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    // --- Pane context ---

    #[test]
    fn pane_hostname_matches() {
        let rules = compile_rules(&[
            ("prod host", r#"pane.hostname == "prod-server""#, "deny"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &remote_pane("prod-server"), &rules).decision, Decision::Deny);
    }

    #[test]
    fn pane_cwd_matches() {
        let rules = compile_rules(&[
            ("etc", r#"pane.cwd == "/etc""#, "deny"),
        ]);
        let pane = PaneContext { cwd: Some("/etc".into()), ..local_pane() };
        assert_eq!(evaluate(&cmd("ls"), &pane, &rules).decision, Decision::Deny);
    }

    // --- Pipe target ---

    #[test]
    fn pipe_target_matches() {
        let rules = compile_rules(&[
            ("pipe to bash", r#"command.name == "bash" && command.is_pipe_target"#, "deny"),
        ]);
        let mut c = cmd("bash");
        c.is_pipe_target = true;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Deny);
    }

    #[test]
    fn non_pipe_target_no_match() {
        let rules = compile_rules(&[
            ("pipe to bash", r#"command.name == "bash" && command.is_pipe_target"#, "deny"),
        ]);
        assert_eq!(evaluate(&cmd("bash"), &local_pane(), &rules).decision, Decision::Ask);
    }

    // --- Glob ---

    #[test]
    fn glob_matches_wildcard() {
        let rules = compile_rules(&[
            ("prod", r#"glob("*.prod.*", command.effective_host)"#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("web.prod.example.com".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Deny);
    }

    #[test]
    fn glob_no_match() {
        let rules = compile_rules(&[
            ("prod", r#"glob("*.prod.*", command.effective_host)"#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("web.staging.example.com".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn glob_with_unknown_host() {
        let rules = compile_rules(&[
            ("prod", r#"glob("*.prod.*", command.effective_host)"#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Unknown;
        // Unknown host → glob returns Unknown → no match
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    // --- Compile errors ---

    #[test]
    fn compile_error_reported() {
        let tagged = vec![TaggedRule {
            config: crate::policy::config::RuleConfig {
                description: "bad rule".into(),
                when: "invalid @@@ syntax".into(),
                action: "allow".into(),
                order: 0,
                message: None,
            },
            source: crate::policy::config::RuleSource::Builtin,
            source_index: 0,
        }];
        match compile(&tagged, Decision::Ask) {
            Ok(_) => panic!("expected compile error"),
            Err(err) => assert!(err.contains("bad rule"), "error was: {}", err),
        }
    }

    #[test]
    fn invalid_action_reported() {
        let tagged = vec![TaggedRule {
            config: crate::policy::config::RuleConfig {
                description: "bad action".into(),
                when: "true".into(),
                action: "invalid".into(),
                order: 0,
                message: None,
            },
            source: crate::policy::config::RuleSource::Builtin,
            source_index: 0,
        }];
        match compile(&tagged, Decision::Ask) {
            Ok(_) => panic!("expected compile error"),
            Err(err) => assert!(err.contains("invalid"), "error was: {}", err),
        }
    }

    // --- Builtin rules compile ---

    #[test]
    fn builtin_rules_all_compile() {
        let (default, tagged, _) = crate::policy::config::load_merged_config(None);
        if let Err(err) = compile(&tagged, default) {
            panic!("builtin rules failed to compile: {}", err);
        }
    }

    // --- Unknown host ---

    #[test]
    fn unknown_host_equals_returns_unknown() {
        let rules = compile_rules(&[
            ("prod check", r#"command.effective_host == "prod""#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Unknown;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn known_host_equals_matches() {
        let rules = compile_rules(&[
            ("prod check", r#"command.effective_host == "prod""#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("prod".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Deny);
    }

    #[test]
    fn unchanged_host_resolves_to_pane_hostname() {
        let rules = compile_rules(&[
            ("staging", r#"command.effective_host == "staging.example.com""#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &remote_pane("staging.example.com"), &rules).decision, Decision::Allow);
    }

    #[test]
    fn local_pane_host_is_null() {
        let rules = compile_rules(&[
            ("local", r#"command.effective_host == null"#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd("ls"), &local_pane(), &rules).decision, Decision::Allow);
    }

    // --- path() function ---

    #[test]
    fn resolve_path_absolute() {
        assert_eq!(resolve_path("/etc/passwd", "/home/user/project", Some("user")), Some("/etc/passwd".into()));
    }

    #[test]
    fn resolve_path_relative() {
        assert_eq!(resolve_path("src/main.rs", "/home/user/project", Some("user")), Some("/home/user/project/src/main.rs".into()));
    }

    #[test]
    fn resolve_path_dotdot_traversal() {
        assert_eq!(resolve_path("../../.ssh/id_rsa", "/home/user/project", Some("user")), Some("/home/.ssh/id_rsa".into()));
    }

    #[test]
    fn resolve_path_dotdot_at_root() {
        assert_eq!(resolve_path("/../../../etc/shadow", "/", Some("user")), Some("/etc/shadow".into()));
    }

    #[test]
    fn resolve_path_tilde() {
        assert_eq!(resolve_path("~/.ssh/id_rsa", "/home/user/project", Some("user")), Some("/home/user/.ssh/id_rsa".into()));
    }

    #[test]
    fn resolve_path_tilde_alone() {
        assert_eq!(resolve_path("~", "/home/user/project", Some("user")), Some("/home/user".into()));
    }

    #[test]
    fn resolve_path_tilde_no_user() {
        assert_eq!(resolve_path("~/.ssh/id_rsa", "/home/user/project", None), None);
    }

    #[test]
    fn resolve_path_dot_current() {
        assert_eq!(resolve_path("./README.md", "/home/user/project", Some("user")), Some("/home/user/project/README.md".into()));
    }

    #[test]
    fn resolve_path_bare_filename() {
        assert_eq!(resolve_path("Cargo.toml", "/home/user/project", Some("user")), Some("/home/user/project/Cargo.toml".into()));
    }

    #[test]
    fn resolve_path_flag_returns_none() {
        assert_eq!(resolve_path("-rf", "/home/user/project", Some("user")), None);
    }

    #[test]
    fn resolve_path_long_flag_returns_none() {
        assert_eq!(resolve_path("--verbose", "/home/user/project", Some("user")), None);
    }

    #[test]
    fn resolve_path_normalizes_dots() {
        assert_eq!(resolve_path("/a/b/./c/../d", "/", Some("user")), Some("/a/b/d".into()));
    }

    // --- path() in CEL rules (integration) ---

    #[test]
    fn path_cel_in_project_allowed() {
        let rules = compile_rules(&[
            ("in-project", r#"command.name == "cat" && !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), pane.cwd))"#, "allow"),
        ]);
        let c = cmd_with_args("cat", &["src/main.rs"]);
        let pane = PaneContext { cwd: Some("/home/user/project".into()), ..local_pane() };
        assert_eq!(evaluate(&c, &pane, &rules).decision, Decision::Allow);
    }

    #[test]
    fn path_cel_out_of_project_asks() {
        let rules = compile_rules(&[
            ("in-project", r#"command.name == "cat" && !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), pane.cwd))"#, "allow"),
        ]);
        let c = cmd_with_args("cat", &["/etc/passwd"]);
        let pane = PaneContext { cwd: Some("/home/user/project".into()), ..local_pane() };
        assert_eq!(evaluate(&c, &pane, &rules).decision, Decision::Ask);
    }

    #[test]
    fn path_cel_traversal_caught() {
        let rules = compile_rules(&[
            ("in-project", r#"command.name == "cat" && !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), pane.cwd))"#, "allow"),
        ]);
        let c = cmd_with_args("cat", &["../../.ssh/id_rsa"]);
        let pane = PaneContext { cwd: Some("/home/user/project".into()), ..local_pane() };
        assert_eq!(evaluate(&c, &pane, &rules).decision, Decision::Ask);
    }

    #[test]
    fn path_cel_tilde_caught() {
        let rules = compile_rules(&[
            ("in-project", r#"command.name == "cat" && !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), pane.cwd))"#, "allow"),
        ]);
        let c = cmd_with_args("cat", &["~/.ssh/id_ed25519"]);
        let pane = PaneContext { cwd: Some("/home/user/project".into()), user: Some("user".into()), ..local_pane() };
        assert_eq!(evaluate(&c, &pane, &rules).decision, Decision::Ask);
    }

    #[test]
    fn path_cel_flag_args_ignored() {
        let rules = compile_rules(&[
            ("in-project", r#"command.name == "cat" && !command.args.exists(a, !startsWith(a, "-") && !startsWith(path(a), pane.cwd))"#, "allow"),
        ]);
        let c = cmd_with_args("cat", &["-n", "src/main.rs"]);
        let pane = PaneContext { cwd: Some("/home/user/project".into()), ..local_pane() };
        assert_eq!(evaluate(&c, &pane, &rules).decision, Decision::Allow);
    }

    // --- AST walker: references_command_field ---

    #[test]
    fn detects_command_effective_user() {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(r#"command.effective_user == "root""#).unwrap();
        assert!(references_command_field(&ast.expr, "effective_user"));
    }

    #[test]
    fn does_not_detect_parent_effective_user() {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(r#"command.parent.effective_user == "root""#).unwrap();
        assert!(!references_command_field(&ast.expr, "effective_user"));
    }

    #[test]
    fn detects_effective_user_in_complex_expr() {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(r#"command.name == "cargo" && command.effective_user == "root""#).unwrap();
        assert!(references_command_field(&ast.expr, "effective_user"));
    }

    #[test]
    fn does_not_detect_unrelated_field() {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(r#"command.name == "cargo""#).unwrap();
        assert!(!references_command_field(&ast.expr, "effective_user"));
    }

    #[test]
    fn detects_effective_host() {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(r#"command.effective_host == "prod""#).unwrap();
        assert!(references_command_field(&ast.expr, "effective_host"));
    }

    // --- Implicit allow constraint ---

    #[test]
    fn allow_skipped_when_user_differs() {
        let rules = compile_rules(&[
            ("cargo", r#"command.name == "cargo""#, "allow"),
        ]);
        let mut c = cmd("cargo");
        c.effective_user = Effective::Known("root".into());
        // Rule matches CEL but implicit constraint fails (root != jess)
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn allow_passes_when_user_unchanged() {
        let rules = compile_rules(&[
            ("cargo", r#"command.name == "cargo""#, "allow"),
        ]);
        assert_eq!(evaluate(&cmd("cargo"), &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn allow_passes_when_user_same() {
        let rules = compile_rules(&[
            ("cargo", r#"command.name == "cargo""#, "allow"),
        ]);
        let mut c = cmd("cargo");
        c.effective_user = Effective::Known("jess".into()); // same as pane.user
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn allow_skipped_when_user_unknown() {
        let rules = compile_rules(&[
            ("cargo", r#"command.name == "cargo""#, "allow"),
        ]);
        let mut c = cmd("cargo");
        c.effective_user = Effective::Unknown;
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn privilege_aware_rule_allows_different_user() {
        let rules = compile_rules(&[
            ("cargo root", r#"command.name == "cargo" && command.effective_user == "root""#, "allow"),
        ]);
        let mut c = cmd("cargo");
        c.effective_user = Effective::Known("root".into());
        // Rule is privilege-aware (references command.effective_user), so constraint skipped
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn allow_skipped_when_host_differs() {
        let rules = compile_rules(&[
            ("ls", r#"command.name == "ls""#, "allow"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("server".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn host_aware_rule_allows_different_host() {
        let rules = compile_rules(&[
            ("ls staging", r#"command.name == "ls" && command.effective_host == "staging""#, "allow"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("staging".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Allow);
    }

    #[test]
    fn ask_rule_not_affected_by_constraint() {
        // ask/deny rules should NOT have implicit constraint
        let rules = compile_rules(&[
            ("rm", r#"command.name == "rm""#, "ask"),
        ]);
        let mut c = cmd("rm");
        c.effective_user = Effective::Known("root".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Ask);
    }

    #[test]
    fn deny_rule_not_affected_by_constraint() {
        let rules = compile_rules(&[
            ("eval", r#"command.name == "eval""#, "deny"),
        ]);
        let mut c = cmd("eval");
        c.effective_user = Effective::Known("root".into());
        assert_eq!(evaluate(&c, &local_pane(), &rules).decision, Decision::Deny);
    }

    // --- getopt ---

    fn s(val: &str) -> TriVal { TriVal::String(val.to_string()) }
    fn args(vals: &[&str]) -> Vec<TriVal> { vals.iter().map(|v| s(v)).collect() }

    fn getopt_operands(result: &TriVal) -> Vec<String> {
        if let TriVal::Map(m) = result {
            if let Some(TriVal::List { elements, .. }) = m.get("operands") {
                return elements.iter().filter_map(|e| {
                    if let TriVal::String(s) = e { Some(s.clone()) } else { None }
                }).collect();
            }
        }
        panic!("not a getopt result: {:?}", result);
    }

    fn getopt_flag(result: &TriVal, flag: &str) -> TriVal {
        if let TriVal::Map(m) = result {
            if let Some(TriVal::Map(flags)) = m.get("flags") {
                let exhaustive = matches!(m.get("exhaustive"), Some(TriVal::Bool(true)));
                let absent = if exhaustive { TriVal::Null } else { TriVal::Unknown };
                return flags.get(flag).cloned().unwrap_or(absent);
            }
        }
        panic!("not a getopt result: {:?}", result);
    }

    fn getopt_terminated(result: &TriVal, flag: &str) -> Vec<Vec<String>> {
        if let TriVal::Map(m) = result {
            if let Some(TriVal::Map(tmap)) = m.get("terminated") {
                if let Some(TriVal::List { elements, .. }) = tmap.get(flag) {
                    return elements.iter().map(|block| {
                        if let TriVal::List { elements: items, .. } = block {
                            items.iter().filter_map(|e| {
                                if let TriVal::String(s) = e { Some(s.clone()) } else { None }
                            }).collect()
                        } else {
                            vec![]
                        }
                    }).collect();
                }
            }
        }
        vec![]
    }

    fn no_terminated() -> HashMap<String, Vec<String>> { HashMap::new() }

    // Test wrapper: default exhaustive=true (known complete args)
    fn getopt(args: &[TriVal], valued: &[String], terminated: &HashMap<String, Vec<String>>) -> TriVal {
        run_getopt(args, valued, terminated, true)
    }

    fn is_exhaustive(result: &TriVal) -> bool {
        if let TriVal::Map(m) = result {
            matches!(m.get("exhaustive"), Some(TriVal::Bool(true)))
        } else {
            panic!("not a getopt result");
        }
    }

    // --- Core getopt: operands and flags ---

    #[test]
    fn getopt_no_flags_all_operands() {
        let r = getopt(&args(&["ls", "-la", "/"]), &[], &no_terminated());
        assert_eq!(getopt_operands(&r), vec!["ls", "-la", "/"]);
    }

    #[test]
    fn getopt_empty_args() {
        let r = getopt(&[], &[], &no_terminated());
        assert_eq!(getopt_operands(&r), Vec::<String>::new());
    }

    #[test]
    fn getopt_valued_flag_consumed() {
        let r = getopt(
            &args(&["-n", "10", "cargo", "build"]),
            &["-n".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["cargo", "build"]);
        assert_eq!(getopt_flag(&r, "-n"), s("10"));
    }

    #[test]
    fn getopt_standalone_flag_skipped() {
        let r = getopt(
            &args(&["-v", "-r", "file.txt"]),
            &[],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["file.txt"]);
    }

    #[test]
    fn getopt_multiple_valued_flags() {
        let r = getopt(
            &args(&["-u", "root", "-C", "3", "rm", "-rf", "/"]),
            &["-u".into(), "-C".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["rm", "-rf", "/"]);
        assert_eq!(getopt_flag(&r, "-u"), s("root"));
        assert_eq!(getopt_flag(&r, "-C"), s("3"));
    }

    #[test]
    fn getopt_absent_flag_returns_null() {
        let r = getopt(
            &args(&["cargo", "build"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert!(matches!(getopt_flag(&r, "-u"), TriVal::Null));
    }

    // --- POSIX stop behavior ---

    #[test]
    fn getopt_posix_stop_at_first_operand() {
        // After "rm", "-rf" should be an operand, not a flag
        let r = getopt(
            &args(&["-u", "root", "rm", "-rf", "/"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn getopt_flags_after_operand_become_operands() {
        let r = getopt(
            &args(&["cmd", "-n", "10"]),
            &["-n".into()],
            &no_terminated(),
        );
        // cmd is first operand, then -n and 10 are also operands (past POSIX stop)
        assert_eq!(getopt_operands(&r), vec!["cmd", "-n", "10"]);
    }

    // --- Double dash separator ---

    #[test]
    fn getopt_double_dash_ends_flags() {
        let r = getopt(
            &args(&["-n", "5", "--", "-rf", "file"]),
            &["-n".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["-rf", "file"]);
        assert_eq!(getopt_flag(&r, "-n"), s("5"));
    }

    #[test]
    fn getopt_double_dash_only() {
        let r = getopt(&args(&["--"]), &[], &no_terminated());
        assert_eq!(getopt_operands(&r), Vec::<String>::new());
    }

    #[test]
    fn getopt_double_dash_before_anything() {
        let r = getopt(
            &args(&["--", "-n", "10", "cmd"]),
            &["-n".into()],
            &no_terminated(),
        );
        // Everything after -- is operands, -n NOT consumed as flag
        assert_eq!(getopt_operands(&r), vec!["-n", "10", "cmd"]);
        assert!(matches!(getopt_flag(&r, "-n"), TriVal::Null));
    }

    // --- Valued flag edge cases ---

    #[test]
    fn getopt_valued_flag_at_end_missing_value() {
        let r = getopt(
            &args(&["-u"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert!(matches!(getopt_flag(&r, "-u"), TriVal::Null));
        assert_eq!(getopt_operands(&r), Vec::<String>::new());
    }

    #[test]
    fn getopt_valued_flag_value_is_dash_prefixed() {
        // -u takes next arg even if it looks like a flag
        let r = getopt(
            &args(&["-u", "-1", "cmd"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-u"), s("-1"));
        assert_eq!(getopt_operands(&r), vec!["cmd"]);
    }

    // --- Auto-unknown (expansion detection) ---

    #[test]
    fn getopt_flag_value_with_expansion_is_unknown() {
        let r = getopt(
            &args(&["-u", "$(whoami)", "cmd"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert!(matches!(getopt_flag(&r, "-u"), TriVal::Unknown));
        assert_eq!(getopt_operands(&r), vec!["cmd"]);
    }

    #[test]
    fn getopt_operand_with_expansion_is_unknown() {
        let r = getopt(
            &args(&["$HOME/file"]),
            &[],
            &no_terminated(),
        );
        let TriVal::Map(m) = &r else { panic!() };
        let TriVal::List { elements, .. } = m.get("operands").unwrap() else { panic!() };
        assert!(matches!(&elements[0], TriVal::Unknown));
    }

    #[test]
    fn getopt_unknown_trival_in_args_becomes_unknown_operand() {
        let mut a = args(&["-n", "5"]);
        a.push(TriVal::Unknown);
        a.push(s("cmd"));
        let r = getopt(&a, &["-n".into()], &no_terminated());
        let ops = &getopt_operands(&r);
        // Unknown pushed as operand, then "cmd" too (past POSIX stop due to Unknown)
        assert_eq!(ops, &["cmd"]);
        // Check the Unknown is there
        let TriVal::Map(m) = &r else { panic!() };
        let TriVal::List { elements, .. } = m.get("operands").unwrap() else { panic!() };
        assert!(matches!(&elements[0], TriVal::Unknown));
    }

    // --- Terminated flags ---

    #[test]
    fn getopt_terminated_single_block() {
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into(), "+".into()]);
        let r = getopt(
            &args(&[".", "-exec", "grep", "foo", "{}", ";"]),
            &[],
            &term,
        );
        assert_eq!(getopt_terminated(&r, "-exec"), vec![vec!["grep", "foo", "{}"]]);
        assert_eq!(getopt_operands(&r), vec!["."]);
    }

    #[test]
    fn getopt_terminated_multiple_blocks() {
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into()]);
        let r = getopt(
            &args(&[".", "-exec", "grep", "foo", "{}", ";", "-exec", "rm", "{}", ";"]),
            &[],
            &term,
        );
        let blocks = getopt_terminated(&r, "-exec");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], vec!["grep", "foo", "{}"]);
        assert_eq!(blocks[1], vec!["rm", "{}"]);
    }

    #[test]
    fn getopt_terminated_after_operands() {
        // find-style: path operands come before -exec
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into()]);
        let r = getopt(
            &args(&["/var", "/tmp", "-exec", "ls", "{}", ";"]),
            &[],
            &term,
        );
        assert_eq!(getopt_operands(&r), vec!["/var", "/tmp"]);
        assert_eq!(getopt_terminated(&r, "-exec"), vec![vec!["ls", "{}"]]);
    }

    #[test]
    fn getopt_terminated_unterminated_block() {
        // -exec without terminator — consumes rest of args
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into()]);
        let r = getopt(
            &args(&["-exec", "grep", "foo"]),
            &[],
            &term,
        );
        assert_eq!(getopt_terminated(&r, "-exec"), vec![vec!["grep", "foo"]]);
    }

    #[test]
    fn getopt_terminated_plus_terminator() {
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into(), "+".into()]);
        let r = getopt(
            &args(&["-exec", "rm", "{}", "+"]),
            &[],
            &term,
        );
        assert_eq!(getopt_terminated(&r, "-exec"), vec![vec!["rm", "{}"]]);
    }

    #[test]
    fn getopt_mix_valued_and_terminated() {
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into()]);
        let r = getopt(
            &args(&["-name", "*.rs", "-exec", "wc", "-l", "{}", ";"]),
            &["-name".into()],
            &term,
        );
        assert_eq!(getopt_flag(&r, "-name"), s("*.rs"));
        assert_eq!(getopt_terminated(&r, "-exec"), vec![vec!["wc", "-l", "{}"]]);
    }

    #[test]
    fn getopt_absent_terminated_flag() {
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into()]);
        let r = getopt(&args(&[".", "-name", "*.rs"]), &[], &term);
        assert!(getopt_terminated(&r, "-exec").is_empty());
    }

    // --- Real command patterns ---

    #[test]
    fn getopt_sudo_pattern() {
        let r = getopt(
            &args(&["-C", "3", "-u", "root", "rm", "-rf", "/"]),
            &["-C".into(), "-g".into(), "-r".into(), "-t".into(), "-U".into(), "-D".into(), "-u".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-u"), s("root"));
        assert_eq!(getopt_flag(&r, "-C"), s("3"));
        assert_eq!(getopt_operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn getopt_ssh_pattern() {
        let r = getopt(
            &args(&["-p", "22", "-i", "key.pem", "user@host", "ls", "-la"]),
            &["-b".into(), "-c".into(), "-D".into(), "-E".into(), "-e".into(), "-F".into(),
              "-I".into(), "-i".into(), "-J".into(), "-L".into(), "-l".into(), "-m".into(),
              "-O".into(), "-o".into(), "-p".into(), "-Q".into(), "-R".into(), "-S".into(),
              "-W".into(), "-w".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-p"), s("22"));
        assert_eq!(getopt_flag(&r, "-i"), s("key.pem"));
        // First operand is host, rest is inner command
        assert_eq!(getopt_operands(&r), vec!["user@host", "ls", "-la"]);
    }

    #[test]
    fn getopt_timeout_pattern() {
        let r = getopt(
            &args(&["-s", "KILL", "30", "curl", "example.com"]),
            &["-s".into(), "--signal".into(), "-k".into(), "--kill-after".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-s"), s("KILL"));
        // 30 is first operand (duration), curl + example.com are operands too
        assert_eq!(getopt_operands(&r), vec!["30", "curl", "example.com"]);
    }

    #[test]
    fn getopt_find_pattern() {
        let mut term = HashMap::new();
        term.insert("-exec".into(), vec![";".into(), "+".into()]);
        term.insert("-execdir".into(), vec![";".into(), "+".into()]);
        term.insert("-ok".into(), vec![";".into(), "+".into()]);
        term.insert("-okdir".into(), vec![";".into(), "+".into()]);
        let r = getopt(
            &args(&[".", "-name", "*.rs", "-exec", "grep", "TODO", "{}", ";"]),
            &[],
            &term,
        );
        // . and -name and *.rs are operands (no valued flags defined)
        assert_eq!(getopt_operands(&r), vec![".", "-name", "*.rs"]);
        assert_eq!(getopt_terminated(&r, "-exec"), vec![vec!["grep", "TODO", "{}"]]);
    }

    #[test]
    fn getopt_kubectl_exec_pattern() {
        // kubectl exec -c container mypod -- ls -la
        // Flags before first operand; -- after operand is just another operand (POSIX)
        let r = getopt(
            &args(&["-c", "mycontainer", "mypod", "--", "ls", "-la"]),
            &["-n".into(), "-c".into(), "--namespace".into(), "--container".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-c"), s("mycontainer"));
        assert_eq!(getopt_operands(&r), vec!["mypod", "--", "ls", "-la"]);
    }

    #[test]
    fn getopt_kubectl_flags_first_then_double_dash() {
        // When -- comes before any operand, it IS the separator
        let r = getopt(
            &args(&["-c", "mycontainer", "--", "ls", "-la"]),
            &["-c".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-c"), s("mycontainer"));
        assert_eq!(getopt_operands(&r), vec!["ls", "-la"]);
    }

    #[test]
    fn getopt_double_dash_after_operand_is_operand() {
        // Per POSIX: after first non-option, getopt never runs again.
        // -- after an operand is just another operand string.
        let r = getopt(
            &args(&["cmd", "--", "-rf"]),
            &[],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["cmd", "--", "-rf"]);
    }

    #[test]
    fn getopt_xargs_pattern() {
        let r = getopt(
            &args(&["-n", "1", "-I", "{}", "rm", "-v"]),
            &["-d".into(), "-I".into(), "-L".into(), "-n".into(), "-P".into(), "-s".into(), "-E".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-n"), s("1"));
        assert_eq!(getopt_flag(&r, "-I"), s("{}"));
        assert_eq!(getopt_operands(&r), vec!["rm", "-v"]);
    }

    #[test]
    fn getopt_env_pattern() {
        // env -u FOO -S "bash -c test" VAR=val cmd arg
        let r = getopt(
            &args(&["-u", "FOO", "cmd", "arg"]),
            &["-u".into(), "-S".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-u"), s("FOO"));
        assert_eq!(getopt_operands(&r), vec!["cmd", "arg"]);
    }

    // --- POSIX conformance: bare dash ---

    #[test]
    fn getopt_bare_dash_is_operand() {
        // POSIX: "-" is not an option, it's an operand (stdin convention)
        let r = getopt(
            &args(&["-v", "-", "file"]),
            &[],
            &no_terminated(),
        );
        // -v is a standalone flag, "-" is first operand (triggers POSIX stop),
        // "file" is also an operand
        assert_eq!(getopt_operands(&r), vec!["-", "file"]);
    }

    #[test]
    fn getopt_bare_dash_alone() {
        let r = getopt(&args(&["-"]), &[], &no_terminated());
        assert_eq!(getopt_operands(&r), vec!["-"]);
    }

    #[test]
    fn getopt_bare_dash_with_valued_flags() {
        // "-" should NOT be consumed as a flag value
        let r = getopt(
            &args(&["-o", "-", "file"]),
            &["-o".into()],
            &no_terminated(),
        );
        // -o takes "-" as its value (option-argument can be anything per Guideline 7)
        assert_eq!(getopt_flag(&r, "-o"), s("-"));
        assert_eq!(getopt_operands(&r), vec!["file"]);
    }

    // --- POSIX conformance: grouped short options ---

    #[test]
    fn getopt_grouped_boolean_flags() {
        // -abc = -a -b -c (all standalone)
        let r = getopt(
            &args(&["-abc", "file"]),
            &[],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["file"]);
    }

    #[test]
    fn getopt_grouped_with_valued_last() {
        // -au root = -a (standalone) then -u root (valued, last char)
        let r = getopt(
            &args(&["-au", "root", "rm", "-rf"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-u"), s("root"));
        assert_eq!(getopt_operands(&r), vec!["rm", "-rf"]);
    }

    #[test]
    fn getopt_grouped_with_valued_attached() {
        // -uroot = -u with attached value "root"
        let r = getopt(
            &args(&["-uroot", "rm"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-u"), s("root"));
        assert_eq!(getopt_operands(&r), vec!["rm"]);
    }

    #[test]
    fn getopt_grouped_valued_mid_group() {
        // -iuroot = -i (standalone), -u with attached value "root"
        let r = getopt(
            &args(&["-iuroot", "cmd"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-u"), s("root"));
        assert_eq!(getopt_operands(&r), vec!["cmd"]);
    }

    #[test]
    fn getopt_sudo_grouped_iu() {
        // sudo -iu root rm — real-world pattern
        // -i standalone, -u valued (last char), "root" is value
        let r = getopt(
            &args(&["-iu", "root", "rm", "-rf", "/"]),
            &["-C".into(), "-g".into(), "-r".into(), "-t".into(),
              "-U".into(), "-D".into(), "-u".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-u"), s("root"));
        assert_eq!(getopt_operands(&r), vec!["rm", "-rf", "/"]);
    }

    // --- POSIX conformance: attached option-arguments ---

    #[test]
    fn getopt_attached_value_short() {
        // -p22 = -p with value "22"
        let r = getopt(
            &args(&["-p22", "host", "ls"]),
            &["-p".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-p"), s("22"));
        assert_eq!(getopt_operands(&r), vec!["host", "ls"]);
    }

    #[test]
    fn getopt_ssh_attached_port() {
        // ssh -p22 -i key host cmd — real-world pattern
        let r = getopt(
            &args(&["-p22", "-i", "key.pem", "user@host", "ls"]),
            &["-p".into(), "-i".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-p"), s("22"));
        assert_eq!(getopt_flag(&r, "-i"), s("key.pem"));
        assert_eq!(getopt_operands(&r), vec!["user@host", "ls"]);
    }

    // --- POSIX conformance: long options with = ---

    #[test]
    fn getopt_long_option_with_equals() {
        // --signal=KILL — value attached with =
        let r = getopt(
            &args(&["--signal=KILL", "30", "cmd"]),
            &["--signal".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "--signal"), s("KILL"));
        assert_eq!(getopt_operands(&r), vec!["30", "cmd"]);
    }

    #[test]
    fn getopt_long_option_separate() {
        // --signal KILL — value in next arg
        let r = getopt(
            &args(&["--signal", "KILL", "30", "cmd"]),
            &["--signal".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "--signal"), s("KILL"));
        assert_eq!(getopt_operands(&r), vec!["30", "cmd"]);
    }

    #[test]
    fn getopt_long_option_equals_unknown_value() {
        // Unknown long option with = is not in valued, skip it
        let r = getopt(
            &args(&["--unknown=val", "cmd"]),
            &[],
            &no_terminated(),
        );
        assert_eq!(getopt_operands(&r), vec!["cmd"]);
    }

    #[test]
    fn getopt_long_option_equals_with_expansion() {
        let r = getopt(
            &args(&["--signal=$(trap)", "cmd"]),
            &["--signal".into()],
            &no_terminated(),
        );
        assert!(matches!(getopt_flag(&r, "--signal"), TriVal::Unknown));
        assert_eq!(getopt_operands(&r), vec!["cmd"]);
    }

    // --- POSIX spec example equivalences ---
    // The spec says these should all be equivalent:
    // cmd -ao arg path path
    // cmd -a -o arg path path
    // cmd -o arg -a path path
    // cmd -a -o arg -- path path
    // cmd -a -oarg path path
    // cmd -aoarg path path

    #[test]
    fn getopt_spec_example_separate() {
        let r = getopt(
            &args(&["-a", "-o", "arg", "path1", "path2"]),
            &["-o".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-o"), s("arg"));
        assert_eq!(getopt_operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn getopt_spec_example_grouped_separate_value() {
        // -ao arg = -a standalone, -o takes "arg" (last char, separate)
        let r = getopt(
            &args(&["-ao", "arg", "path1", "path2"]),
            &["-o".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-o"), s("arg"));
        assert_eq!(getopt_operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn getopt_spec_example_reordered() {
        // -o arg -a path path — option order doesn't matter,
        // -a is still a flag (before any operand)
        let r = getopt(
            &args(&["-o", "arg", "-a", "path1", "path2"]),
            &["-o".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-o"), s("arg"));
        assert_eq!(getopt_operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn getopt_spec_example_double_dash() {
        // -a -o arg -- path path
        let r = getopt(
            &args(&["-a", "-o", "arg", "--", "path1", "path2"]),
            &["-o".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-o"), s("arg"));
        assert_eq!(getopt_operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn getopt_spec_example_attached() {
        // -a -oarg path path
        let r = getopt(
            &args(&["-a", "-oarg", "path1", "path2"]),
            &["-o".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-o"), s("arg"));
        assert_eq!(getopt_operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn getopt_spec_example_grouped_attached() {
        // -aoarg path path = -a standalone, -o with attached "arg"
        let r = getopt(
            &args(&["-aoarg", "path1", "path2"]),
            &["-o".into()],
            &no_terminated(),
        );
        assert_eq!(getopt_flag(&r, "-o"), s("arg"));
        assert_eq!(getopt_operands(&r), vec!["path1", "path2"]);
    }

    // --- Exhaustiveness / three-valued logic ---

    #[test]
    fn getopt_known_flags_only_is_exhaustive() {
        let r = getopt(
            &args(&["-u", "root", "cmd"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert!(is_exhaustive(&r));
    }

    #[test]
    fn getopt_unknown_short_flag_degrades_exhaustive() {
        // -x is not in valued list — we don't know if it takes a value
        let r = getopt(
            &args(&["-x", "cmd"]),
            &[],
            &no_terminated(),
        );
        assert!(!is_exhaustive(&r));
    }

    #[test]
    fn getopt_unknown_long_flag_degrades_exhaustive() {
        let r = getopt(
            &args(&["--unknown", "cmd"]),
            &[],
            &no_terminated(),
        );
        assert!(!is_exhaustive(&r));
    }

    #[test]
    fn getopt_unknown_long_flag_with_equals_stays_exhaustive() {
        // --unknown=val is self-contained, doesn't affect operand boundaries
        let r = getopt(
            &args(&["--unknown=val", "cmd"]),
            &[],
            &no_terminated(),
        );
        assert!(is_exhaustive(&r));
    }

    #[test]
    fn getopt_unknown_trival_degrades_exhaustive() {
        let mut a = args(&["-u", "root"]);
        a.push(TriVal::Unknown);
        a.push(s("cmd"));
        let r = getopt(&a, &["-u".into()], &no_terminated());
        assert!(!is_exhaustive(&r));
    }

    #[test]
    fn getopt_non_exhaustive_input_propagates() {
        let r = run_getopt(
            &args(&["-u", "root", "cmd"]),
            &["-u".into()],
            &no_terminated(),
            false, // input is non-exhaustive
        );
        assert!(!is_exhaustive(&r));
        // Absent flag returns Unknown, not Null
        assert!(matches!(getopt_flag(&r, "-z"), TriVal::Unknown));
    }

    #[test]
    fn getopt_exhaustive_absent_flag_is_null() {
        let r = getopt(
            &args(&["-u", "root", "cmd"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert!(matches!(getopt_flag(&r, "-z"), TriVal::Null));
    }

    #[test]
    fn getopt_non_exhaustive_absent_flag_is_unknown() {
        // -x is unknown, so result is non-exhaustive
        let r = getopt(
            &args(&["-x", "-u", "root", "cmd"]),
            &["-u".into()],
            &no_terminated(),
        );
        assert!(!is_exhaustive(&r));
        // -z not seen, but args are non-exhaustive, so absence is uncertain
        assert!(matches!(getopt_flag(&r, "-z"), TriVal::Unknown));
    }

    #[test]
    fn getopt_all_known_flags_is_exhaustive() {
        // -v is standalone (not valued), but it's still "known" in the sense
        // that we skip it. However, we DON'T actually know it's standalone —
        // we assume all non-valued flags are standalone.
        // So any unknown flag degrades exhaustiveness.
        let r = getopt(
            &args(&["-v", "cmd"]),
            &[], // -v not in valued list = unknown
            &no_terminated(),
        );
        // -v is unknown — we can't confirm it doesn't take a value
        assert!(!is_exhaustive(&r));
    }
}
