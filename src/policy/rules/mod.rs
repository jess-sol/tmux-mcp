//! Rules: compiled from `[[rules]]`, they decide each command of a line.
//!
//! A rule's keys each test one subject: `host` the machine, `user` the user
//! the command runs as, `command` the command path, `when` its parts, `path`
//! and `write` the paths it touches. CEL is evaluated with three-valued logic
//! (True/False/Unknown): `Unknown == "root"` → Unknown, `Unknown != "root"`
//! → Unknown. The evaluator also serves the grammars' `with` keys. A rule
//! loosens a stricter one only through an *Exemption* of its id.

mod compile;
mod eval;
mod identity;
#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use cel_parser::ast as cel;

use super::config::RuleConfig;
use super::grammar::Grammars;
use super::parse::CommandInfo;
use super::value::{TriBool, TriVal};
use super::{Decision, Via};
use eval::{Ctx, Site, eval_expr, tri_eq};
use identity::identity;

pub(crate) use compile::{Refs, parse_cel};
pub(crate) use identity::resolve;

// --- Types ---

pub struct CompiledRule {
    pub id: String,
    /// The command path: an identity, then subcommand words.
    command: Option<Vec<String>>,
    when: Option<cel::IdedExpr>,
    path: Option<cel::IdedExpr>,
    write: Option<cel::IdedExpr>,
    host: Option<cel::IdedExpr>,
    user: Option<cel::IdedExpr>,
    pub action: Decision,
    message: Option<Vec<Part>>,
    /// A project rule, which applies only on this machine.
    local_only: bool,
    /// The one *Channel* it counts on; None for both.
    via: Option<Via>,
    /// The indices of the stricter rules it exempts.
    exempts: Vec<usize>,
}

pub struct RuleSet {
    pub rules: Vec<CompiledRule>,
    pub default: Decision,
}

/// What the rules know beyond the command: this machine, its project, and
/// the *Channel* the line arrived through.
#[derive(Clone)]
pub(crate) struct Env {
    /// The hostname of the tmux-mcp process: "this machine".
    pub this_host: String,
    /// The directory that relative command identities start from.
    pub anchor: Option<PathBuf>,
    pub project_dirs: Vec<PathBuf>,
    pub via: Via,
}

/// The machine a command runs on, against this one.
enum Host {
    Here,
    There(String),
    Unknown,
}

/// What one command shows the rules.
struct Facts {
    identity: TriVal,
    /// Written as a relative path, so its identity is a path even when the
    /// directory cannot be known.
    relative: bool,
    subcommands: Vec<TriVal>,
    args: TriVal,
    /// Every resolved path it reads or writes.
    paths: TriVal,
    /// The resolved paths it writes.
    writes: TriVal,
    /// The directory it runs in; None when it cannot be known.
    dir: Option<PathBuf>,
    variables: TriVal,
    host: Host,
    /// The name of the user it runs as, or Unknown.
    user: TriVal,
    /// It runs as the shell's own user.
    own_user: bool,
    /// The whole line.
    line: String,
    /// The command's place in the line, as `command.*` shows it, with the
    /// bytes that `drop` cuts under keys no rule can read.
    place: HashMap<String, TriVal>,
}

/// A rule's message: text, and CEL holes that render into it.
pub(crate) enum Part {
    Text(String),
    Hole(cel::IdedExpr),
}

/// How a rule matched: on True, or on an Unknown it could not rule out.
enum Match {
    True,
    Unknown,
}

/// How one rule meets one command: its keys judged on the command, and its
/// `path` and `write` keys judged on each path.
struct Meeting {
    command: TriBool,
    path: Option<PathTruths>,
    write: Option<PathTruths>,
}

/// One key judged on each path of a list.
struct PathTruths {
    each: Vec<(TriVal, TriBool)>,
    exhaustive: bool,
}

/// Where an *Exemption* stops a rule counting: on the command, or on paths.
#[derive(Default, Clone)]
struct Exempted {
    whole: bool,
    paths: HashSet<String>,
}

// --- Public API ---

/// Compiles rules, each paired with whether it comes from the project:
/// checks what each reads against the grammars, and resolves the ids that
/// each exempts across all sources.
pub fn compile(configs: &[(RuleConfig, bool)], default: Decision, grammars: &Grammars) -> Result<RuleSet, String> {
    let mut rules: Vec<CompiledRule> = configs
        .iter()
        .map(|(config, is_project)| {
            compile::rule(config, *is_project, grammars).map_err(|e| format!("rule `{}`: {e}", config.id))
        })
        .collect::<Result<_, _>>()?;
    let ids = compile::ids(&rules)?;
    let exempts = configs
        .iter()
        .enumerate()
        .map(|(i, (config, _))| {
            config
                .exempts
                .iter()
                .map(|id| compile::exemption(&rules, i, id, &ids))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("rule `{}`: {e}", config.id))
        })
        .collect::<Result<Vec<_>, _>>()?;
    for (rule, exempts) in rules.iter_mut().zip(exempts) {
        rule.exempts = exempts;
    }
    Ok(RuleSet { rules, default })
}

/// The decision of `rules` for one command of `line`, and the reasons of
/// every rule at that action. Every rule that matches and is not exempted
/// counts, and the strictest action wins: deny, then ask, then allow. No
/// match gives the default.
pub(crate) fn evaluate(cmd: &CommandInfo, line: &str, env: &Env, rules: &RuleSet) -> (Decision, Vec<String>) {
    decide(rules, &Facts::of(cmd, line, env), env)
}

/// Evaluates the CEL of a grammar's `with` key over `vars`. `home(u)` gives
/// `home` when `u` is the command's own user.
pub(crate) fn eval_with(expr: &cel::IdedExpr, vars: HashMap<String, TriVal>, home: TriVal) -> TriVal {
    eval_expr(&expr.expr, &Ctx { vars, site: None, home })
}

// --- Deciding ---

impl Facts {
    fn of(cmd: &CommandInfo, line: &str, env: &Env) -> Facts {
        let as_dir = |cwd: &TriVal| match cwd {
            TriVal::String(d) => Some(PathBuf::from(d)),
            _ => None,
        };
        let list = |paths: &TriVal| match paths {
            TriVal::List { elements, exhaustive } => (elements.clone(), *exhaustive),
            _ => (vec![TriVal::Unknown], false),
        };
        let (mut paths, reads_exhaustive) = list(&cmd.reads);
        let (mut writes, writes_exhaustive) = list(&cmd.writes);
        paths.extend(writes.iter().cloned());
        // A redirect that opens no file (`2>&1`) has a null target.
        for r in cmd.redirects.iter().filter(|r| r.target != TriVal::Null) {
            if r.is_write {
                writes.push(r.target.clone());
            }
            paths.push(r.target.clone());
        }
        let host = match &cmd.context.host {
            TriVal::String(h) if *h == env.this_host => Host::Here,
            TriVal::String(h) => Host::There(h.clone()),
            _ => Host::Unknown,
        };

        Facts {
            identity: identity(&cmd.name, &host, env, as_dir(&cmd.context.cwd).as_deref()),
            relative: matches!(&cmd.name, TriVal::String(n) if n.contains('/') && !n.starts_with(['/', '~'])),
            subcommands: match &cmd.subcommands {
                TriVal::List { elements, .. } => elements.clone(),
                _ => vec![TriVal::Unknown],
            },
            args: TriVal::Map(cmd.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            paths: TriVal::List { elements: paths, exhaustive: reads_exhaustive && writes_exhaustive },
            writes: TriVal::List { elements: writes, exhaustive: writes_exhaustive },
            dir: as_dir(&cmd.cwd),
            variables: cmd.variables.clone(),
            host,
            user: cmd.context.user.clone(),
            own_user: cmd.context.own_user,
            line: line.to_string(),
            place: place(cmd),
        }
    }
}

/// `command.*` beyond its name: the facts of its place in the line.
fn place(cmd: &CommandInfo) -> HashMap<String, TriVal> {
    let text = |s: &str| TriVal::String(s.to_string());
    let op = |o: &Option<String>| o.as_deref().map_or(TriVal::Null, text);
    let strings = |items: Vec<TriVal>| TriVal::List { elements: items, exhaustive: true };
    let range = |(start, end): (usize, usize)| strings(vec![TriVal::Int(start as i64), TriVal::Int(end as i64)]);
    HashMap::from([
        ("text".to_string(), text(&cmd.text)),
        ("top".to_string(), TriVal::Bool(cmd.top)),
        ("op_before".to_string(), op(&cmd.op_before)),
        ("op_after".to_string(), op(&cmd.op_after)),
        ("constant".to_string(), TriVal::Bool(cmd.constant)),
        ("expands".to_string(), strings(cmd.expands.iter().map(|e| text(e)).collect())),
        ("redirects".to_string(), strings(cmd.redirects.iter().map(|r| text(&r.text)).collect())),
        ("cwd".to_string(), cmd.cwd.clone()),
        ("shell_cwd".to_string(), cmd.context.cwd.clone()),
        (CUT.to_string(), range(cmd.cut)),
        (
            REDIRECT_CUTS.to_string(),
            strings(cmd.redirects.iter().map(|r| strings(vec![text(&r.text), range(r.cut)])).collect()),
        ),
    ])
}

/// Keys of `command` that hold the bytes `drop` cuts. They are no CEL
/// identifier, so no rule reads them.
pub(super) const CUT: &str = "@cut";
pub(super) const REDIRECT_CUTS: &str = "@redirect_cuts";

/// The strictest action among the rules that match and are not exempted,
/// and the reason of each rule at that action, in rule order. No match
/// gives the default.
fn decide(rules: &RuleSet, facts: &Facts, env: &Env) -> (Decision, Vec<String>) {
    let meetings: Vec<Meeting> = rules.rules.iter().map(|rule| meet(rule, facts, env)).collect();
    let exempted = exemptions(rules, &meetings, facts);
    let matched: Vec<(&CompiledRule, Match)> = rules
        .rules
        .iter()
        .zip(&meetings)
        .zip(&exempted)
        .filter(|(_, exempted)| !exempted.whole)
        .filter_map(|((rule, meeting), exempted)| Some((rule, rule_match(rule, meeting.truth(&exempted.paths))?)))
        .collect();
    let Some(decision) = matched.iter().map(|(rule, _)| &rule.action).max_by_key(|a| super::severity(a)).cloned() else {
        return (rules.default.clone(), vec!["default".to_string()]);
    };
    let site = Site { env, dir: facts.dir.as_deref(), host: &facts.host };
    let reasons = matched
        .into_iter()
        .filter(|(rule, _)| rule.action == decision)
        .map(|(rule, how)| reason(rule, how, &when_ctx(rule, facts, site)))
        .collect();
    (decision, reasons)
}

/// The *Exemption*s that apply, loosest rules first: allows exempt asks and
/// denies, then the asks that still count exempt denies. A rule exempts only
/// while it surely matches, and an exempted rule's own exemptions do not
/// apply where it is exempted.
fn exemptions(rules: &RuleSet, meetings: &[Meeting], facts: &Facts) -> Vec<Exempted> {
    let mut exempted = vec![Exempted::default(); rules.rules.len()];
    let paths: Vec<&str> = match &facts.paths {
        TriVal::List { elements, .. } => elements.iter().filter_map(text).collect(),
        _ => Vec::new(),
    };
    for action in [Decision::Allow, Decision::Ask] {
        for (i, rule) in rules.rules.iter().enumerate().filter(|(_, r)| r.action == action) {
            let own = exempted[i].clone();
            if own.whole || meetings[i].truth(&own.paths) != TriBool::True {
                continue;
            }
            for &target in &rule.exempts {
                if rules.rules[target].is_per_path() {
                    let sure = paths.iter().filter(|p| !own.paths.contains(**p) && meetings[i].sure_on(p));
                    exempted[target].paths.extend(sure.map(|p| p.to_string()));
                } else if !rule.is_per_path() || own.paths.is_empty() && meetings[i].sure_on_every() {
                    exempted[target].whole = true;
                }
            }
        }
    }
    exempted
}

/// Unknown fails closed: it matches a deny or ask rule, never an allow rule.
fn rule_match(rule: &CompiledRule, truth: TriBool) -> Option<Match> {
    match truth {
        TriBool::True => Some(Match::True),
        TriBool::Unknown if rule.action != Decision::Allow => Some(Match::Unknown),
        _ => None,
    }
}

/// How `rule` meets the command: every key but `path` and `write` judged on
/// the command, and those two judged on each path.
fn meet(rule: &CompiledRule, facts: &Facts, env: &Env) -> Meeting {
    let site = Site { env, dir: facts.dir.as_deref(), host: &facts.host };
    // On another *Channel* the rule is False, so it neither matches nor exempts.
    let mut command = if rule.via.is_some_and(|via| via != env.via) { TriBool::False } else { host_match(rule, facts, env) };
    if command != TriBool::False {
        command = command.and(user_match(rule, facts));
    }
    if command != TriBool::False {
        command = command.and(command_match(rule, facts));
    }
    if let (Some(when), false) = (&rule.when, command == TriBool::False) {
        command = command.and(eval_expr(&when.expr, &when_ctx(rule, facts, site)).is_truthy());
    }
    let judge = |key: &Option<cel::IdedExpr>, paths: &TriVal| key.as_ref().map(|expr| PathTruths::judge(expr, paths, site));
    Meeting { command, path: judge(&rule.path, &facts.paths), write: judge(&rule.write, &facts.writes) }
}

impl CompiledRule {
    /// It has `path` or `write`, so it is judged on each path.
    fn is_per_path(&self) -> bool {
        self.path.is_some() || self.write.is_some()
    }
}

impl Meeting {
    /// The rule matches when every key it has matches; its `path` and
    /// `write` keys skip the paths it is exempted on.
    fn truth(&self, exempted: &HashSet<String>) -> TriBool {
        let mut truth = self.command;
        for key in [&self.path, &self.write].into_iter().flatten() {
            truth = truth.and(key.any(exempted));
        }
        truth
    }

    /// Every key surely holds on the command and on `path`.
    fn sure_on(&self, path: &str) -> bool {
        self.command == TriBool::True
            && [&self.path, &self.write].into_iter().flatten().all(|key| key.at(path) == TriBool::True)
    }

    /// Every key surely holds on the command and on every path of an
    /// exhaustive list.
    fn sure_on_every(&self) -> bool {
        self.command == TriBool::True
            && [&self.path, &self.write]
                .into_iter()
                .flatten()
                .all(|key| key.exhaustive && key.each.iter().all(|(_, t)| *t == TriBool::True))
    }
}

impl PathTruths {
    fn judge(expr: &cel::IdedExpr, paths: &TriVal, site: Site) -> PathTruths {
        let TriVal::List { elements, exhaustive } = paths else {
            return PathTruths { each: Vec::new(), exhaustive: false };
        };
        let each = elements
            .iter()
            .map(|path| {
                let vars = HashMap::from([("path".to_string(), path.clone())]);
                let ctx = Ctx { vars, site: Some(site), home: TriVal::Unknown };
                (path.clone(), eval_expr(&expr.expr, &ctx).is_truthy())
            })
            .collect();
        PathTruths { each, exhaustive: *exhaustive }
    }

    /// True when the key holds for any path but the exempted ones; over a
    /// non-exhaustive list, only a path that holds is certain.
    fn any(&self, exempted: &HashSet<String>) -> TriBool {
        let mut truth = TriBool::False;
        for (path, t) in &self.each {
            if text(path).is_some_and(|p| exempted.contains(p)) {
                continue;
            }
            truth = truth.or(*t);
            if truth == TriBool::True {
                return truth;
            }
        }
        if self.exhaustive { truth } else { TriBool::Unknown }
    }

    /// The key on `path`; False for a path it does not judge.
    fn at(&self, path: &str) -> TriBool {
        self.each.iter().find(|(p, _)| text(p) == Some(path)).map_or(TriBool::False, |(_, t)| *t)
    }
}

/// A known path; an Unknown one is never exempted.
fn text(path: &TriVal) -> Option<&str> {
    match path {
        TriVal::String(p) => Some(p),
        _ => None,
    }
}

/// Rules are strict by default: an ask or deny rule without `host` matches
/// every host, an allow rule without it only this machine. A project rule
/// applies only on this machine; a host that cannot be known matches Unknown.
fn host_match(rule: &CompiledRule, facts: &Facts, env: &Env) -> TriBool {
    match (&facts.host, &rule.host) {
        (Host::There(_), _) if rule.local_only => TriBool::False,
        (Host::Unknown, _) if rule.local_only => TriBool::Unknown,
        (_, None) if rule.action != Decision::Allow => TriBool::True,
        (Host::Unknown, _) => TriBool::Unknown,
        (host, None) => TriBool::of(matches!(host, Host::Here)),
        (Host::Here, Some(expr)) => eval_on(expr, "host", TriVal::String(env.this_host.clone())),
        (Host::There(name), Some(expr)) => eval_on(expr, "host", TriVal::String(name.clone())),
    }
}

/// Rules are strict by default: an ask or deny rule without `user` matches
/// every user, an allow rule without it only the shell's own user, and an
/// Unknown user Unknown.
fn user_match(rule: &CompiledRule, facts: &Facts) -> TriBool {
    match (&rule.user, facts.own_user, &facts.user) {
        (Some(expr), ..) => eval_on(expr, "user", facts.user.clone()),
        (None, ..) if rule.action != Decision::Allow => TriBool::True,
        (None, true, _) => TriBool::True,
        (None, false, TriVal::Unknown) => TriBool::Unknown,
        (None, false, _) => TriBool::False,
    }
}

/// `expr` over the one variable `name`.
fn eval_on(expr: &cel::IdedExpr, name: &str, value: TriVal) -> TriBool {
    let ctx = Ctx { vars: HashMap::from([(name.to_string(), value)]), site: None, home: TriVal::Unknown };
    eval_expr(&expr.expr, &ctx).is_truthy()
}

/// The command is the rule's path or a subcommand below it. A relative
/// name is a path identity wherever it resolves, so it never equals a bare
/// name: spelling a tool as a path can turn its deny into an ask, never
/// gain an allow.
fn command_match(rule: &CompiledRule, facts: &Facts) -> TriBool {
    let Some(path) = &rule.command else { return TriBool::True };
    if facts.relative && !path[0].starts_with("./") {
        return TriBool::False;
    }
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
    let mut command = facts.place.clone();
    command.insert("name".to_string(), facts.identity.clone());
    let vars = HashMap::from([
        ("args".to_string(), facts.args.clone()),
        ("subcommand".to_string(), subcommand),
        ("command".to_string(), TriVal::Map(command)),
        ("variables".to_string(), facts.variables.clone()),
        ("line".to_string(), TriVal::String(facts.line.clone())),
    ]);
    Ctx { vars, site: Some(site), home: TriVal::Unknown }
}

/// The reason starts with the rule's id, the name an *Exemption* uses, then
/// its message with each hole rendered over `ctx`.
fn reason(rule: &CompiledRule, matched: Match, ctx: &Ctx) -> String {
    let mut reason = match &rule.message {
        Some(parts) => {
            let message: String = parts
                .iter()
                .map(|part| match part {
                    Part::Text(text) => text.clone(),
                    Part::Hole(expr) => render(&eval_expr(&expr.expr, ctx)),
                })
                .collect();
            format!("{}: {message}", rule.id)
        }
        None => rule.id.clone(),
    };
    if let Match::Unknown = matched {
        // Tells the model what fired the rule, so it can write the command
        // with literal words.
        reason.push_str(" (could not be ruled out)");
    }
    reason
}

/// A hole's value as text: a list as its elements joined by spaces, null
/// and Unknown as "…".
fn render(value: &TriVal) -> String {
    match value {
        TriVal::String(s) => s.clone(),
        TriVal::Int(n) => n.to_string(),
        TriVal::Bool(b) => b.to_string(),
        TriVal::List { elements, .. } => elements.iter().map(render).collect::<Vec<_>>().join(" "),
        TriVal::Map(_) | TriVal::Null | TriVal::Unknown => "…".to_string(),
    }
}
