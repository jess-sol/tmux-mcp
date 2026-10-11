//! Compiling one `[[rules]]` entry: its action, its command path, its CEL,
//! and the load-time checks of what the CEL reads.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use cel_parser::ast as cel;

use super::CompiledRule;
use super::identity::normalize;
use crate::policy::Decision;
use crate::policy::config::RuleConfig;
use crate::policy::grammar::Grammars;

/// The variables an expression reads, and the `args.<name>` it selects.
#[derive(Default)]
struct Refs {
    idents: BTreeSet<String>,
    args: BTreeSet<String>,
}

pub(super) fn rule(config: &RuleConfig, is_project: bool, grammars: &Grammars) -> Result<CompiledRule, String> {
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
