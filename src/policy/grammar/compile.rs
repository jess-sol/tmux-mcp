//! Compiling a `[commands.<name>]` table into its levels, with every load
//! error the format names for grammars.

use std::collections::{BTreeMap, BTreeSet};

use globset::Glob;

use super::parser::Parser;
use super::{Level, Marks, Opt, SUBCOMMAND, Slot, SlotKind, With};
use crate::policy::config::{ArgumentConfig, ContextConfig, GrammarConfig, ParserName};
use crate::policy::rules;

/// The names and global spellings of the levels above the one compiling.
#[derive(Default, Clone)]
struct Chain {
    names: BTreeSet<String>,
    global_spellings: BTreeSet<String>,
}

enum Item {
    Slot(Slot),
    Opt(Opt),
}

/// The defaults, the subcommand words, and the arguments that `appends` sets.
type Settings = (
    BTreeMap<String, Vec<String>>,
    BTreeSet<String>,
    BTreeSet<String>,
);

impl Level {
    /// A grammar's top level, with every level below it.
    pub(super) fn compile(path: &str, config: &GrammarConfig) -> Result<Level, String> {
        Level::compile_level(path, config, None, &Chain::default())
    }

    fn compile_level(
        path: &str,
        config: &GrammarConfig,
        inherited: Option<&Parser>,
        chain: &Chain,
    ) -> Result<Level, String> {
        let parser = match (&config.parser, inherited) {
            (Some(p), _) => Parser::from(p),
            (None, Some(p)) => p.clone(),
            (None, None) => return Err(format!("{path}: a top level needs `parser`")),
        };

        let mut slots = Vec::new();
        let mut options = Vec::new();
        for argument in &config.arguments {
            match argument {
                ArgumentConfig::Spec(spec) => match parse_spec(path, spec, &parser)? {
                    Item::Slot(slot) => slots.push(slot),
                    Item::Opt(opt) => options.push(opt),
                },
                ArgumentConfig::Group(specs) => {
                    let (slot, group) = parse_group(path, specs, &parser)?;
                    slots.push(slot);
                    options.extend(group);
                }
            }
        }
        let mut global = Vec::new();
        for argument in &config.global {
            match argument {
                ArgumentConfig::Spec(spec) => match parse_spec(path, spec, &parser)? {
                    Item::Opt(opt) => global.push(opt),
                    Item::Slot(_) => {
                        return Err(format!(
                            "{path}: `{spec}` is an operand, and `global` holds options"
                        ));
                    }
                },
                ArgumentConfig::Group(_) => {
                    return Err(format!("{path}: `global` holds options, not groups"));
                }
            }
        }

        let sub_at = check_slots(path, &slots)?;
        let names = check_names(path, &slots, &options, &global, chain)?;
        let (defaults, words, appends) =
            settings(path, config, &mut slots, &mut options, &mut global)?;
        let marks = marks(path, config, &names, &slots, appends)?;
        let with = with(path, &config.with, |n| {
            names.contains(n) || chain.names.contains(n)
        })?;

        let mut child_chain = chain.clone();
        child_chain.names.extend(names);
        child_chain
            .global_spellings
            .extend(global.iter().flat_map(Opt::spellings));
        let mut tables = BTreeMap::new();
        // A reserved name never gets here: serde reads it as the key of that name.
        for (word, sub) in &config.subcommands {
            if sub_at.is_none() {
                return Err(format!(
                    "{path}: the table `{word}` is on a level without `subcommand`"
                ));
            }
            if words.contains(word) {
                return Err(format!("{path}: `{word}` is both in `words` and a table"));
            }
            let level =
                Level::compile_level(&format!("{path}.{word}"), sub, Some(&parser), &child_chain)?;
            tables.insert(word.clone(), level);
        }
        if sub_at.is_some() && words.is_empty() && tables.is_empty() {
            return Err(format!("{path}: `subcommand` has no words and no tables"));
        }

        Ok(Level {
            parser,
            slots,
            options,
            global,
            defaults,
            marks,
            with,
            words,
            tables,
        })
    }
}

impl Opt {
    fn spellings(&self) -> impl Iterator<Item = String> + '_ {
        let long = self.long.iter().map(|l| format!("--{l}"));
        let short = self.short.iter().map(|c| format!("-{c}"));
        long.chain(short).chain(self.single.iter().cloned())
    }
}

/// One argument string: `"files"`, `"files..."`, `"--force|-f"`, `"--output|-o:"`,
/// `"-exec..."`, `"dir=-C:"` or `"subcommand"`.
fn parse_spec(path: &str, spec: &str, parser: &Parser) -> Result<Item, String> {
    let bad = || format!("{path}: `{spec}` is not an argument string");
    let (prefix, body) = match spec.split_once('=') {
        Some((p, b)) => (Some(p), b),
        None => (None, spec),
    };
    if body == SUBCOMMAND {
        if prefix.is_some() {
            return Err(bad());
        }
        return Ok(Item::Slot(Slot {
            name: SUBCOMMAND.into(),
            kind: SlotKind::Subcommand,
            matches: None,
            group: vec![],
        }));
    }

    if body.starts_with('-') {
        let (body, value, command) = match (body.strip_suffix(':'), body.strip_suffix("...")) {
            (Some(b), _) => (b, true, false),
            (None, Some(b)) => (b, false, true),
            (None, None) => (body, false, false),
        };
        if command && parser.name != ParserName::Find {
            return Err(format!(
                "{path}: `{spec}`: an option spelled with `...` is only for the `find` parser"
            ));
        }
        let mut opt = Opt {
            name: String::new(),
            long: vec![],
            short: vec![],
            single: vec![],
            value,
            command,
            dash: None,
        };
        for spelling in body.split('|') {
            if let Some(long) = spelling.strip_prefix("--") {
                if !is_spelling(long) {
                    return Err(bad());
                }
                opt.long.push(long.to_string());
            } else if let Some(rest) = spelling.strip_prefix('-') {
                if !is_spelling(rest) {
                    return Err(bad());
                }
                let mut chars = rest.chars();
                match (parser.name, chars.next(), chars.next()) {
                    (ParserName::Find, ..) => opt.single.push(spelling.to_string()),
                    (_, Some(c), None) => opt.short.push(c),
                    _ => {
                        return Err(format!(
                            "{path}: `{spelling}` is a single-dash word, which only the `find` parser reads"
                        ));
                    }
                }
            } else {
                return Err(bad());
            }
        }
        let first = body.split('|').next().unwrap_or_default();
        opt.name = name(
            path,
            prefix,
            first.trim_start_matches('-').replace('-', "_"),
        )?;
        return Ok(Item::Opt(opt));
    }

    if body.contains([':', '|']) {
        return Err(bad());
    }
    let (base, kind) = match body.strip_suffix("...") {
        Some(b) => (b, SlotKind::List),
        None => (body, SlotKind::One),
    };
    if base == SUBCOMMAND {
        return Err(format!("{path}: `subcommand` takes no `...`"));
    }
    let name = name(path, prefix, base.to_string())?;
    Ok(Item::Slot(Slot {
        name,
        kind,
        matches: None,
        group: vec![],
    }))
}

/// A group: one operand, then the options that stand in for it.
fn parse_group(path: &str, specs: &[String], parser: &Parser) -> Result<(Slot, Vec<Opt>), String> {
    let bad =
        || format!("{path}: a group is one operand, then options that take a value: {specs:?}");
    let mut items = specs.iter().map(|s| parse_spec(path, s, parser));
    let Some(Item::Slot(mut slot)) = items.next().transpose()? else {
        return Err(bad());
    };
    if slot.kind != SlotKind::One {
        return Err(bad());
    }
    let mut group = Vec::new();
    for item in items {
        match item? {
            Item::Opt(opt) if opt.value => group.push(opt),
            _ => return Err(bad()),
        }
    }
    if group.is_empty() {
        return Err(bad());
    }
    slot.group = group.iter().map(|o| o.name.clone()).collect();
    Ok((slot, group))
}

fn name(path: &str, prefix: Option<&str>, derived: String) -> Result<String, String> {
    let name = match prefix {
        Some(p) if p == derived => {
            return Err(format!(
                "{path}: `{p}=` repeats the name the argument already has"
            ));
        }
        Some(p) => p.to_string(),
        None => derived,
    };
    let mut chars = name.chars();
    let snake = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !snake {
        return Err(format!(
            "{path}: the name `{name}` is not snake_case; give one with `name=`"
        ));
    }
    Ok(name)
}

fn is_spelling(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The index of the subcommand slot, after checking where it stands.
fn check_slots(path: &str, slots: &[Slot]) -> Result<Option<usize>, String> {
    let subs: Vec<usize> = slots
        .iter()
        .enumerate()
        .filter(|(_, s)| s.kind == SlotKind::Subcommand)
        .map(|(i, _)| i)
        .collect();
    match subs[..] {
        [] => Ok(None),
        [at] if slots[..at].iter().any(|s| s.kind == SlotKind::List) => Err(format!(
            "{path}: a list operand stands before `subcommand` and would take its word"
        )),
        [at] => Ok(Some(at)),
        _ => Err(format!("{path}: `subcommand` is declared twice")),
    }
}

fn check_names(
    path: &str,
    slots: &[Slot],
    options: &[Opt],
    global: &[Opt],
    chain: &Chain,
) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let slot_names = slots
        .iter()
        .filter(|s| s.kind != SlotKind::Subcommand)
        .map(|s| &s.name);
    for name in slot_names.chain(options.iter().chain(global).map(|o| &o.name)) {
        if chain.names.contains(name) || !names.insert(name.clone()) {
            return Err(format!(
                "{path}: `{name}` is declared twice along its levels"
            ));
        }
    }
    for spelling in options.iter().chain(global).flat_map(Opt::spellings) {
        if chain.global_spellings.contains(&spelling) {
            return Err(format!(
                "{path}: `{spelling}` is already a global option of a level above"
            ));
        }
    }
    Ok(names)
}

fn settings(
    path: &str,
    config: &GrammarConfig,
    slots: &mut [Slot],
    options: &mut [Opt],
    global: &mut [Opt],
) -> Result<Settings, String> {
    let mut defaults = BTreeMap::new();
    let mut words = BTreeSet::new();
    let mut appends = BTreeSet::new();
    for (name, s) in &config.argument {
        let mut opt = options
            .iter_mut()
            .chain(global.iter_mut())
            .find(|o| &o.name == name);
        let slot = slots.iter_mut().find(|x| &x.name == name);
        let kind = match (&slot, &opt) {
            (Some(slot), _) => Some(slot.kind),
            (None, Some(_)) => None,
            (None, None) => {
                return Err(format!(
                    "{path}: `argument.{name}` names no argument of this level"
                ));
            }
        };
        if s.dash {
            let Some(o) = opt.as_deref_mut().filter(|o| o.value) else {
                return Err(format!(
                    "{path}: `argument.{name}.dash` is only for an option with a value"
                ));
            };
            let names = s.words.iter().flatten().map(|w| w.to_uppercase());
            o.dash = Some(names.collect());
        } else if let Some(w) = &s.words {
            if kind != Some(SlotKind::Subcommand) {
                return Err(format!(
                    "{path}: `argument.{name}.words` is only for `subcommand` or a `dash` option"
                ));
            }
            words = w.iter().cloned().collect();
        }
        if let Some(pattern) = &s.matches {
            let Some(slot) = slot.filter(|s| s.kind != SlotKind::Subcommand) else {
                return Err(format!(
                    "{path}: `argument.{name}.match` is only for an operand"
                ));
            };
            let glob =
                Glob::new(pattern).map_err(|e| format!("{path}: `argument.{name}.match`: {e}"))?;
            slot.matches = Some(glob.compile_matcher());
        }
        if let Some(d) = &s.default {
            let is_flag = opt.is_some_and(|o| !o.value);
            if is_flag || kind == Some(SlotKind::Subcommand) {
                return Err(format!(
                    "{path}: `argument.{name}.default` is only for an operand or an option with a value"
                ));
            }
            defaults.insert(name.clone(), d.clone());
        }
        if s.appends {
            appends.insert(name.clone());
        }
    }
    Ok((defaults, words, appends))
}

fn marks(
    path: &str,
    config: &GrammarConfig,
    names: &BTreeSet<String>,
    slots: &[Slot],
    appends: BTreeSet<String>,
) -> Result<Marks, String> {
    let m = &config.marks;
    let lists = [
        ("read", &m.read),
        ("write", &m.write),
        ("cwd", &m.cwd),
        ("command", &m.command),
        ("shell", &m.shell),
        ("variables", &m.variables),
    ];
    for (mark, list) in lists {
        if let Some(name) = list.iter().find(|n| !names.contains(*n)) {
            return Err(format!(
                "{path}: `marks.{mark}` names `{name}`, which this level does not declare"
            ));
        }
    }
    if let Some(slot) = slots
        .iter()
        .find(|s| s.kind == SlotKind::One && m.command.contains(&s.name))
    {
        return Err(format!(
            "{path}: `marks.command` names `{}`, a one-word operand; a command is `{0}...`",
            slot.name
        ));
    }
    if let Some(name) = appends.iter().find(|n| !m.command.contains(*n)) {
        return Err(format!(
            "{path}: `argument.{name}.appends` is on an argument that `marks.command` does not name"
        ));
    }
    Ok(Marks {
        read: m.read.clone(),
        write: m.write.clone(),
        cwd: m.cwd.clone(),
        command: m.command.clone(),
        shell: m.shell.clone(),
        variables: m.variables.clone(),
        appends,
    })
}

/// The CEL of the fields that `with` sets. It reads only `args` of the names
/// along the level's chain, the command's `cwd`, `user` and `host`, and `with`.
fn with(
    path: &str,
    config: &ContextConfig,
    declared: impl Fn(&str) -> bool,
) -> Result<With, String> {
    let compile = |field: &str, text: &Option<String>| {
        let Some(text) = text else { return Ok(None) };
        let bad = |e: String| format!("{path}: `with.{field}`: {e}");
        let expr = rules::parse_cel(text).map_err(bad)?;
        let refs = rules::Refs::of(&expr.expr);
        if let Some(name) = refs
            .idents
            .iter()
            .find(|n| !matches!(n.as_str(), "args" | "cwd" | "user" | "host" | "with"))
        {
            return Err(bad(format!(
                "`{name}` is not one of args, cwd, user, host and with"
            )));
        }
        if let Some(name) = refs.args.iter().find(|n| !declared(n)) {
            return Err(bad(format!(
                "`args.{name}` is not declared along this level"
            )));
        }
        Ok(Some(expr))
    };
    Ok(With {
        cwd: compile("cwd", &config.cwd)?,
        user: compile("user", &config.user)?,
        host: compile("host", &config.host)?,
    })
}
