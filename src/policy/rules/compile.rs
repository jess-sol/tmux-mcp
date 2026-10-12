//! Compiling one `[[rules]]` entry: its id, its action, its command path, its
//! CEL, and the load-time checks of what the CEL reads; then the ids that
//! the rules of every source exempt.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path};

use cel_parser::ast as cel;

use super::eval::UNKNOWN;
use super::identity::normalize;
use super::{CompiledRule, Part};
use crate::policy::config::{CommandConfig, RuleConfig};
use crate::policy::grammar::Grammars;
use crate::policy::{Decision, Via};

/// The variables an expression reads, and the `args.<name>` it selects.
/// `unknown` is a value, not a variable.
#[derive(Default)]
pub(crate) struct Refs {
    pub(crate) idents: BTreeSet<String>,
    pub(crate) args: BTreeSet<String>,
}

pub(super) fn rule(
    config: &RuleConfig,
    is_project: bool,
    grammars: &Grammars,
) -> Result<CompiledRule, String> {
    if !is_id(&config.id) {
        return Err("an id is lowercase words joined by `-`".into());
    }
    let action = match config.action.as_str() {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        "ask" => Decision::Ask,
        other => return Err(format!("the action `{other}` is not allow, ask or deny")),
    };
    // Without `via`, a rule is for tmux: Bash carries only rules that say so.
    let via = config.via.clone().unwrap_or_else(|| vec![Via::Tmux]);
    if via.is_empty() {
        return Err("`via` names no channel".into());
    }
    let command = config.command.as_ref().map(command_paths).transpose()?;
    let declared = command
        .as_ref()
        .map(|paths| {
            paths
                .iter()
                .map(|path| Ok((path, grammars.declared(path)?)))
                .collect::<Result<Vec<_>, String>>()
        })
        .transpose()?;

    let when = config.when.as_deref().map(parse_cel).transpose()?;
    let path = config.path.as_deref().map(parse_cel).transpose()?;
    let write = config.write.as_deref().map(parse_cel).transpose()?;
    let host = config.host.as_deref().map(parse_cel).transpose()?;
    let user = config.user.as_deref().map(parse_cel).transpose()?;
    let message = config.message.as_deref().map(message).transpose()?;
    let holes = message.iter().flatten().filter_map(|part| match part {
        Part::Hole(expr) => Some(("when", expr)),
        Part::Text(_) => None,
    });

    let keys = [
        ("when", &when),
        ("path", &path),
        ("write", &write),
        ("host", &host),
        ("user", &user),
    ];
    for (key, expr) in keys
        .into_iter()
        .filter_map(|(key, expr)| Some((key, expr.as_ref()?)))
        .chain(holes)
    {
        let refs = Refs::of(&expr.expr);
        if refs.idents.contains("path") && !matches!(key, "path" | "write") {
            return Err("`path` is only for the `path` and `write` keys".into());
        }
        for own in ["host", "user"] {
            if refs.idents.contains(own) && key != own {
                return Err(format!("`{own}` is only for the `{own}` key"));
            }
        }
        let reads_parts = refs.idents.contains("args") || refs.idents.contains("subcommand");
        let Some(declared) = &declared else {
            if reads_parts {
                return Err("`args` and `subcommand` need a `command`".into());
            }
            continue;
        };
        // Each command of a list declares every name the rule reads.
        for (path, names) in declared {
            if let Some(name) = refs.args.iter().find(|n| !names.contains(*n)) {
                let names: Vec<_> = names.iter().map(String::as_str).collect();
                return Err(format!(
                    "`args.{name}` is not declared by `{}`; it declares: {}",
                    path.join(" "),
                    names.join(", ")
                ));
            }
        }
    }

    Ok(CompiledRule {
        id: config.id.clone(),
        command,
        when,
        path,
        write,
        host,
        user,
        action,
        message,
        local_only: is_project,
        via,
        exempts: Vec::new(),
    })
}

/// A message's text and its `{{ expr }}` holes; a hole reads what `when`
/// reads.
fn message(text: &str) -> Result<Vec<Part>, String> {
    let mut parts = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("{{") {
        let Some(close) = rest[open..].find("}}") else {
            return Err("the message has a `{{` without its `}}`".into());
        };
        parts.push(Part::Text(rest[..open].to_string()));
        parts.push(Part::Hole(parse_cel(rest[open + 2..open + close].trim())?));
        rest = &rest[open + close + 2..];
    }
    parts.push(Part::Text(rest.to_string()));
    Ok(parts)
}

/// Each rule's index by its id, which no two rules of any sources share.
pub(super) fn ids(rules: &[CompiledRule]) -> Result<HashMap<&str, usize>, String> {
    let mut ids = HashMap::new();
    for (i, rule) in rules.iter().enumerate() {
        if ids.insert(rule.id.as_str(), i).is_some() {
            return Err(format!("the id `{}` is used twice", rule.id));
        }
    }
    Ok(ids)
}

/// The rule that rule `i` exempts as `id`: one that exists, is stricter,
/// and is judged per path only if rule `i` is too.
pub(super) fn exemption(
    rules: &[CompiledRule],
    i: usize,
    id: &str,
    ids: &HashMap<&str, usize>,
) -> Result<usize, String> {
    let Some(&target) = ids.get(id) else {
        return Err(format!("`exempts` names `{id}`, which no rule has"));
    };
    let (rule, exempted) = (&rules[i], &rules[target]);
    if crate::policy::severity(&exempted.action) <= crate::policy::severity(&rule.action) {
        return Err(format!(
            "`{id}` is not stricter: an allow exempts asks and denies, an ask exempts denies"
        ));
    }
    if exempted.is_per_path() && !rule.is_per_path() {
        return Err(format!(
            "`{id}` is judged per path, so the rule that exempts it needs `path` or `write`"
        ));
    }
    Ok(target)
}

fn is_id(id: &str) -> bool {
    !id.is_empty()
        && id.split('-').all(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// cel-parser panics on some malformed input (`@@`), so a panic is a parse
/// error too.
pub(crate) fn parse_cel(text: &str) -> Result<cel::IdedExpr, String> {
    let parsed = std::panic::catch_unwind(|| cel_parser::Parser::new().parse(text));
    match parsed {
        Ok(Ok(expr)) => Ok(expr),
        Ok(Err(e)) => Err(format!("CEL parse error in `{text}`: {e:?}")),
        Err(_) => Err(format!("CEL parse error in `{text}`")),
    }
}

/// A rule's `command`: one command path, or a list of paths of one depth, so
/// that `subcommand` names the same level for each.
fn command_paths(command: &CommandConfig) -> Result<Vec<Vec<String>>, String> {
    let paths = match command {
        CommandConfig::One(path) => vec![command_path(path)?],
        CommandConfig::Many(paths) => paths
            .iter()
            .map(|p| command_path(p))
            .collect::<Result<_, _>>()?,
    };
    match paths.first() {
        None => Err("`command` is empty".into()),
        Some(first) if paths.iter().any(|p| p.len() != first.len()) => {
            Err("the paths of a `command` list have one depth".into())
        }
        Some(_) => Ok(paths),
    }
}

/// A command path: a bare name or a `./` path from the anchor, then
/// subcommand words.
fn command_path(command: &str) -> Result<Vec<String>, String> {
    let mut words: Vec<String> = command.split_whitespace().map(String::from).collect();
    let Some(first) = words.first_mut() else {
        return Err("`command` is empty".into());
    };
    if first.contains('/') {
        let rest = first.strip_prefix("./").filter(|r| {
            Path::new(r)
                .components()
                .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
        });
        let Some(rest) = rest else {
            return Err(format!(
                "`{first}`: a rule's `command` starts with a bare name or a `./` path"
            ));
        };
        *first = format!("./{}", normalize(Path::new(rest)).display());
    }
    Ok(words)
}

impl Refs {
    pub(crate) fn of(expr: &cel::Expr) -> Refs {
        let mut refs = Refs::default();
        refs.walk(expr, &mut Vec::new());
        refs
    }

    fn walk(&mut self, expr: &cel::Expr, bound: &mut Vec<String>) {
        match expr {
            cel::Expr::Ident(name) if !bound.contains(name) && name != UNKNOWN => {
                _ = self.idents.insert(name.clone())
            }
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
