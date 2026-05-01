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
        "getopt" => eval_getopt(call, ctx),
        "or" => eval_or_func(&call.args, ctx),
        "rsplit" => eval_rsplit(&call.args, ctx),
        "slice" => eval_slice(&call.args, ctx),
        "take_until" => eval_take_until(&call.args, ctx),
        "split_at" => eval_split_at(&call.args, ctx),
        "_[_]" => eval_index(&call.args, ctx),
        "_+_" => eval_add(&call.args, ctx),
        "_?_:_" => eval_ternary(&call.args, ctx),
        _ => {
            // Member calls: obj.method(args)
            if let Some(target) = &call.target {
                let obj = eval_expr(&target.expr, ctx);
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

// --- CEL operators and functions ---

/// `cond ? then : else` — ternary conditional
fn eval_ternary(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 3 { return TriVal::Unknown; }
    let cond = eval_expr(&args[0].expr, ctx);
    match cond.is_truthy() {
        TriBool::True => eval_expr(&args[1].expr, ctx),
        TriBool::False => eval_expr(&args[2].expr, ctx),
        TriBool::Unknown => TriVal::Unknown,
    }
}

/// `a + b` — integer addition or list concatenation
fn eval_add(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let a = eval_expr(&args[0].expr, ctx);
    let b = eval_expr(&args[1].expr, ctx);
    match (&a, &b) {
        (TriVal::Int(a), TriVal::Int(b)) => TriVal::Int(a + b),
        (TriVal::List { elements: a, .. }, TriVal::List { elements: b, .. }) => {
            let mut result = a.clone();
            result.extend(b.iter().cloned());
            TriVal::List { elements: result, exhaustive: true }
        }
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        _ => TriVal::Unknown,
    }
}

/// `split_at(list, markers)` — split list into groups, starting new group at each marker.
fn eval_split_at(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let list = eval_expr(&args[0].expr, ctx);
    let markers = eval_expr(&args[1].expr, ctx);
    match (&list, &markers) {
        (TriVal::List { elements, .. }, TriVal::List { elements: marker_list, .. }) => {
            let marker_strings: Vec<&str> = marker_list.iter().filter_map(|m| {
                if let TriVal::String(s) = m { Some(s.as_str()) } else { None }
            }).collect();

            if elements.is_empty() {
                return TriVal::List { elements: Vec::new(), exhaustive: true };
            }

            let mut groups: Vec<TriVal> = Vec::new();
            let mut current: Vec<TriVal> = Vec::new();

            for el in elements {
                let is_marker = if let TriVal::String(s) = el {
                    marker_strings.contains(&s.as_str())
                } else {
                    false
                };

                if is_marker {
                    groups.push(TriVal::List { elements: current, exhaustive: true });
                    current = vec![el.clone()];
                } else {
                    current.push(el.clone());
                }
            }
            groups.push(TriVal::List { elements: current, exhaustive: true });

            TriVal::List { elements: groups, exhaustive: true }
        }
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        _ => TriVal::Unknown,
    }
}

/// `take_until(list, tokens)` — elements from front until a token (exclusive). All if no match.
fn eval_take_until(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let list = eval_expr(&args[0].expr, ctx);
    let tokens = eval_expr(&args[1].expr, ctx);
    match (&list, &tokens) {
        (TriVal::List { elements, exhaustive }, TriVal::List { elements: tok_list, .. }) => {
            let tok_strings: Vec<&str> = tok_list.iter().filter_map(|t| {
                if let TriVal::String(s) = t { Some(s.as_str()) } else { None }
            }).collect();
            let mut result = Vec::new();
            for el in elements {
                if let TriVal::String(s) = el {
                    if tok_strings.contains(&s.as_str()) {
                        break;
                    }
                }
                result.push(el.clone());
            }
            TriVal::List { elements: result, exhaustive: *exhaustive }
        }
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        _ => TriVal::Unknown,
    }
}

/// `slice(list, n)` — list from index n onward
fn eval_slice(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let list = eval_expr(&args[0].expr, ctx);
    let n = eval_expr(&args[1].expr, ctx);
    match (&list, &n) {
        (TriVal::List { elements, exhaustive }, TriVal::Int(i)) => {
            let start = if *i < 0 { 0 } else { *i as usize };
            let slice = if start < elements.len() {
                elements[start..].to_vec()
            } else {
                Vec::new()
            };
            TriVal::List { elements: slice, exhaustive: *exhaustive }
        }
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        _ => TriVal::Unknown,
    }
}

/// `list[index]`
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

/// `or(val, fallback)` — null coalescing
fn eval_or_func(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let val = eval_expr(&args[0].expr, ctx);
    match &val {
        TriVal::Null => eval_expr(&args[1].expr, ctx),
        TriVal::Unknown => TriVal::Unknown,
        _ => val,
    }
}

/// `rsplit(str, sep)` or `rsplit(str, sep, n)` — split, null-pad left to n
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
                    let mut elements: Vec<TriVal> = Vec::with_capacity(n);
                    let pad = n.saturating_sub(parts.len());
                    for _ in 0..pad { elements.push(TriVal::Null); }
                    let start = parts.len().saturating_sub(n);
                    for p in &parts[start..] { elements.push(TriVal::String(p.to_string())); }
                    TriVal::List { elements, exhaustive: true }
                }
            }
        }
        _ => TriVal::Unknown,
    }
}

/// `getopt(args, optstring)` or `getopt(args, [valued_list])` — CEL function escape hatch.
/// Accepts optstring ("u:C:") or legacy valued list (["-u", "-C"]) as second arg.
fn eval_getopt(call: &cel::CallExpr, ctx: &HashMap<String, TriVal>) -> TriVal {
    if call.args.len() != 2 { return TriVal::Unknown; }
    let args_val = eval_expr(&call.args[0].expr, ctx);
    let spec_val = eval_expr(&call.args[1].expr, ctx);

    let (args, input_exhaustive) = match &args_val {
        TriVal::List { elements, exhaustive } => (elements.as_slice(), *exhaustive),
        TriVal::Unknown => return TriVal::Unknown,
        _ => return TriVal::Null,
    };

    let spec = match &spec_val {
        // New: optstring format "u:C:"
        TriVal::String(optstring) => {
            super::args::ArgSpec::from_optstring(super::args::ArgStyle::Posix, optstring)
        }
        // Legacy: valued list ["-u", "-C"] — treat all as valued
        TriVal::List { elements, .. } => {
            let options: Vec<super::args::OptDef> = elements.iter().filter_map(|e| {
                if let TriVal::String(s) = e {
                    let name = s.trim_start_matches('-').to_string();
                    Some(super::args::OptDef { name, has_arg: true })
                } else {
                    None
                }
            }).collect();
            super::args::ArgSpec { style: super::args::ArgStyle::Posix, options }
        }
        _ => return TriVal::Unknown,
    };

    let parsed = super::args::parse_args(args, &spec, input_exhaustive);
    super::parse::parsed_args_to_trival(&parsed)
}

/// Member-call methods on getopt result: .value(flag), .positional(n), .operands_from(n)
fn eval_getopt_method(
    getopt_map: &HashMap<String, TriVal>,
    method: &str,
    args: &[cel::IdedExpr],
    ctx: &HashMap<String, TriVal>,
) -> Option<TriVal> {
    let exhaustive = matches!(getopt_map.get("exhaustive"), Some(TriVal::Bool(true)));
    let absent = if exhaustive { TriVal::Null } else { TriVal::Unknown };

    match method {
        "value" => {
            if args.len() != 1 { return Some(TriVal::Unknown); }
            let flag = eval_expr(&args[0].expr, ctx);
            if let TriVal::String(flag_name) = &flag {
                let name = flag_name.trim_start_matches('-');
                if let Some(TriVal::Map(flags)) = getopt_map.get("flags") {
                    Some(flags.get(name).cloned().unwrap_or(absent))
                } else {
                    Some(absent)
                }
            } else {
                Some(TriVal::Unknown)
            }
        }
        "positional" => {
            if args.len() != 1 { return Some(TriVal::Unknown); }
            let n = eval_expr(&args[0].expr, ctx);
            if let TriVal::Int(idx) = &n {
                if let Some(TriVal::List { elements, .. }) = getopt_map.get("operands") {
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
                if let Some(TriVal::List { elements, .. }) = getopt_map.get("operands") {
                    let i = if *idx < 0 { 0 } else { *idx as usize };
                    let rest = if i < elements.len() { elements[i..].to_vec() } else { Vec::new() };
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

    // --- CEL expression evaluator helper ---

    /// Evaluate a CEL expression with an optional context.
    fn eval_cel(expr: &str) -> TriVal {
        eval_cel_with(expr, &HashMap::new())
    }

    fn eval_cel_with(expr: &str, ctx: &HashMap<String, TriVal>) -> TriVal {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(expr).unwrap_or_else(|e| panic!("CEL parse error: {:?}", e));
        eval_expr(&ast.expr, ctx)
    }

    fn eval_cel_args(expr: &str, args: &[&str]) -> TriVal {
        let mut cmd_map = HashMap::new();
        let args_trival: Vec<TriVal> = args.iter()
            .map(|a| TriVal::String(a.to_string()))
            .collect();
        cmd_map.insert("args".into(), TriVal::List { elements: args_trival, exhaustive: true });
        let mut ctx = HashMap::new();
        ctx.insert("command".into(), TriVal::Map(cmd_map));
        eval_cel_with(expr, &ctx)
    }

    // --- CEL operator: _+_ (int add + list concat) ---

    #[test]
    fn cel_add_integers() {
        assert_eq!(eval_cel("1 + 2"), TriVal::Int(3));
        assert_eq!(eval_cel("0 + 0"), TriVal::Int(0));
    }

    #[test]
    fn cel_add_lists() {
        let r = eval_cel(r#"["a"] + ["b", "c"]"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 3);
            assert_eq!(elements[0], TriVal::String("a".into()));
            assert_eq!(elements[2], TriVal::String("c".into()));
        } else {
            panic!("expected list, got {:?}", r);
        }
    }

    #[test]
    fn cel_add_empty_lists() {
        let r = eval_cel(r#"[] + ["a"]"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 1);
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn cel_add_type_mismatch() {
        assert!(matches!(eval_cel(r#""a" + 1"#), TriVal::Unknown));
    }

    #[test]
    fn cel_map_comprehension() {
        // .map(a, a) is identity — this tests that _+_ list concat works internally
        let r = eval_cel_args(r#"command.args.map(a, a)"#, &["ls", "-la"]);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 2);
            assert_eq!(elements[0], TriVal::String("ls".into()));
            assert_eq!(elements[1], TriVal::String("-la".into()));
        } else {
            panic!("expected list, got {:?}", r);
        }
    }

    // --- CEL operator: _?_:_ (ternary conditional) ---

    #[test]
    fn cel_ternary_true() {
        assert_eq!(eval_cel(r#"true ? "yes" : "no""#), TriVal::String("yes".into()));
    }

    #[test]
    fn cel_ternary_false() {
        assert_eq!(eval_cel(r#"false ? "yes" : "no""#), TriVal::String("no".into()));
    }

    #[test]
    fn cel_ternary_with_comparison() {
        assert_eq!(eval_cel(r#"1 == 1 ? "eq" : "ne""#), TriVal::String("eq".into()));
    }

    #[test]
    fn cel_ternary_nested() {
        assert_eq!(
            eval_cel(r#"true ? (false ? "a" : "b") : "c""#),
            TriVal::String("b".into()),
        );
    }

    #[test]
    fn cel_ternary_unknown_cond() {
        let r = eval_cel_with("x ? 1 : 2", &HashMap::new());
        // x is unbound → Unknown, ternary should propagate
        assert!(matches!(r, TriVal::Unknown));
    }

    #[test]
    fn cel_map_with_ternary() {
        let r = eval_cel_args(
            r#"command.args.map(a, a == "-v" ? "verbose" : a)"#,
            &["-v", "cmd"],
        );
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements[0], TriVal::String("verbose".into()));
            assert_eq!(elements[1], TriVal::String("cmd".into()));
        } else {
            panic!("expected list, got {:?}", r);
        }
    }

    // --- CEL function: slice(list, n) ---

    #[test]
    fn cel_slice_from_start() {
        let r = eval_cel(r#"slice(["a","b","c","d"], 0)"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 4);
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_slice_from_middle() {
        let r = eval_cel(r#"slice(["a","b","c","d"], 2)"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 2);
            assert_eq!(elements[0], TriVal::String("c".into()));
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_slice_past_end() {
        let r = eval_cel(r#"slice(["a","b"], 10)"#);
        if let TriVal::List { elements, .. } = r {
            assert!(elements.is_empty());
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_slice_empty() {
        let r = eval_cel(r#"slice([], 0)"#);
        if let TriVal::List { elements, .. } = r {
            assert!(elements.is_empty());
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_slice_negative_clamps() {
        let r = eval_cel(r#"slice(["a","b"], -1)"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 2);
        } else { panic!("expected list"); }
    }

    // --- CEL function: take_until(list, tokens) ---

    #[test]
    fn cel_take_until_basic() {
        let r = eval_cel(r#"take_until(["a","b",";","c"], [";"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements, vec![TriVal::String("a".into()), TriVal::String("b".into())]);
        } else { panic!("expected list, got {:?}", r); }
    }

    #[test]
    fn cel_take_until_no_match() {
        let r = eval_cel(r#"take_until(["a","b","c"], [";"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 3);
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_take_until_at_start() {
        let r = eval_cel(r#"take_until([";","a"], [";"])"#);
        if let TriVal::List { elements, .. } = r {
            assert!(elements.is_empty());
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_take_until_empty() {
        let r = eval_cel(r#"take_until([], [";"])"#);
        if let TriVal::List { elements, .. } = r {
            assert!(elements.is_empty());
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_take_until_multiple_tokens() {
        let r = eval_cel(r#"take_until(["a","b","+","c",";"], [";","+"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements, vec![TriVal::String("a".into()), TriVal::String("b".into())]);
        } else { panic!("expected list"); }
    }

    // --- CEL function: split_at(list, markers) ---

    #[test]
    fn cel_split_at_basic() {
        let r = eval_cel(r#"split_at(["a","b","|","c","d"], ["|"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 2);
            // First group: ["a","b"]
            if let TriVal::List { elements: g0, .. } = &elements[0] {
                assert_eq!(g0.len(), 2);
                assert_eq!(g0[0], TriVal::String("a".into()));
            } else { panic!("expected list for group 0"); }
            // Second group: ["|","c","d"]
            if let TriVal::List { elements: g1, .. } = &elements[1] {
                assert_eq!(g1.len(), 3);
                assert_eq!(g1[0], TriVal::String("|".into()));
            } else { panic!("expected list for group 1"); }
        } else { panic!("expected list, got {:?}", r); }
    }

    #[test]
    fn cel_split_at_no_marker() {
        let r = eval_cel(r#"split_at(["a","b","c"], ["|"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 1); // one group = whole list
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_split_at_marker_at_start() {
        let r = eval_cel(r#"split_at(["|","a","|","b"], ["|"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 3); // [], ["|","a"], ["|","b"]
            if let TriVal::List { elements: g0, .. } = &elements[0] {
                assert!(g0.is_empty());
            } else { panic!("expected empty list for group 0"); }
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_split_at_multiple_markers() {
        let r = eval_cel(r#"split_at(["a","-exec","b",";","-ok","c","+"], ["-exec","-ok"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 3);
            // ["a"], ["-exec","b",";"], ["-ok","c","+"]
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_split_at_empty() {
        let r = eval_cel(r#"split_at([], ["|"])"#);
        if let TriVal::List { elements, .. } = r {
            assert!(elements.is_empty());
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_split_at_marker_at_end() {
        let r = eval_cel(r#"split_at(["a","|"], ["|"])"#);
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 2); // ["a"], ["|"]
        } else { panic!("expected list"); }
    }

    #[test]
    fn cel_filter_comprehension() {
        let r = eval_cel_args(
            r#"command.args.filter(a, a != "-la")"#,
            &["ls", "-la", "/"],
        );
        if let TriVal::List { elements, .. } = r {
            assert_eq!(elements.len(), 2);
            assert_eq!(elements[0], TriVal::String("ls".into()));
            assert_eq!(elements[1], TriVal::String("/".into()));
        } else {
            panic!("expected list, got {:?}", r);
        }
    }
}
