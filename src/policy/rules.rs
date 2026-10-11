//! Three-valued CEL evaluator for policy rules.
//!
//! Uses cel-parser for parsing CEL syntax into an AST, then evaluates with
//! three-valued logic (True/False/Unknown). This handles unknowable values
//! correctly: `Unknown == "root"` → Unknown, `Unknown != "root"` → Unknown.
//!
//! Two special values:
//! - Null: definite absence. `Null == X` → False, `Null != X` → True.
//! - Unknown: unknowable value (e.g. a `$(…)` word). All comparisons → Unknown.

use std::collections::HashMap;

use cel_parser::ast as cel;
use cel_parser::reference::Val;

use super::config::RuleConfig;
use super::parse::CommandInfo;
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
    fn is_truthy(&self) -> TriBool {
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
enum TriBool {
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
    /// An *Override rule*: it decides alone when it matches.
    pub is_override: bool,
    pub message: Option<String>,
}

pub struct RuleSet {
    pub rules: Vec<CompiledRule>,
    pub default: Decision,
}

// --- Public API ---

/// Compile rules into a RuleSet. Parses CEL expressions at load time.
pub fn compile(configs: &[RuleConfig], default: Decision) -> Result<RuleSet, String> {
    let mut rules = Vec::with_capacity(configs.len());
    for config in configs {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(&config.when)
            .map_err(|e| format!("CEL parse error in rule '{}': {:?}", config.description, e))?;
        let action = match config.action.as_str() {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            "ask" => Decision::Ask,
            other => return Err(format!(
                "Invalid action '{}' in rule '{}' (must be allow/ask/deny)",
                other, config.description
            )),
        };
        rules.push(CompiledRule {
            description: config.description.clone(),
            ast,
            action,
            is_override: config.is_override,
            message: config.message.clone(),
        });
    }
    Ok(RuleSet { rules, default })
}

/// Evaluate a single CommandInfo against the rule set.
///
/// Every matching rule counts, and the strictest action wins: deny, then ask,
/// then allow. *Override rule*s are a group of their own: when any matches,
/// the strictest override decides and the other rules do not count. No match
/// gives the default.
pub fn evaluate(cmd: &CommandInfo, ctx: &PaneContext, rules: &RuleSet) -> PolicyResult {
    decide(rules, &build_context(cmd, ctx))
}

/// The decision of `rules` over one evaluation context.
fn decide(rules: &RuleSet, eval_ctx: &HashMap<String, TriVal>) -> PolicyResult {
    let strictest = |overrides: bool| {
        strictest_match(rules.rules.iter().filter(|r| r.is_override == overrides), eval_ctx)
    };

    strictest(true).or_else(|| strictest(false)).unwrap_or_else(|| PolicyResult {
        decision: rules.default.clone(),
        rule: "default".to_string(),
    })
}

/// How a rule matched: on a True `when`, or on an Unknown one it could not
/// rule out.
enum Match {
    True,
    Unknown,
}

/// The result of the strictest matching rule; on a tie, the first one.
fn strictest_match<'a>(
    rules: impl Iterator<Item = &'a CompiledRule>,
    eval_ctx: &HashMap<String, TriVal>,
) -> Option<PolicyResult> {
    let mut strictest: Option<PolicyResult> = None;
    for rule in rules {
        let Some(matched) = rule_match(rule, eval_ctx) else { continue };
        if strictest.as_ref().is_none_or(|s| super::severity(&rule.action) > super::severity(&s.decision)) {
            strictest = Some(rule_result(rule, matched));
        }
    }
    strictest
}

/// Unknown fails closed: it matches a deny or ask rule, never an allow rule,
/// and never an override, whose match switches the other rules off.
fn rule_match(rule: &CompiledRule, eval_ctx: &HashMap<String, TriVal>) -> Option<Match> {
    match eval_expr(&rule.ast.expr, eval_ctx).is_truthy() {
        TriBool::True => Some(Match::True),
        TriBool::Unknown if rule.action != Decision::Allow && !rule.is_override => Some(Match::Unknown),
        _ => None,
    }
}

fn rule_result(rule: &CompiledRule, matched: Match) -> PolicyResult {
    let mut rule_desc = match &rule.message {
        Some(msg) => format!("{}: {}", rule.description, msg),
        None => rule.description.clone(),
    };
    if let Match::Unknown = matched {
        // Tells the model what fired the rule, so it can write the command
        // with literal words.
        rule_desc.push_str(" (could not be ruled out)");
    }
    PolicyResult {
        decision: rule.action.clone(),
        rule: rule_desc,
    }
}

// --- Three-valued evaluator ---

fn eval_expr(expr: &cel::Expr, ctx: &HashMap<String, TriVal>) -> TriVal {
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
        "or" => eval_or_func(&call.args, ctx),
        "_[_]" => eval_index(&call.args, ctx),
        "_+_" => eval_add(&call.args, ctx),
        "_?_:_" => eval_ternary(&call.args, ctx),
        "size" => {
            // size(x) or x.size() → Int(len) for exhaustive lists/strings, else Unknown.
            // Resolve the operand from the receiver (method form) or the first
            // positional arg (function form), mirroring glob/contains/startsWith.
            let operand = call
                .target
                .as_ref()
                .map(|t| &t.expr)
                .or_else(|| call.args.first().map(|a| &a.expr));
            match operand.map(|e| eval_expr(e, ctx)) {
                Some(TriVal::List { elements, exhaustive }) => {
                    if exhaustive {
                        TriVal::Int(elements.len() as i64)
                    } else {
                        TriVal::Unknown
                    }
                }
                Some(TriVal::String(s)) => TriVal::Int(s.len() as i64),
                _ => TriVal::Unknown,
            }
        }
        _ => TriVal::Unknown,
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

fn eval_cmp(args: &[cel::IdedExpr], ctx: &HashMap<String, TriVal>, op: fn(i64, i64) -> bool) -> TriVal {
    if args.len() != 2 { return TriVal::Unknown; }
    let a = eval_expr(&args[0].expr, ctx);
    let b = eval_expr(&args[1].expr, ctx);
    match (&a, &b) {
        (TriVal::Int(a), TriVal::Int(b)) => TriVal::Bool(op(*a, *b)),
        (TriVal::Unknown, _) | (_, TriVal::Unknown) => TriVal::Unknown,
        _ => TriVal::Unknown,
    }
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

    // If the list is non-exhaustive, only a definitive conclusion stands.
    // For exists (accu_init=false): True is definitive (found match), else Unknown.
    // For all (accu_init=true): False is definitive (found failure), else Unknown.
    if !exhaustive {
        let init_truthy = eval_expr(&comp.accu_init.expr, ctx).is_truthy();
        let definitive = match init_truthy {
            TriBool::False => TriBool::True,   // exists: True is definitive
            _ => TriBool::False,               // all: False is definitive
        };
        if accu.is_truthy() != definitive {
            accu = TriVal::Unknown;
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
        (TriVal::List { elements: a, exhaustive: a_ex }, TriVal::List { elements: b, exhaustive: b_ex }) => {
            let mut result = a.clone();
            result.extend(b.iter().cloned());
            TriVal::List { elements: result, exhaustive: *a_ex && *b_ex }
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

// --- Context construction ---

fn build_context(cmd: &CommandInfo, pane: &PaneContext) -> HashMap<String, TriVal> {
    let command = HashMap::from([("name".to_string(), TriVal::String(cmd.name.clone()))]);
    let foreground = match &pane.foreground {
        Some(s) => TriVal::String(s.clone()),
        None => TriVal::Null,
    };
    let pane = HashMap::from([("foreground".to_string(), foreground)]);
    HashMap::from([
        ("command".to_string(), TriVal::Map(command)),
        ("pane".to_string(), TriVal::Map(pane)),
    ])
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn local_pane() -> PaneContext {
        PaneContext {
            hostname: None,
            cwd: Some("/home/user".into()),
            foreground: Some("bash".into()),
            user: Some("jess".into()),
        }
    }

    fn compile_rules(specs: &[(&str, &str, &str)]) -> RuleSet {
        let overrides: Vec<_> = specs.iter().map(|&(desc, when, action)| (desc, when, action, false)).collect();
        compile_override_rules(&overrides)
    }

    /// `compile_rules` with each rule's `override` flag.
    fn compile_override_rules(specs: &[(&str, &str, &str, bool)]) -> RuleSet {
        let configs: Vec<RuleConfig> = specs.iter().map(|&(desc, when, action, is_override)| RuleConfig {
            description: desc.to_string(),
            when: when.to_string(),
            action: action.to_string(),
            is_override,
            message: None,
        }).collect();
        compile(&configs, Decision::Ask).unwrap()
    }

    fn decision_of(name: &str, rules: &RuleSet) -> Decision {
        evaluate(&CommandInfo::simple(name), &local_pane(), rules).decision
    }

    fn ctx_with(name: &str, val: TriVal) -> HashMap<String, TriVal> {
        HashMap::from([(name.to_string(), val)])
    }

    fn strings(vals: &[&str], exhaustive: bool) -> TriVal {
        TriVal::List { elements: vals.iter().map(|v| TriVal::String(v.to_string())).collect(), exhaustive }
    }

    fn eval_cel(expr: &str) -> TriVal {
        eval_cel_with(expr, &HashMap::new())
    }

    fn eval_cel_with(expr: &str, ctx: &HashMap<String, TriVal>) -> TriVal {
        let parser = cel_parser::Parser::new();
        let ast = parser.parse(expr).unwrap_or_else(|e| panic!("CEL parse error: {:?}", e));
        eval_expr(&ast.expr, ctx)
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

    // --- Matching and combining ---

    #[test]
    fn name_equals_matches() {
        let rules = compile_rules(&[("ls rule", r#"command.name == "ls""#, "allow")]);
        assert_eq!(decision_of("ls", &rules), Decision::Allow);
        assert_eq!(decision_of("cat", &rules), Decision::Ask);
    }

    #[test]
    fn name_in_list_matches() {
        let rules = compile_rules(&[("tools", r#"command.name in ["ls","cat","grep"]"#, "allow")]);
        assert_eq!(decision_of("cat", &rules), Decision::Allow);
    }

    #[test]
    fn the_strictest_matching_rule_wins_wherever_it_sits() {
        let rules = compile_rules(&[
            ("allow ls", r#"command.name == "ls""#, "allow"),
            ("deny ls", r#"command.name == "ls""#, "deny"),
        ]);
        assert_eq!(decision_of("ls", &rules), Decision::Deny);
    }

    #[test]
    fn no_match_returns_default() {
        let rules = compile_rules(&[("allow cat", r#"command.name == "cat""#, "allow")]);
        let r = evaluate(&CommandInfo::simple("ls"), &local_pane(), &rules);
        assert_eq!((r.decision, r.rule.as_str()), (Decision::Ask, "default"));
    }

    #[test]
    fn pane_foreground_is_visible() {
        let rules = compile_rules(&[("in bash", r#"pane.foreground == "bash""#, "deny")]);
        assert_eq!(decision_of("ls", &rules), Decision::Deny);
    }

    #[test]
    fn a_rule_that_may_match_fires_unless_it_allows_or_overrides() {
        const UNSURE: &str = "rule (could not be ruled out)";
        // (override, action, when, beside, over a non-exhaustive xs, over an exhaustive xs):
        // each rule sits beside a rule `true` of action `beside`, over xs = ["x"].
        let rows = [
            (false, "ask", r#"xs.exists(a, a == "-r")"#, "allow",
                (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
            (false, "ask", r#"(xs + ["y"]).exists(a, a == "-r")"#, "allow",
                (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
            (false, "deny", r#""-r" in xs"#, "allow",
                (Decision::Deny, UNSURE), (Decision::Allow, "beside")),
            (false, "allow", r#"!xs.exists(a, a == "-r")"#, "ask",
                (Decision::Ask, "beside"), (Decision::Ask, "beside")),
            (true, "allow", r#"xs.exists(a, a == "-r")"#, "ask",
                (Decision::Ask, "beside"), (Decision::Ask, "beside")),
        ];
        for (is_override, action, when, beside, incomplete, complete) in rows {
            let rules = compile_override_rules(&[("rule", when, action, is_override), ("beside", "true", beside, false)]);
            for (exhaustive, want) in [(false, incomplete), (true, complete)] {
                let got = decide(&rules, &ctx_with("xs", strings(&["x"], exhaustive)));
                assert_eq!((got.decision, got.rule.as_str()), want, "{when}, exhaustive={exhaustive}");
            }
        }
    }

    // --- Unknown (unknowable value) ---

    #[test]
    fn unknown_equality_is_unknown() {
        assert_eq!(eval_cel_with(r#"x == "root""#, &ctx_with("x", TriVal::Unknown)), TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"x != "root""#, &ctx_with("x", TriVal::Unknown)), TriVal::Unknown);
    }

    #[test]
    fn kleene_and_or() {
        let ctx = ctx_with("x", TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"true || x == "root""#, &ctx), TriVal::Bool(true));
        assert_eq!(eval_cel_with(r#"true && x == "root""#, &ctx), TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"false && x == "root""#, &ctx), TriVal::Bool(false));
    }

    // --- Null (definite absence) ---

    #[test]
    fn null_compares_definitely() {
        let ctx = ctx_with("x", TriVal::Null);
        assert_eq!(eval_cel_with(r#"x.name == "sudo""#, &ctx), TriVal::Bool(false));
        assert_eq!(eval_cel_with(r#"x.name != "sudo""#, &ctx), TriVal::Bool(true));
        assert_eq!(eval_cel_with("x == null", &ctx), TriVal::Bool(true));
    }

    // --- Non-exhaustive lists ---

    #[test]
    fn a_non_exhaustive_list_answers_only_what_it_can() {
        let open = ctx_with("xs", strings(&["-v"], false));
        let closed = ctx_with("xs", strings(&["-v"], true));
        assert_eq!(eval_cel_with(r#"xs.exists(a, a == "-v")"#, &open), TriVal::Bool(true));
        assert_eq!(eval_cel_with(r#"xs.exists(a, a == "-rf")"#, &open), TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"xs.exists(a, a == "-rf")"#, &closed), TriVal::Bool(false));
        assert_eq!(eval_cel_with(r#""-v" in xs"#, &open), TriVal::Bool(true));
        assert_eq!(eval_cel_with(r#""-rf" in xs"#, &open), TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"xs.all(a, a == "-v")"#, &open), TriVal::Unknown);
        assert_eq!(eval_cel_with("xs.size()", &open), TriVal::Unknown);
        assert_eq!(eval_cel_with("xs.size()", &closed), TriVal::Int(1));
    }

    // --- Functions ---

    #[test]
    fn glob_matches_wildcard() {
        let ctx = ctx_with("h", TriVal::String("web.prod.example.com".into()));
        assert_eq!(eval_cel_with(r#"glob("*.prod.*", h)"#, &ctx), TriVal::Bool(true));
        let ctx = ctx_with("h", TriVal::String("web.staging.example.com".into()));
        assert_eq!(eval_cel_with(r#"glob("*.prod.*", h)"#, &ctx), TriVal::Bool(false));
        assert_eq!(eval_cel_with(r#"glob("*.prod.*", h)"#, &ctx_with("h", TriVal::Unknown)), TriVal::Unknown);
    }

    #[test]
    fn size_on_strings_and_lists() {
        assert_eq!(eval_cel(r#""echo".size()"#), TriVal::Int(4));
        assert_eq!(eval_cel(r#"size("ls")"#), TriVal::Int(2));
        assert_eq!(eval_cel(r#"size(["a", "b"]) > 1"#), TriVal::Bool(true));
    }

    #[test]
    fn comparison_operators() {
        assert_eq!(eval_cel("2 > 1"), TriVal::Bool(true));
        assert_eq!(eval_cel("1 >= 1"), TriVal::Bool(true));
        assert_eq!(eval_cel("1 < 1"), TriVal::Bool(false));
    }

    #[test]
    fn cel_add_integers_and_lists() {
        assert_eq!(eval_cel("1 + 2"), TriVal::Int(3));
        assert_eq!(eval_cel(r#"["a"] + ["b", "c"]"#), strings(&["a", "b", "c"], true));
        assert_eq!(eval_cel(r#"[] + ["a"]"#), strings(&["a"], true));
        assert_eq!(eval_cel(r#""a" + 1"#), TriVal::Unknown);
    }

    #[test]
    fn cel_map_and_filter() {
        let ctx = ctx_with("xs", strings(&["ls", "-la", "/"], true));
        assert_eq!(eval_cel_with("xs.map(a, a)", &ctx), strings(&["ls", "-la", "/"], true));
        assert_eq!(eval_cel_with(r#"xs.filter(a, a != "-la")"#, &ctx), strings(&["ls", "/"], true));
        assert_eq!(
            eval_cel_with(r#"xs.map(a, a == "-la" ? "long" : a)"#, &ctx),
            strings(&["ls", "long", "/"], true),
        );
    }

    #[test]
    fn cel_ternary() {
        assert_eq!(eval_cel(r#"true ? "yes" : "no""#), TriVal::String("yes".into()));
        assert_eq!(eval_cel(r#"false ? "yes" : "no""#), TriVal::String("no".into()));
        assert_eq!(eval_cel(r#"true ? (false ? "a" : "b") : "c""#), TriVal::String("b".into()));
        assert_eq!(eval_cel("x ? 1 : 2"), TriVal::Unknown);
    }

    // --- Compile errors ---

    #[test]
    fn compile_error_reported() {
        let configs = vec![RuleConfig {
            description: "bad rule".into(),
            when: "invalid @@@ syntax".into(),
            action: "allow".into(),
            is_override: false,
            message: None,
        }];
        match compile(&configs, Decision::Ask) {
            Ok(_) => panic!("expected compile error"),
            Err(err) => assert!(err.contains("bad rule"), "error was: {}", err),
        }
    }

    #[test]
    fn invalid_action_reported() {
        let configs = vec![RuleConfig {
            description: "bad action".into(),
            when: "true".into(),
            action: "invalid".into(),
            is_override: false,
            message: None,
        }];
        match compile(&configs, Decision::Ask) {
            Ok(_) => panic!("expected compile error"),
            Err(err) => assert!(err.contains("invalid"), "error was: {}", err),
        }
    }

    #[test]
    fn builtin_rules_all_compile() {
        let (default, _, tagged) = crate::policy::config::load_merged_config(None);
        if let Err(err) = compile(&tagged, default) {
            panic!("builtin rules failed to compile: {}", err);
        }
    }
}
