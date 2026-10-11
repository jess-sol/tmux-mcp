//! Rules: compiled from `[[rules]]`, they decide each command of a line.
//!
//! A rule's keys each test one subject: `host` the machine, `command` the
//! command path, `when` its parts, `path` and `write` the paths it touches.
//! CEL is evaluated with three-valued logic (True/False/Unknown):
//! `Unknown == "root"` → Unknown, `Unknown != "root"` → Unknown.
//!
//! Two special values:
//! - Null: definite absence. `Null == X` → False, `Null != X` → True.
//! - Unknown: unknowable value (e.g. a `$(…)` word). All comparisons → Unknown.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};

use cel_parser::ast as cel;
use cel_parser::reference::Val;

use super::config::RuleConfig;
use super::grammar::Grammars;
use super::parse::{CommandInfo, literal};
use super::programs;
use super::{Decision, PolicyResult};

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

    fn of(b: bool) -> TriBool {
        if b { TriBool::True } else { TriBool::False }
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
    /// The command path: an identity, then subcommand words.
    command: Option<Vec<String>>,
    when: Option<cel::IdedExpr>,
    path: Option<cel::IdedExpr>,
    write: Option<cel::IdedExpr>,
    host: Option<cel::IdedExpr>,
    pub action: Decision,
    /// An *Override rule*: it decides alone when it matches.
    pub is_override: bool,
    pub message: Option<String>,
    /// A project rule, which applies only on this machine.
    local_only: bool,
}

pub struct RuleSet {
    pub rules: Vec<CompiledRule>,
    pub default: Decision,
}

/// Where the commands of a line run, as the rules see it.
pub(crate) struct Env {
    pub host: Host,
    /// The hostname of the tmux-mcp process.
    pub this_host: String,
    /// The shell's directory; None when it cannot be known.
    pub pane_cwd: Option<PathBuf>,
    /// `$HOME` on this machine; None elsewhere.
    pub home: Option<PathBuf>,
    /// The directory that relative command identities start from.
    pub anchor: Option<PathBuf>,
    pub project_dirs: Vec<PathBuf>,
    pub foreground: Option<String>,
}

pub(crate) enum Host {
    Here,
    There(String),
    Unknown,
}

/// What one command shows the rules.
struct Facts {
    identity: TriVal,
    subcommands: Vec<TriVal>,
    args: TriVal,
    /// Every resolved path it reads or writes.
    paths: TriVal,
    /// The resolved paths it writes.
    writes: TriVal,
    /// The directory it runs in; None when it cannot be known.
    dir: Option<PathBuf>,
}

/// The variables of one CEL evaluation, and where its paths resolve.
#[derive(Clone)]
struct Ctx<'a> {
    vars: HashMap<String, TriVal>,
    site: Option<Site<'a>>,
}

#[derive(Clone, Copy)]
struct Site<'a> {
    env: &'a Env,
    dir: Option<&'a Path>,
}

/// How a rule matched: on True, or on an Unknown it could not rule out.
enum Match {
    True,
    Unknown,
}

// --- Public API ---

/// Compiles rules, each paired with whether it comes from the project, and
/// checks what each reads against the grammars.
pub fn compile(configs: &[(RuleConfig, bool)], default: Decision, grammars: &Grammars) -> Result<RuleSet, String> {
    let rules = configs
        .iter()
        .map(|(config, is_project)| {
            compile_rule(config, *is_project, grammars).map_err(|e| format!("rule `{}`: {e}", config.description))
        })
        .collect::<Result<_, _>>()?;
    Ok(RuleSet { rules, default })
}

/// The decision of `rules` for one command. Every matching rule counts, and
/// the strictest action wins: deny, then ask, then allow. *Override rule*s
/// are a group of their own: when any matches, the strictest override
/// decides and the other rules do not count. No match gives the default.
pub(crate) fn evaluate(cmd: &CommandInfo, env: &Env, rules: &RuleSet) -> PolicyResult {
    decide(rules, &Facts::of(cmd, env), env)
}

/// `word` as a path: absolute, from `base` when relative. `~/…` resolves in
/// `home`; `~user/…` stays as written, under no directory of this machine.
pub(crate) fn resolve(word: &str, base: Option<&Path>, home: Option<&Path>) -> TriVal {
    let resolved = if word == "~" || word.starts_with("~/") {
        home.map(|h| h.join(word[1..].trim_start_matches('/')))
    } else if word.starts_with('~') {
        return TriVal::String(word.to_string());
    } else if word.starts_with('/') {
        Some(PathBuf::from(word))
    } else {
        base.map(|b| b.join(word))
    };
    match resolved {
        Some(p) => TriVal::String(normalize(&p).to_string_lossy().into_owned()),
        None => TriVal::Unknown,
    }
}

// --- Compiling ---

fn compile_rule(config: &RuleConfig, is_project: bool, grammars: &Grammars) -> Result<CompiledRule, String> {
    let action = match config.action.as_str() {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        "ask" => Decision::Ask,
        other => return Err(format!("the action `{other}` is not allow, ask or deny")),
    };
    let command = config.command.as_deref().map(command_path).transpose()?;
    let declared = command.as_ref().map(|c| grammars.declared(c)).transpose()?;

    let when = config.when.as_deref().map(parse_cel).transpose()?;
    let path = config.path.as_deref().map(parse_cel).transpose()?;
    let write = config.write.as_deref().map(parse_cel).transpose()?;
    let host = config.host.as_deref().map(parse_cel).transpose()?;

    for (key, expr) in [("when", &when), ("path", &path), ("write", &write), ("host", &host)] {
        let Some(expr) = expr else { continue };
        let refs = Refs::of(&expr.expr);
        if refs.idents.contains("path") && !matches!(key, "path" | "write") {
            return Err("`path` is only for the `path` and `write` keys".into());
        }
        if refs.idents.contains("host") && key != "host" {
            return Err("`host` is only for the `host` key".into());
        }
        let reads_parts = refs.idents.contains("args") || refs.idents.contains("subcommand");
        match (&declared, &command) {
            (None, _) if reads_parts => return Err("`args` and `subcommand` need a `command`".into()),
            (Some(names), Some(command)) => {
                if let Some(name) = refs.args.iter().find(|n| !names.contains(*n)) {
                    let names: Vec<_> = names.iter().map(String::as_str).collect();
                    return Err(format!(
                        "`args.{name}` is not declared by `{}`; it declares: {}",
                        command.join(" "),
                        names.join(", ")
                    ));
                }
            }
            _ => {}
        }
    }

    Ok(CompiledRule {
        description: config.description.clone(),
        command,
        when,
        path,
        write,
        host,
        action,
        is_override: config.is_override,
        message: config.message.clone(),
        local_only: is_project,
    })
}

/// A rule's `command`: a bare name or a `./` path from the anchor, then
/// subcommand words.
fn command_path(command: &str) -> Result<Vec<String>, String> {
    let mut words: Vec<String> = command.split_whitespace().map(String::from).collect();
    let Some(first) = words.first_mut() else { return Err("`command` is empty".into()) };
    if first.contains('/') {
        let rest = first.strip_prefix("./").filter(|r| Path::new(r).components().all(|c| matches!(c, Component::Normal(_) | Component::CurDir)));
        let Some(rest) = rest else {
            return Err(format!("`{first}`: a rule's `command` starts with a bare name or a `./` path"));
        };
        *first = format!("./{}", normalize(Path::new(rest)).display());
    }
    Ok(words)
}

/// cel-parser panics on some malformed input (`@@`), so a panic is a parse
/// error too.
fn parse_cel(text: &str) -> Result<cel::IdedExpr, String> {
    let parsed = std::panic::catch_unwind(|| cel_parser::Parser::new().parse(text));
    match parsed {
        Ok(Ok(expr)) => Ok(expr),
        Ok(Err(e)) => Err(format!("CEL parse error in `{text}`: {e:?}")),
        Err(_) => Err(format!("CEL parse error in `{text}`")),
    }
}

/// The variables an expression reads, and the `args.<name>` it selects.
#[derive(Default)]
struct Refs {
    idents: BTreeSet<String>,
    args: BTreeSet<String>,
}

impl Refs {
    fn of(expr: &cel::Expr) -> Refs {
        let mut refs = Refs::default();
        refs.walk(expr, &mut Vec::new());
        refs
    }

    fn walk(&mut self, expr: &cel::Expr, bound: &mut Vec<String>) {
        match expr {
            cel::Expr::Ident(name) if !bound.contains(name) => _ = self.idents.insert(name.clone()),
            cel::Expr::Select(s) => match &s.operand.expr {
                cel::Expr::Ident(name) if name == "args" && !bound.contains(name) => {
                    self.idents.insert(name.clone());
                    self.args.insert(s.field.clone());
                }
                other => self.walk(other, bound),
            },
            cel::Expr::Call(c) => {
                for e in c.target.iter().map(|t| &**t).chain(&c.args) {
                    self.walk(&e.expr, bound);
                }
            }
            cel::Expr::List(l) => {
                for e in &l.elements {
                    self.walk(&e.expr, bound);
                }
            }
            cel::Expr::Comprehension(c) => {
                self.walk(&c.iter_range.expr, bound);
                let depth = bound.len();
                bound.extend([c.iter_var.clone(), c.accu_var.clone()]);
                for e in [&c.accu_init, &c.loop_cond, &c.loop_step, &c.result] {
                    self.walk(&e.expr, bound);
                }
                bound.truncate(depth);
            }
            _ => {}
        }
    }
}

// --- Deciding ---

impl Facts {
    fn of(cmd: &CommandInfo, env: &Env) -> Facts {
        let home = env.home.as_deref();
        let dir = match &cmd.cwd {
            TriVal::Null => env.pane_cwd.clone(),
            TriVal::String(d) => match resolve(d, env.pane_cwd.as_deref(), home) {
                TriVal::String(p) if p.starts_with('/') => Some(PathBuf::from(p)),
                _ => None,
            },
            _ => None,
        };
        let resolve_all = |words: &TriVal| -> (Vec<TriVal>, bool) {
            match words {
                TriVal::List { elements, exhaustive } => {
                    let paths = elements
                        .iter()
                        .map(|w| match w {
                            TriVal::String(w) => resolve(w, dir.as_deref(), home),
                            _ => TriVal::Unknown,
                        })
                        .collect();
                    (paths, *exhaustive)
                }
                _ => (vec![TriVal::Unknown], false),
            }
        };
        let (reads, reads_exhaustive) = resolve_all(&cmd.reads);
        let (mut writes, writes_exhaustive) = resolve_all(&cmd.writes);
        let mut paths = reads;
        paths.extend(writes.iter().cloned());
        for r in &cmd.redirects {
            let target = if r.has_expansion {
                TriVal::Unknown
            } else {
                resolve(&r.target, env.pane_cwd.as_deref(), home)
            };
            if r.is_write {
                writes.push(target.clone());
            }
            paths.push(target);
        }

        Facts {
            identity: identity(&cmd.name, env),
            subcommands: match &cmd.subcommands {
                TriVal::List { elements, .. } => elements.clone(),
                _ => vec![TriVal::Unknown],
            },
            args: TriVal::Map(cmd.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            paths: TriVal::List { elements: paths, exhaustive: reads_exhaustive && writes_exhaustive },
            writes: TriVal::List { elements: writes, exhaustive: writes_exhaustive },
            dir,
        }
    }
}

fn decide(rules: &RuleSet, facts: &Facts, env: &Env) -> PolicyResult {
    let strictest = |overrides: bool| {
        strictest_match(rules.rules.iter().filter(|r| r.is_override == overrides), facts, env)
    };

    strictest(true).or_else(|| strictest(false)).unwrap_or_else(|| PolicyResult {
        decision: rules.default.clone(),
        rule: "default".to_string(),
    })
}

/// The result of the strictest matching rule; on a tie, the first one.
fn strictest_match<'a>(
    rules: impl Iterator<Item = &'a CompiledRule>,
    facts: &Facts,
    env: &Env,
) -> Option<PolicyResult> {
    let mut strictest: Option<PolicyResult> = None;
    for rule in rules {
        let Some(matched) = rule_match(rule, facts, env) else { continue };
        if strictest.as_ref().is_none_or(|s| super::severity(&rule.action) > super::severity(&s.decision)) {
            strictest = Some(rule_result(rule, matched));
        }
    }
    strictest
}

/// Unknown fails closed: it matches a deny or ask rule, never an allow rule,
/// and never an override, whose match switches the other rules off.
fn rule_match(rule: &CompiledRule, facts: &Facts, env: &Env) -> Option<Match> {
    match rule_truth(rule, facts, env) {
        TriBool::True => Some(Match::True),
        TriBool::Unknown if rule.action != Decision::Allow && !rule.is_override => Some(Match::Unknown),
        _ => None,
    }
}

/// A rule matches when every key it has matches.
fn rule_truth(rule: &CompiledRule, facts: &Facts, env: &Env) -> TriBool {
    let site = Site { env, dir: facts.dir.as_deref() };
    let mut truth = host_match(rule, env);
    if truth != TriBool::False {
        truth = truth.and(command_match(rule, facts));
    }
    if let (Some(when), false) = (&rule.when, truth == TriBool::False) {
        truth = truth.and(eval_expr(&when.expr, &when_ctx(rule, facts, site)).is_truthy());
    }
    for (key, paths) in [(&rule.path, &facts.paths), (&rule.write, &facts.writes)] {
        if let (Some(expr), false) = (key, truth == TriBool::False) {
            truth = truth.and(any_path(expr, paths, site));
        }
    }
    truth
}

/// A rule without `host` matches only this machine; a project rule applies
/// only on this machine; a host that cannot be known matches Unknown.
fn host_match(rule: &CompiledRule, env: &Env) -> TriBool {
    let name = match &env.host {
        Host::Unknown => return TriBool::Unknown,
        Host::There(_) if rule.local_only => return TriBool::False,
        Host::Here => &env.this_host,
        Host::There(name) => name,
    };
    match &rule.host {
        None => TriBool::of(matches!(env.host, Host::Here)),
        Some(expr) => {
            let ctx = Ctx { vars: HashMap::from([("host".into(), TriVal::String(name.clone()))]), site: None };
            eval_expr(&expr.expr, &ctx).is_truthy()
        }
    }
}

/// The command is the rule's path or a subcommand below it.
fn command_match(rule: &CompiledRule, facts: &Facts) -> TriBool {
    let Some(path) = &rule.command else { return TriBool::True };
    let mut truth = tri_eq(&facts.identity, &TriVal::String(path[0].clone()));
    for (i, word) in path[1..].iter().enumerate() {
        let below = facts.subcommands.get(i).cloned().unwrap_or(TriVal::Null);
        truth = truth.and(tri_eq(&below, &TriVal::String(word.clone())));
    }
    truth
}

fn when_ctx<'a>(rule: &CompiledRule, facts: &Facts, site: Site<'a>) -> Ctx<'a> {
    let depth = rule.command.as_ref().map_or(0, |c| c.len() - 1);
    let subcommand = facts.subcommands.get(depth).cloned().unwrap_or(TriVal::Null);
    let command = HashMap::from([("name".to_string(), facts.identity.clone())]);
    let foreground = site.env.foreground.clone().map_or(TriVal::Null, TriVal::String);
    let pane = HashMap::from([("foreground".to_string(), foreground)]);
    let vars = HashMap::from([
        ("args".to_string(), facts.args.clone()),
        ("subcommand".to_string(), subcommand),
        ("command".to_string(), TriVal::Map(command)),
        ("pane".to_string(), TriVal::Map(pane)),
    ]);
    Ctx { vars, site: Some(site) }
}

/// True when `expr` holds for any path; over a non-exhaustive list, only a
/// path that holds is certain.
fn any_path(expr: &cel::IdedExpr, paths: &TriVal, site: Site) -> TriBool {
    let TriVal::List { elements, exhaustive } = paths else { return TriBool::Unknown };
    let mut truth = TriBool::False;
    for path in elements {
        let ctx = Ctx { vars: HashMap::from([("path".to_string(), path.clone())]), site: Some(site) };
        truth = truth.or(eval_expr(&expr.expr, &ctx).is_truthy());
        if truth == TriBool::True {
            return truth;
        }
    }
    if *exhaustive { truth } else { TriBool::Unknown }
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
    PolicyResult { decision: rule.action.clone(), rule: rule_desc }
}

// --- Command identity ---

/// A command's identity, from its first word as text: a bare name is itself;
/// a path inside the anchor is `./` and its path from the anchor; another
/// absolute path is its last component; a relative path outside the anchor
/// keeps its `../`. Off this machine a relative path cannot be known.
fn identity(raw: &str, env: &Env) -> TriVal {
    let Some(name) = literal(raw) else { return TriVal::Unknown };
    if !name.contains('/') {
        return TriVal::String(name);
    }
    let last = || TriVal::String(name.rsplit('/').next().unwrap_or_default().to_string());
    let written_absolute = name.starts_with('/') || name.starts_with('~');
    if !matches!(env.host, Host::Here) {
        return if written_absolute { last() } else { TriVal::Unknown };
    }
    let TriVal::String(path) = resolve(&name, env.pane_cwd.as_deref(), env.home.as_deref()) else {
        return TriVal::Unknown;
    };
    let path = PathBuf::from(path);
    match &env.anchor {
        _ if !path.is_absolute() => last(),
        Some(anchor) if path.starts_with(anchor) => {
            TriVal::String(format!("./{}", path.strip_prefix(anchor).unwrap_or(&path).display()))
        }
        _ if written_absolute => last(),
        Some(anchor) => TriVal::String(relative(anchor, &path)),
        None => TriVal::Unknown,
    }
}

/// The path from `from` to `to`, both absolute and normalized.
fn relative(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let ups = std::iter::repeat_n("..".to_string(), from.len() - common);
    let downs = to[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned());
    ups.chain(downs).collect::<Vec<_>>().join("/")
}

/// `.` and `..` resolved as text; `..` above the root stays at the root.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => _ = out.pop(),
            other => out.push(other),
        }
    }
    out
}

// --- Three-valued evaluator ---

fn eval_expr(expr: &cel::Expr, ctx: &Ctx) -> TriVal {
    match expr {
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
        _ => TriVal::Unknown,
    }
}

// --- Comparison operators ---

fn tri_eq(a: &TriVal, b: &TriVal) -> TriBool {
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
fn eval_path_fn(call: &cel::CallExpr, ctx: &Ctx, f: fn(&Path, &Env) -> TriVal) -> TriVal {
    let Some(site) = ctx.site else { return TriVal::Unknown };
    match one_operand(call, ctx) {
        TriVal::String(word) => match resolve(&word, site.dir, site.env.home.as_deref()) {
            TriVal::String(p) => f(Path::new(&p), site.env),
            other => other,
        },
        TriVal::Null => TriVal::Null,
        _ => TriVal::Unknown,
    }
}

/// Under a project directory of this machine, by whole path components.
fn in_project(path: &Path, env: &Env) -> TriVal {
    match env.host {
        Host::Here if path.is_absolute() => TriVal::Bool(env.project_dirs.iter().any(|d| path.starts_with(d))),
        Host::Here | Host::There(_) => TriVal::Bool(false),
        Host::Unknown => TriVal::Unknown,
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

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::config::parse_config;

    const T: &str = "[commands.t]\nparser = \"gnu\"\narguments = [\"xs...\"]\n";

    fn ruleset(rules_toml: &str) -> RuleSet {
        let config = parse_config(&format!("{T}{rules_toml}")).unwrap();
        let grammars = Grammars::compile([&config.commands]).unwrap();
        let rules: Vec<_> = config.rules.into_iter().map(|r| (r, false)).collect();
        compile(&rules, Decision::Ask, &grammars).unwrap_or_else(|e| panic!("{e}"))
    }

    fn env(host: Host) -> Env {
        Env {
            host,
            this_host: "here".into(),
            pane_cwd: Some("/src/proj/sub".into()),
            home: Some("/home/u".into()),
            anchor: Some("/src/proj".into()),
            project_dirs: vec!["/src/proj".into()],
            foreground: Some("bash".into()),
        }
    }

    fn facts_with_xs(xs: TriVal) -> Facts {
        Facts {
            identity: TriVal::String("t".into()),
            subcommands: vec![],
            args: TriVal::Map(HashMap::from([("xs".to_string(), xs)])),
            paths: TriVal::List { elements: vec![], exhaustive: true },
            writes: TriVal::List { elements: vec![], exhaustive: true },
            dir: Some("/src/proj".into()),
        }
    }

    fn vars(pairs: &[(&str, TriVal)]) -> Ctx<'static> {
        Ctx { vars: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), site: None }
    }

    fn strings(vals: &[&str], exhaustive: bool) -> TriVal {
        TriVal::List { elements: vals.iter().map(|v| TriVal::String(v.to_string())).collect(), exhaustive }
    }

    fn eval_cel(expr: &str) -> TriVal {
        eval_cel_with(expr, &vars(&[]))
    }

    fn eval_cel_with(expr: &str, ctx: &Ctx) -> TriVal {
        let ast = cel_parser::Parser::new().parse(expr).unwrap_or_else(|e| panic!("CEL parse error: {:?}", e));
        eval_expr(&ast.expr, ctx)
    }

    // --- Dependency contract: cel-parser ---

    #[test]
    fn cel_parser_names_operators_and_macros() {
        let func = |text: &str| match cel_parser::Parser::new().parse(text).unwrap().expr {
            cel::Expr::Call(c) => c.func_name,
            other => format!("{other:?}"),
        };
        assert_eq!(func(r#"x == "a""#), "_==_");
        assert_eq!(func("a && b"), "_&&_");
        assert_eq!(func(r#"x in ["a"]"#), "@in");
        assert_eq!(func("xs[0]"), "_[_]");
        assert!(matches!(cel_parser::Parser::new().parse("xs.exists(x, x)").unwrap().expr, cel::Expr::Comprehension(_)));
        assert!(matches!(cel_parser::Parser::new().parse("a.b.c").unwrap().expr, cel::Expr::Select(_)));
        assert!(cel_parser::Parser::new().parse("invalid @@@ syntax").is_err());
    }

    // --- Matching and combining ---

    #[test]
    fn a_rule_that_may_match_fires_unless_it_allows_or_overrides() {
        const UNSURE: &str = "rule (could not be ruled out)";
        // (override, action, when, beside, over a non-exhaustive xs, over an exhaustive xs):
        // each rule sits beside a rule of action `beside`, over xs = ["x"].
        let rows = [
            ("false", "ask", r#"args.xs.exists(a, a == "-r")"#, "allow", (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
            ("false", "ask", r#"(args.xs + ["y"]).exists(a, a == "-r")"#, "allow", (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
            ("false", "deny", r#""-r" in args.xs"#, "allow", (Decision::Deny, UNSURE), (Decision::Allow, "beside")),
            ("false", "allow", r#"!args.xs.exists(a, a == "-r")"#, "ask", (Decision::Ask, "beside"), (Decision::Ask, "beside")),
            ("true", "allow", r#"args.xs.exists(a, a == "-r")"#, "ask", (Decision::Ask, "beside"), (Decision::Ask, "beside")),
        ];
        for (is_override, action, when, beside, incomplete, complete) in rows {
            let rules = ruleset(&format!(
                "[[rules]]\ndescription = \"rule\"\ncommand = \"t\"\nwhen = '{when}'\naction = \"{action}\"\noverride = {is_override}\n\
                 [[rules]]\ndescription = \"beside\"\naction = \"{beside}\"\n"
            ));
            for (exhaustive, want) in [(false, incomplete), (true, complete)] {
                let got = decide(&rules, &facts_with_xs(strings(&["x"], exhaustive)), &env(Host::Here));
                assert_eq!((got.decision, got.rule.as_str()), want, "{when}, exhaustive={exhaustive}");
            }
        }
    }

    #[test]
    fn a_rule_meets_only_this_machine_unless_its_host_says_otherwise() {
        let rules = ruleset(
            r#"
            [[rules]]
            description = "anywhere"
            host = 'true'
            action = "deny"
            [[rules]]
            description = "here"
            action = "ask"
            "#,
        );
        let facts = facts_with_xs(strings(&[], true));
        let rows = [
            (Host::Here, Decision::Deny),
            (Host::There("prod".into()), Decision::Deny),
            (Host::Unknown, Decision::Deny),
        ];
        for (host, want) in rows {
            assert_eq!(decide(&rules, &facts, &env(host)).decision, want);
        }
        let here_only = ruleset("[[rules]]\ndescription = \"here\"\naction = \"allow\"\n");
        assert_eq!(decide(&here_only, &facts, &env(Host::There("prod".into()))).decision, Decision::Ask);
        assert_eq!(decide(&here_only, &facts, &env(Host::Unknown)).decision, Decision::Ask);
        assert_eq!(decide(&here_only, &facts, &env(Host::Here)).decision, Decision::Allow);
    }

    #[test]
    fn a_project_rule_applies_only_on_this_machine() {
        let config = parse_config("[[rules]]\ndescription = \"p\"\nhost = 'true'\naction = \"deny\"").unwrap();
        let rules = compile(&[(config.rules[0].clone(), true)], Decision::Ask, &Grammars::default()).unwrap();
        let facts = facts_with_xs(strings(&[], true));
        assert_eq!(decide(&rules, &facts, &env(Host::There("prod".into()))).decision, Decision::Ask);
        assert_eq!(decide(&rules, &facts, &env(Host::Here)).decision, Decision::Deny);
    }

    #[test]
    fn a_path_rule_holds_for_any_path_and_is_unsure_over_an_open_list() {
        let rules = ruleset("[[rules]]\ndescription = \"out\"\npath = '!in_project(path)'\naction = \"ask\"\n");
        let rows = [
            (strings(&["/src/proj/a"], true), "default"),
            (strings(&["/src/proj/a", "/etc/x"], true), "out"),
            (strings(&["/etc/x"], false), "out"),
            (strings(&["/src/proj/a"], false), "out (could not be ruled out)"),
            (TriVal::List { elements: vec![TriVal::Unknown], exhaustive: true }, "out (could not be ruled out)"),
        ];
        for (paths, want) in rows {
            let facts = Facts { paths: paths.clone(), ..facts_with_xs(strings(&[], true)) };
            assert_eq!(decide(&rules, &facts, &env(Host::Here)).rule, want, "{paths:?}");
        }
    }

    // --- Command identity ---

    #[test]
    fn a_command_is_named_by_its_path_from_the_anchor() {
        let rows = [
            ("rm", Host::Here, TriVal::String("rm".into())),
            ("'rm'", Host::Here, TriVal::String("rm".into())),
            ("/usr/bin/rm", Host::Here, TriVal::String("rm".into())),
            ("/src/proj/scripts/bench", Host::Here, TriVal::String("./scripts/bench".into())),
            ("../scripts/bench", Host::Here, TriVal::String("./scripts/bench".into())),
            ("./bin/cat", Host::Here, TriVal::String("./sub/bin/cat".into())),
            ("../../other/x", Host::Here, TriVal::String("../other/x".into())),
            ("~/bin/x", Host::Here, TriVal::String("x".into())),
            ("/usr/bin/rm", Host::There("prod".into()), TriVal::String("rm".into())),
            ("./run", Host::There("prod".into()), TriVal::Unknown),
            ("$X", Host::Here, TriVal::Unknown),
        ];
        for (name, host, want) in rows {
            assert_eq!(identity(name, &env(host)), want, "{name}");
        }
    }

    // --- Paths ---

    #[test]
    fn a_path_resolves_as_text() {
        let base = Some(Path::new("/src/proj"));
        let home = Some(Path::new("/home/u"));
        let rows = [
            ("a/../b", TriVal::String("/src/proj/b".into())),
            ("../../..", TriVal::String("/".into())),
            ("/etc/./x", TriVal::String("/etc/x".into())),
            ("~/x", TriVal::String("/home/u/x".into())),
            ("~bob/x", TriVal::String("~bob/x".into())),
        ];
        for (word, want) in rows {
            assert_eq!(resolve(word, base, home), want, "{word}");
        }
        assert_eq!(resolve("a", None, home), TriVal::Unknown);
    }

    #[test]
    fn path_functions_resolve_against_the_command_directory() {
        let env = env(Host::Here);
        let site = Site { env: &env, dir: Some(Path::new("/src/proj/sub")) };
        let ctx = Ctx { vars: HashMap::new(), site: Some(site) };
        let rows = [
            ("in_project('..')", TriVal::Bool(true)),
            ("in_project('../../proj-evil/x')", TriVal::Bool(false)),
            ("in_project('/src/projx')", TriVal::Bool(false)),
            ("in_project('~bob/x')", TriVal::Bool(false)),
            ("is_device('/dev/null')", TriVal::Bool(true)),
            ("is_device('/devx')", TriVal::Bool(false)),
            ("path('a/../b')", TriVal::String("/src/proj/sub/b".into())),
            ("path(null)", TriVal::Null),
        ];
        for (expr, want) in rows {
            assert_eq!(eval_cel_with(expr, &ctx), want, "{expr}");
        }
        let elsewhere = Env { host: Host::Unknown, ..env };
        let ctx = Ctx { vars: HashMap::new(), site: Some(Site { env: &elsewhere, dir: Some(Path::new("/src/proj")) }) };
        assert_eq!(eval_cel_with("in_project('/src/proj')", &ctx), TriVal::Unknown);
    }

    // --- Unknown, Null and lists ---

    #[test]
    fn unknown_propagates_through_kleene_logic() {
        let ctx = vars(&[("x", TriVal::Unknown)]);
        assert_eq!(eval_cel_with(r#"x == "root""#, &ctx), TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"x != "root""#, &ctx), TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"true || x == "root""#, &ctx), TriVal::Bool(true));
        assert_eq!(eval_cel_with(r#"true && x == "root""#, &ctx), TriVal::Unknown);
        assert_eq!(eval_cel_with(r#"false && x == "root""#, &ctx), TriVal::Bool(false));
    }

    #[test]
    fn null_compares_definitely() {
        let ctx = vars(&[("x", TriVal::Null)]);
        assert_eq!(eval_cel_with(r#"x.name == "sudo""#, &ctx), TriVal::Bool(false));
        assert_eq!(eval_cel_with(r#"x.name != "sudo""#, &ctx), TriVal::Bool(true));
        assert_eq!(eval_cel_with("x == null", &ctx), TriVal::Bool(true));
        assert_eq!(eval_cel_with(r#"x in [null, "show"]"#, &ctx), TriVal::Bool(true));
    }

    #[test]
    fn a_non_exhaustive_list_answers_only_what_it_can() {
        let open = vars(&[("xs", strings(&["-v"], false))]);
        let closed = vars(&[("xs", strings(&["-v"], true))]);
        let rows = [
            (r#"xs.exists(a, a == "-v")"#, TriVal::Bool(true), TriVal::Bool(true)),
            (r#"xs.exists(a, a == "-rf")"#, TriVal::Unknown, TriVal::Bool(false)),
            (r#""-rf" in xs"#, TriVal::Unknown, TriVal::Bool(false)),
            (r#"xs.all(a, a == "-v")"#, TriVal::Unknown, TriVal::Bool(true)),
            ("xs.size()", TriVal::Unknown, TriVal::Int(1)),
            ("xs[0]", TriVal::String("-v".into()), TriVal::String("-v".into())),
            ("xs[1]", TriVal::Unknown, TriVal::Null),
            (r#"xs == ["-v"]"#, TriVal::Unknown, TriVal::Bool(true)),
            ("xs == []", TriVal::Bool(false), TriVal::Bool(false)),
            (r#"xs == ["-x"]"#, TriVal::Bool(false), TriVal::Bool(false)),
            (r#"xs == ["-v", "-x"]"#, TriVal::Unknown, TriVal::Bool(false)),
            (r#"xs == "-v""#, TriVal::Unknown, TriVal::Unknown),
        ];
        for (expr, want_open, want_closed) in rows {
            assert_eq!(eval_cel_with(expr, &open), want_open, "{expr} over an open list");
            assert_eq!(eval_cel_with(expr, &closed), want_closed, "{expr} over a closed list");
        }
    }

    // --- Functions and operators ---

    #[test]
    fn string_functions_test_their_operands() {
        let rows = [
            (r#"glob("*.prod.*", "web.prod.example.com")"#, TriVal::Bool(true)),
            (r#"glob("*.prod.*", "web.staging.example.com")"#, TriVal::Bool(false)),
            (r#"matches("abc123", "^[a-z]+[0-9]+$")"#, TriVal::Bool(true)),
            (r#""abc".matches("[")"#, TriVal::Unknown),
            (r#"contains("abc", "b")"#, TriVal::Bool(true)),
            (r#""abc".startsWith("ab")"#, TriVal::Bool(true)),
            (r#"or(null, "x")"#, TriVal::String("x".into())),
            (r#"awk_is_plain("{print $1}")"#, TriVal::Bool(true)),
            (r#"sed_is_plain("s/a/b/w x")"#, TriVal::Bool(false)),
        ];
        for (expr, want) in rows {
            assert_eq!(eval_cel(expr), want, "{expr}");
        }
        let ctx = vars(&[("h", TriVal::Unknown)]);
        assert_eq!(eval_cel_with(r#"glob("*", h)"#, &ctx), TriVal::Unknown);
    }

    #[test]
    fn operators_compute_over_known_values() {
        let ctx = vars(&[("xs", strings(&["ls", "-la", "/"], true))]);
        let rows = [
            ("2 > 1 && 1 >= 1 && !(1 < 1)", TriVal::Bool(true)),
            ("1 + 2", TriVal::Int(3)),
            (r#"["a"] + ["b"]"#, strings(&["a", "b"], true)),
            (r#""a" + 1"#, TriVal::Unknown),
            (r#"size("ls")"#, TriVal::Int(2)),
            (r#"true ? (false ? "a" : "b") : "c""#, TriVal::String("b".into())),
            ("x ? 1 : 2", TriVal::Unknown),
            (r#"xs.filter(a, a != "-la")"#, strings(&["ls", "/"], true)),
            (r#"xs.map(a, a == "-la" ? "long" : a)"#, strings(&["ls", "long", "/"], true)),
        ];
        for (expr, want) in rows {
            assert_eq!(eval_cel_with(expr, &ctx), want, "{expr}");
        }
    }
}
