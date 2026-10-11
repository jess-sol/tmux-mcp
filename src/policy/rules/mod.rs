//! Rules: compiled from `[[rules]]`, they decide each command of a line.
//!
//! A rule's keys each test one subject: `host` the machine, `command` the
//! command path, `when` its parts, `path` and `write` the paths it touches.
//! CEL is evaluated with three-valued logic (True/False/Unknown):
//! `Unknown == "root"` → Unknown, `Unknown != "root"` → Unknown.

mod compile;
mod eval;
mod identity;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::PathBuf;

use cel_parser::ast as cel;

use super::config::RuleConfig;
use super::grammar::Grammars;
use super::parse::CommandInfo;
use super::value::{TriBool, TriVal};
use super::{Decision, PolicyResult};
use eval::{Ctx, Site, eval_expr, tri_eq};
use identity::identity;

pub(crate) use compile::parse_cel;
pub(crate) use identity::resolve;

// --- Types ---

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
#[derive(Clone)]
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
}

#[derive(Clone)]
pub(crate) enum Host {
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
    /// Unknown when an outer command changed the user it runs as.
    user: TriVal,
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
            compile::rule(config, *is_project, grammars).map_err(|e| format!("rule `{}`: {e}", config.description))
        })
        .collect::<Result<_, _>>()?;
    Ok(RuleSet { rules, default })
}

/// The decision of `rules` for one command. Every matching rule counts, and
/// the strictest action wins: deny, then ask, then allow. *Override rule*s
/// are a group of their own: when any matches, the strictest override
/// decides and the other rules do not count. No match gives the default.
pub(crate) fn evaluate(cmd: &CommandInfo, env: &Env, rules: &RuleSet) -> PolicyResult {
    // A host that an outer command changed cannot be known.
    let changed;
    let env = match cmd.host {
        TriVal::Unknown => {
            changed = Env { host: Host::Unknown, ..env.clone() };
            &changed
        }
        _ => env,
    };
    decide(rules, &Facts::of(cmd, env), env)
}

// --- Deciding ---

impl Facts {
    fn of(cmd: &CommandInfo, env: &Env) -> Facts {
        let home = env.home.as_deref();
        // The shell's directory, where redirects and the command name resolve;
        // a `cd` or an outer command may have changed it.
        let shell_dir = match cmd.cwd {
            TriVal::Unknown => None,
            _ => env.pane_cwd.as_deref(),
        };
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
            let target = if r.has_expansion { TriVal::Unknown } else { resolve(&r.target, shell_dir, home) };
            if r.is_write {
                writes.push(target.clone());
            }
            paths.push(target);
        }

        Facts {
            identity: identity(&cmd.name, env, shell_dir),
            relative: matches!(&cmd.name, TriVal::String(n) if n.contains('/') && !n.starts_with(['/', '~'])),
            subcommands: match &cmd.subcommands {
                TriVal::List { elements, .. } => elements.clone(),
                _ => vec![TriVal::Unknown],
            },
            args: TriVal::Map(cmd.args.iter().map(|(k, v)| (k.clone(), v.clone())).collect()),
            paths: TriVal::List { elements: paths, exhaustive: reads_exhaustive && writes_exhaustive },
            writes: TriVal::List { elements: writes, exhaustive: writes_exhaustive },
            dir,
            variables: cmd.variables.clone(),
            user: cmd.user.clone(),
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

/// A rule matches when every key it has matches, and only a command that
/// runs as the pane's own user.
fn rule_truth(rule: &CompiledRule, facts: &Facts, env: &Env) -> TriBool {
    let site = Site { env, dir: facts.dir.as_deref() };
    let mut truth = host_match(rule, env);
    if facts.user == TriVal::Unknown {
        truth = truth.and(TriBool::Unknown);
    }
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
    let command = HashMap::from([("name".to_string(), facts.identity.clone())]);
    let vars = HashMap::from([
        ("args".to_string(), facts.args.clone()),
        ("subcommand".to_string(), subcommand),
        ("command".to_string(), TriVal::Map(command)),
        ("variables".to_string(), facts.variables.clone()),
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
