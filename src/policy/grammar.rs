//! Grammars: each `[commands.<name>]` table compiled once, then used to parse
//! the words of one command into named parts. A grammar never decides; rules
//! read what it parses.

use std::collections::{BTreeMap, BTreeSet};

use globset::{Glob, GlobMatcher};

use super::config::{ArgumentConfig, GrammarConfig, ParserConfig, ParserName};
use super::parse::Word;
use super::rules::TriVal;

// --- Types ---

/// The compiled grammars of the sources that apply, by command name.
#[derive(Default)]
pub struct Grammars {
    commands: BTreeMap<String, Level>,
}

/// What a grammar makes of one command's words.
pub(crate) struct Parse {
    pub subcommands: TriVal,
    pub args: BTreeMap<String, TriVal>,
    pub cwd: TriVal,
    pub reads: TriVal,
    pub writes: TriVal,
}

/// A grammar's top level or one of its subcommand levels.
struct Level {
    parser: Parser,
    /// Operands in the order they take words; the subcommand position is one.
    slots: Vec<Slot>,
    /// Options that parse only before this level's subcommand word.
    options: Vec<Opt>,
    /// Options that parse at this level and every level below.
    global: Vec<Opt>,
    defaults: BTreeMap<String, Vec<String>>,
    marks: Marks,
    /// Subcommand words that parse the rest with this level.
    words: BTreeSet<String>,
    /// Subcommand words that parse the rest with a level of their own.
    tables: BTreeMap<String, Level>,
}

#[derive(Clone)]
struct Parser {
    name: ParserName,
    leading_number: Option<String>,
    number: Option<String>,
    short_equals: bool,
}

struct Slot {
    name: String,
    kind: SlotKind,
    matches: Option<GlobMatcher>,
    /// The options of its group: when any is given, the slot takes no word.
    group: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    One,
    List,
    Subcommand,
}

struct Opt {
    name: String,
    /// Long spellings without their `--`.
    long: Vec<String>,
    short: Vec<char>,
    /// The `find` parser's spellings: whole words, dash included.
    single: Vec<String>,
    value: bool,
}

#[derive(Default)]
struct Marks {
    read: Vec<String>,
    write: Vec<String>,
    cwd: Vec<String>,
}

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

/// The state of one command's parse, across its levels.
#[derive(Default)]
struct Run<'g> {
    parts: BTreeMap<String, TriVal>,
    subcommands: Vec<TriVal>,
    levels: Vec<&'g Level>,
    /// An Unknown word could be any option: every part it may reach,
    /// given or not, reads Unknown, and parsing stops.
    unsure: bool,
    /// After an undeclared option or subcommand word, a later word may be
    /// its value: what is given from here reads Unknown. What no word gives
    /// stays known.
    doubt: bool,
    /// A word holds a role the grammar does not know: operands read Unknown
    /// and the path lists are non-exhaustive.
    open: bool,
}

/// What an option word did.
struct Read {
    /// How many of the following words it took as its value.
    taken: usize,
    /// It is not declared and may take a value the grammar cannot see.
    undeclared: bool,
}

/// What became of an operand word offered to the slots before the subcommand.
enum Placed {
    Taken,
    /// No slot before the subcommand takes it: it is the subcommand word.
    Subcommand,
    Unsure,
}

const RESERVED: [&str; 7] = ["parser", "arguments", "global", "argument", "marks", "with", "then"];
const SUBCOMMAND: &str = "subcommand";

// --- Public API ---

impl Grammars {
    /// Compiles the grammars of each source in turn; a later source's grammar
    /// replaces an earlier one of the same name, with all its levels.
    pub(crate) fn compile<'a>(
        sources: impl IntoIterator<Item = &'a BTreeMap<String, GrammarConfig>>,
    ) -> Result<Grammars, String> {
        let mut commands = BTreeMap::new();
        for source in sources {
            for (name, config) in source {
                let level = Level::compile(&format!("commands.{name}"), config, None, &Chain::default())?;
                commands.insert(name.clone(), level);
            }
        }
        Ok(Grammars { commands })
    }

    /// Parses a command's words with the grammar of its name, else of the
    /// last component of its path. A command without a grammar has no parts.
    pub(crate) fn parse(&self, name: &str, words: &[Word]) -> Parse {
        let Some(level) = self.lookup(name) else {
            return Parse {
                subcommands: list(Vec::new(), true),
                args: BTreeMap::new(),
                cwd: TriVal::Null,
                reads: list(Vec::new(), true),
                writes: list(Vec::new(), true),
            };
        };
        let mut run = Run::default();
        level.parse(&[], words, &mut run);
        run.finish()
    }

    /// The names that the levels along `command` declare, for a rule's
    /// `command` key: `["git", "stash"]` gives git's names and stash's.
    pub(crate) fn declared(&self, command: &[String]) -> Result<BTreeSet<String>, String> {
        let shown = command.join(" ");
        let Some(mut level) = self.lookup(&command[0]) else {
            if command.len() > 1 {
                return Err(format!("`{}` has no grammar, so `{shown}` names no subcommand", command[0]));
            }
            return Ok(BTreeSet::new());
        };
        let mut names = level.names();
        let mut past_words = false;
        for word in &command[1..] {
            if let (false, Some(table)) = (past_words, level.tables.get(word)) {
                level = table;
                names.extend(level.names());
            } else if !past_words && level.words.contains(word) {
                past_words = true;
            } else {
                return Err(format!("`{shown}`: `{word}` is not a subcommand that its grammar allows"));
            }
        }
        Ok(names)
    }

    fn lookup(&self, name: &str) -> Option<&Level> {
        self.commands.get(name).or_else(|| self.commands.get(name.rsplit('/').next()?))
    }
}

// --- Compiling ---

impl Level {
    fn compile(path: &str, config: &GrammarConfig, inherited: Option<&Parser>, chain: &Chain) -> Result<Level, String> {
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
                    Item::Slot(_) => return Err(format!("{path}: `{spec}` is an operand, and `global` holds options")),
                },
                ArgumentConfig::Group(_) => return Err(format!("{path}: `global` holds options, not groups")),
            }
        }

        let sub_at = check_slots(path, &slots)?;
        let names = check_names(path, &slots, &options, &global, chain)?;
        let (defaults, words) = settings(path, config, &mut slots, &options, &global)?;
        let marks = marks(path, config, &names)?;

        let mut child_chain = chain.clone();
        child_chain.names.extend(names);
        child_chain.global_spellings.extend(global.iter().flat_map(Opt::spellings));
        let mut tables = BTreeMap::new();
        for (word, sub) in &config.subcommands {
            if RESERVED.contains(&word.as_str()) {
                return Err(format!("{path}: `{word}` is a reserved name, not a subcommand"));
            }
            if sub_at.is_none() {
                return Err(format!("{path}: the table `{word}` is on a level without `subcommand`"));
            }
            if words.contains(word) {
                return Err(format!("{path}: `{word}` is both in `words` and a table"));
            }
            let level = Level::compile(&format!("{path}.{word}"), sub, Some(&parser), &child_chain)?;
            tables.insert(word.clone(), level);
        }
        if sub_at.is_some() && words.is_empty() && tables.is_empty() {
            return Err(format!("{path}: `subcommand` has no words and no tables"));
        }

        Ok(Level { parser, slots, options, global, defaults, marks, words, tables })
    }

    /// The names this level declares, its global options included.
    fn names(&self) -> BTreeSet<String> {
        let slots = self.slots.iter().filter(|s| s.kind != SlotKind::Subcommand).map(|s| &s.name);
        let options = self.options.iter().chain(&self.global).map(|o| &o.name);
        slots.chain(options).cloned().collect()
    }
}

impl Parser {
    fn from(config: &ParserConfig) -> Parser {
        match config {
            ParserConfig::Name(name) => {
                Parser { name: *name, leading_number: None, number: None, short_equals: false }
            }
            ParserConfig::Settings(s) => Parser {
                name: s.name,
                leading_number: s.leading_number.clone(),
                number: s.number.clone(),
                short_equals: s.short_equals,
            },
        }
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
/// `"dir=-C:"` or `"subcommand"`.
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
        return Ok(Item::Slot(Slot { name: SUBCOMMAND.into(), kind: SlotKind::Subcommand, matches: None, group: vec![] }));
    }

    if body.starts_with('-') {
        let (body, value) = match body.strip_suffix(':') {
            Some(b) => (b, true),
            None => (body, false),
        };
        let mut opt = Opt { name: String::new(), long: vec![], short: vec![], single: vec![], value };
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
        opt.name = name(path, prefix, first.trim_start_matches('-').replace('-', "_"))?;
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
    Ok(Item::Slot(Slot { name, kind, matches: None, group: vec![] }))
}

/// A group: one operand, then the options that stand in for it.
fn parse_group(path: &str, specs: &[String], parser: &Parser) -> Result<(Slot, Vec<Opt>), String> {
    let bad = || format!("{path}: a group is one operand, then options that take a value: {specs:?}");
    let mut items = specs.iter().map(|s| parse_spec(path, s, parser));
    let Some(Item::Slot(mut slot)) = items.next().transpose()? else { return Err(bad()) };
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
        Some(p) if p == derived => return Err(format!("{path}: `{p}=` repeats the name the argument already has")),
        Some(p) => p.to_string(),
        None => derived,
    };
    let mut chars = name.chars();
    let snake = chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !snake {
        return Err(format!("{path}: the name `{name}` is not snake_case; give one with `name=`"));
    }
    Ok(name)
}

fn is_spelling(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The index of the subcommand slot, after checking where it stands.
fn check_slots(path: &str, slots: &[Slot]) -> Result<Option<usize>, String> {
    let subs: Vec<usize> =
        slots.iter().enumerate().filter(|(_, s)| s.kind == SlotKind::Subcommand).map(|(i, _)| i).collect();
    match subs[..] {
        [] => Ok(None),
        [at] if slots[..at].iter().any(|s| s.kind == SlotKind::List) => {
            Err(format!("{path}: a list operand stands before `subcommand` and would take its word"))
        }
        [at] => Ok(Some(at)),
        _ => Err(format!("{path}: `subcommand` is declared twice")),
    }
}

fn check_names(path: &str, slots: &[Slot], options: &[Opt], global: &[Opt], chain: &Chain) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    let slot_names = slots.iter().filter(|s| s.kind != SlotKind::Subcommand).map(|s| &s.name);
    for name in slot_names.chain(options.iter().chain(global).map(|o| &o.name)) {
        if chain.names.contains(name) || !names.insert(name.clone()) {
            return Err(format!("{path}: `{name}` is declared twice along its levels"));
        }
    }
    for spelling in options.iter().chain(global).flat_map(Opt::spellings) {
        if chain.global_spellings.contains(&spelling) {
            return Err(format!("{path}: `{spelling}` is already a global option of a level above"));
        }
    }
    Ok(names)
}

type Settings = (BTreeMap<String, Vec<String>>, BTreeSet<String>);

fn settings(path: &str, config: &GrammarConfig, slots: &mut [Slot], options: &[Opt], global: &[Opt]) -> Result<Settings, String> {
    let mut defaults = BTreeMap::new();
    let mut words = BTreeSet::new();
    for (name, s) in &config.argument {
        let opt = options.iter().chain(global).find(|o| &o.name == name);
        let slot = slots.iter_mut().find(|x| &x.name == name);
        let kind = match (&slot, opt) {
            (Some(slot), _) => Some(slot.kind),
            (None, Some(_)) => None,
            (None, None) => return Err(format!("{path}: `argument.{name}` names no argument of this level")),
        };
        if let Some(w) = &s.words {
            if kind != Some(SlotKind::Subcommand) {
                return Err(format!("{path}: `argument.{name}.words` is only for `subcommand`"));
            }
            words = w.iter().cloned().collect();
        }
        if let Some(pattern) = &s.matches {
            let Some(slot) = slot.filter(|s| s.kind != SlotKind::Subcommand) else {
                return Err(format!("{path}: `argument.{name}.match` is only for an operand"));
            };
            let glob = Glob::new(pattern).map_err(|e| format!("{path}: `argument.{name}.match`: {e}"))?;
            slot.matches = Some(glob.compile_matcher());
        }
        if let Some(d) = &s.default {
            let is_flag = opt.is_some_and(|o| !o.value);
            if is_flag || kind == Some(SlotKind::Subcommand) {
                return Err(format!("{path}: `argument.{name}.default` is only for an operand or an option with a value"));
            }
            defaults.insert(name.clone(), d.clone());
        }
    }
    Ok((defaults, words))
}

fn marks(path: &str, config: &GrammarConfig, names: &BTreeSet<String>) -> Result<Marks, String> {
    let m = &config.marks;
    for (mark, list) in [("read", &m.read), ("write", &m.write), ("cwd", &m.cwd)] {
        if let Some(name) = list.iter().find(|n| !names.contains(*n)) {
            return Err(format!("{path}: `marks.{mark}` names `{name}`, which this level does not declare"));
        }
    }
    Ok(Marks { read: m.read.clone(), write: m.write.clone(), cwd: m.cwd.clone() })
}

// --- Parsing ---

impl Level {
    fn parse<'g>(&'g self, globals: &[&'g Opt], words: &[Word], run: &mut Run<'g>) {
        run.levels.push(self);
        for slot in &self.slots {
            match slot.kind {
                SlotKind::One => _ = run.parts.insert(slot.name.clone(), TriVal::Null),
                SlotKind::List => _ = run.parts.insert(slot.name.clone(), list(Vec::new(), true)),
                SlotKind::Subcommand => {}
            }
        }
        for opt in self.options.iter().chain(&self.global) {
            let empty = if opt.value { list(Vec::new(), true) } else { TriVal::Bool(false) };
            run.parts.insert(opt.name.clone(), empty);
        }
        let mut globals = globals.to_vec();
        globals.extend(&self.global);

        let sub_at = self.slots.iter().position(|s| s.kind == SlotKind::Subcommand);
        let (pre, post) = match sub_at {
            Some(at) => (&self.slots[..at], &self.slots[at + 1..]),
            None => (&[][..], &self.slots[..]),
        };
        // Operands go to `post` once no subcommand word can come.
        let mut past_sub = sub_at.is_none();
        let mut sub_reached = sub_at.is_none();
        // A `words` word ends this level's own options.
        let mut after_word = false;
        let mut options_end = false;
        let mut expression = false;
        let mut next_pre = 0;
        let mut operands = Vec::new();
        let mut i = 0;
        while i < words.len() && !run.unsure {
            let word = &words[i];
            let at_start = i == 0;
            i += 1;
            match word {
                Word::Unknown { splits } if !options_end || *splits => {
                    run.unsure = true;
                    break;
                }
                Word::Known(text) if !options_end && self.parser.is_option(text, at_start) => {
                    let own = if after_word { &[][..] } else { &self.options[..] };
                    let active: Vec<&Opt> = own.iter().chain(globals.iter().copied()).collect();
                    let read = self.option(text, &words[i..], at_start, &active, &mut options_end, &mut expression, run);
                    i += read.taken;
                    if read.undeclared {
                        run.doubt = true;
                        run.open = true;
                        // The subcommand word may be this option's value, or come later.
                        if !sub_reached {
                            run.subcommands.push(TriVal::Unknown);
                            sub_reached = true;
                            past_sub = true;
                        }
                    }
                    continue;
                }
                _ => {}
            }

            if self.parser.name == ParserName::Posix {
                options_end = true;
            }
            if self.parser.name == ParserName::Find && expression {
                // An expression word that is no option's value: no slot holds it.
                run.open = true;
                continue;
            }
            if past_sub {
                operands.push(word.clone());
                continue;
            }
            match place(pre, &mut next_pre, word, run) {
                Placed::Taken => continue,
                Placed::Unsure => break,
                Placed::Subcommand => {}
            }
            sub_reached = true;
            match word {
                Word::Known(w) if self.tables.contains_key(w) => {
                    run.subcommands.push(TriVal::String(w.clone()));
                    self.tables[w].parse(&globals, &words[i..], run);
                    return;
                }
                Word::Known(w) if self.words.contains(w) => {
                    run.subcommands.push(TriVal::String(w.clone()));
                    past_sub = true;
                    after_word = true;
                }
                _ => {
                    // A word the grammar does not allow: what it does is unknown,
                    // and this level's own options no longer parse after it.
                    run.subcommands.push(TriVal::Unknown);
                    run.doubt = true;
                    run.open = true;
                    past_sub = true;
                    after_word = true;
                }
            }
        }

        if !run.unsure && !run.open && !assign(post, operands, run) {
            run.open = true;
        }
        if run.open {
            for slot in &self.slots {
                match slot.kind {
                    SlotKind::One if run.parts.get(&slot.name) == Some(&TriVal::Null) || post_slot(post, slot) => {
                        run.parts.insert(slot.name.clone(), TriVal::Unknown);
                    }
                    SlotKind::List => _ = run.parts.insert(slot.name.clone(), TriVal::Unknown),
                    _ => {}
                }
            }
        }
        if run.unsure {
            let own_active = !after_word && (sub_at.is_none() || !sub_reached);
            self.mark_unsure(&globals, own_active, sub_reached, run);
        }
    }

    /// Reads the option word `text`: matched exactly, never by prefix.
    #[allow(clippy::too_many_arguments)]
    fn option(
        &self,
        text: &str,
        rest: &[Word],
        at_start: bool,
        active: &[&Opt],
        options_end: &mut bool,
        expression: &mut bool,
        run: &mut Run,
    ) -> Read {
        let p = &self.parser;
        // An undeclared option took nothing only when nothing follows it.
        let undeclared = |attached: bool| Read { taken: 0, undeclared: attached || !rest.is_empty() };
        let took = |taken| Read { taken, undeclared: false };
        if p.name == ParserName::Find {
            if is_find_operator(text) {
                *expression = true;
                return took(0);
            }
            // -H, -L and -P stand before the start points.
            if !matches!(text, "-H" | "-L" | "-P") || *expression {
                *expression = true;
            }
            return match active.iter().find(|o| o.single.iter().any(|s| s == text)) {
                Some(o) if o.value => took(take_value(o, None, rest, run)),
                Some(o) => took(set_flag(o, run)),
                None => undeclared(false),
            };
        }
        if text == "--" {
            *options_end = true;
            return took(0);
        }
        if let Some(name) = &p.leading_number
            && at_start
            && is_number(text, true)
        {
            push_value(run, name, Word::Known(text.trim_start_matches('-').to_string()));
            return took(0);
        }
        if let Some(name) = &p.number
            && is_number(text, false)
        {
            push_value(run, name, Word::Known(text[1..].to_string()));
            return took(0);
        }
        if let Some(long) = text.strip_prefix("--") {
            let (spelling, attached) = match long.split_once('=') {
                Some((s, v)) => (s, Some(v.to_string())),
                None => (long, None),
            };
            // A declared flag written `--color=auto` takes that value and nothing more.
            return match active.iter().find(|o| o.long.iter().any(|l| l == spelling)) {
                Some(o) if o.value => took(take_value(o, attached, rest, run)),
                Some(o) => took(set_flag(o, run)),
                None => undeclared(attached.is_some()),
            };
        }
        let chars: Vec<char> = text[1..].chars().collect();
        let mut read = took(0);
        for (j, c) in chars.iter().enumerate() {
            match active.iter().find(|o| o.short.contains(c)) {
                Some(o) if o.value => {
                    let attached: String = chars[j + 1..].iter().collect();
                    let attached = match attached.strip_prefix('=') {
                        Some(v) if p.short_equals => v.to_string(),
                        _ => attached,
                    };
                    read.taken = take_value(o, (!attached.is_empty()).then_some(attached), rest, run);
                    return read;
                }
                Some(o) => _ = set_flag(o, run),
                // The rest of the cluster may be this letter's value.
                None if j + 1 < chars.len() => {
                    run.doubt = true;
                    read.undeclared = true;
                }
                None => read.undeclared |= !rest.is_empty(),
            }
        }
        read
    }

    /// After a word that could not be placed, every part it may have set
    /// reads Unknown: flags not given, unfilled operands, and every list.
    /// The level's own options count only while they still parse, before
    /// its subcommand word.
    fn mark_unsure(&self, globals: &[&Opt], own_active: bool, sub_reached: bool, run: &mut Run) {
        for slot in &self.slots {
            match slot.kind {
                SlotKind::Subcommand if !sub_reached => run.subcommands.push(TriVal::Unknown),
                SlotKind::Subcommand => {}
                _ => unsure_part(run, &slot.name),
            }
        }
        let own = if own_active { &self.options[..] } else { &[][..] };
        for opt in own.iter().chain(globals.iter().copied()) {
            unsure_part(run, &opt.name);
        }
    }
}

impl Parser {
    fn is_option(&self, text: &str, at_start: bool) -> bool {
        match self.name {
            ParserName::Find => (text.starts_with('-') && text.len() > 1) || is_find_operator(text),
            _ => {
                (text.starts_with('-') && text.len() > 1)
                    || (at_start && self.leading_number.is_some() && is_number(text, true))
            }
        }
    }
}

/// Offers an operand word to the slots before the subcommand position.
fn place(pre: &[Slot], next: &mut usize, word: &Word, run: &mut Run) -> Placed {
    while *next < pre.len() {
        let slot = &pre[*next];
        *next += 1;
        if given(run, &slot.group) {
            continue;
        }
        if let Some(m) = &slot.matches {
            match word {
                Word::Known(w) if !m.is_match(w) => continue,
                Word::Unknown { .. } => {
                    run.unsure = true;
                    return Placed::Unsure;
                }
                Word::Known(_) => {}
            }
        }
        run.parts.insert(slot.name.clone(), value_of(word));
        return Placed::Taken;
    }
    Placed::Subcommand
}

fn post_slot(post: &[Slot], slot: &Slot) -> bool {
    post.iter().any(|s| std::ptr::eq(s, slot))
}

/// Assigns operand words to slots: in order, except that one-word operands
/// after a list take theirs from the end. False when a word is left over.
fn assign(slots: &[Slot], words: Vec<Word>, run: &mut Run) -> bool {
    let slots: Vec<&Slot> = slots.iter().filter(|s| !given(run, &s.group)).collect();
    let n = words.len();
    let mut front = 0;
    for (k, slot) in slots.iter().enumerate() {
        match slot.kind {
            SlotKind::One if front < n => {
                if let (Some(m), Word::Known(w)) = (&slot.matches, &words[front])
                    && !m.is_match(w)
                {
                    continue;
                }
                run.parts.insert(slot.name.clone(), value_of(&words[front]));
                front += 1;
            }
            SlotKind::List => {
                let later = &slots[k + 1..];
                let end = match &slot.matches {
                    Some(m) => {
                        let mut end = front;
                        while end < n && matches!(&words[end], Word::Known(w) if m.is_match(w)) {
                            end += 1;
                        }
                        end
                    }
                    None if later.iter().any(|s| s.kind == SlotKind::List) => n,
                    None => n.saturating_sub(later.len()).max(front),
                };
                let elements = words[front..end].iter().map(value_of).collect();
                run.parts.insert(slot.name.clone(), list(elements, true));
                front = end;
            }
            _ => {}
        }
    }
    front == n
}

fn take_value(opt: &Opt, attached: Option<String>, rest: &[Word], run: &mut Run) -> usize {
    match (attached, rest.first()) {
        (Some(v), _) => {
            push_value(run, &opt.name, Word::Known(v));
            0
        }
        (None, Some(Word::Unknown { splits: true })) => {
            run.unsure = true;
            1
        }
        (None, Some(w)) => {
            push_value(run, &opt.name, w.clone());
            1
        }
        (None, None) => 0,
    }
}

/// Under doubt, the flag word may be another option's value: given reads Unknown.
fn set_flag(opt: &Opt, run: &mut Run) -> usize {
    let part = run.parts.entry(opt.name.clone()).or_insert(TriVal::Bool(false));
    *part = match (run.doubt, &part) {
        (false, _) | (true, TriVal::Bool(true)) => TriVal::Bool(true),
        (true, _) => TriVal::Unknown,
    };
    0
}

/// Under doubt, the value may not be one: the list may hold more than it shows.
fn push_value(run: &mut Run, name: &str, word: Word) {
    if let Some(TriVal::List { elements, exhaustive }) = run.parts.get_mut(name) {
        if run.doubt {
            *exhaustive = false;
        } else {
            elements.push(value_of(&word));
        }
    }
}

fn given(run: &Run, group: &[String]) -> bool {
    group.iter().any(|g| matches!(run.parts.get(g), Some(TriVal::List { elements, .. }) if !elements.is_empty()))
}

fn unsure_part(run: &mut Run, name: &str) {
    match run.parts.get_mut(name) {
        Some(TriVal::List { exhaustive, .. }) => *exhaustive = false,
        Some(v @ (TriVal::Null | TriVal::Bool(false))) => *v = TriVal::Unknown,
        _ => {}
    }
}

fn is_number(text: &str, plus: bool) -> bool {
    let digits = match text.strip_prefix('-') {
        Some(d) => d,
        None if plus => match text.strip_prefix('+') {
            Some(d) => d,
            None => return false,
        },
        None => return false,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

fn is_find_operator(text: &str) -> bool {
    matches!(text, "(" | ")" | "!" | "," | "-a" | "-o" | "-not" | "-and" | "-or")
}

fn value_of(word: &Word) -> TriVal {
    match word {
        Word::Known(w) => TriVal::String(w.clone()),
        Word::Unknown { .. } => TriVal::Unknown,
    }
}

fn list(elements: Vec<TriVal>, exhaustive: bool) -> TriVal {
    TriVal::List { elements, exhaustive }
}

// --- Marks ---

impl Run<'_> {
    fn finish(mut self) -> Parse {
        for level in &self.levels {
            for (name, default) in &level.defaults {
                apply_default(self.parts.get_mut(name), default);
            }
        }

        let exhaustive = !self.unsure && !self.open;
        let mut reads = Gather::new(exhaustive);
        let mut writes = Gather::new(exhaustive);
        let mut cwd_values = Vec::new();
        let mut has_cwd = false;
        for level in &self.levels {
            for name in &level.marks.read {
                reads.add(self.parts.get(name));
            }
            for name in &level.marks.write {
                writes.add(self.parts.get(name));
            }
            for name in &level.marks.cwd {
                has_cwd = true;
                cwd_values.extend(self.parts.get(name).cloned());
            }
        }
        let cwd = if has_cwd { compose_cwd(&cwd_values) } else { TriVal::Null };
        match &cwd {
            TriVal::String(_) => reads.elements.push(TriVal::String(".".into())),
            TriVal::Unknown => reads.elements.push(TriVal::Unknown),
            _ => {}
        }

        Parse {
            subcommands: list(self.subcommands, true),
            args: self.parts,
            cwd,
            reads: list(reads.elements, reads.exhaustive),
            writes: list(writes.elements, writes.exhaustive),
        }
    }
}

/// A default stands in for an absent argument; next to a non-exhaustive
/// empty list it may stand in, so the list stays non-exhaustive.
fn apply_default(part: Option<&mut TriVal>, default: &[String]) {
    let words = || default.iter().map(|w| TriVal::String(w.clone())).collect::<Vec<_>>();
    match part {
        Some(v @ TriVal::Null) => {
            if let Some(first) = default.first() {
                *v = TriVal::String(first.clone());
            }
        }
        Some(TriVal::List { elements, .. }) if elements.is_empty() => elements.extend(words()),
        _ => {}
    }
}

/// The directory that cwd-marked values lead to, each relative to the one
/// before; Null when none was given.
fn compose_cwd(values: &[TriVal]) -> TriVal {
    let mut dir: Option<String> = None;
    let mut step = |d: &str| {
        dir = Some(match &dir {
            Some(prev) if !d.starts_with('/') => format!("{prev}/{d}"),
            _ => d.to_string(),
        });
    };
    for value in values {
        match value {
            TriVal::String(d) => step(d),
            TriVal::List { elements, exhaustive: true } => {
                for e in elements {
                    match e {
                        TriVal::String(d) => step(d),
                        _ => return TriVal::Unknown,
                    }
                }
            }
            TriVal::Null => {}
            _ => return TriVal::Unknown,
        }
    }
    dir.map_or(TriVal::Null, TriVal::String)
}

struct Gather {
    elements: Vec<TriVal>,
    exhaustive: bool,
}

impl Gather {
    fn new(exhaustive: bool) -> Gather {
        Gather { elements: Vec::new(), exhaustive }
    }

    fn add(&mut self, part: Option<&TriVal>) {
        match part {
            Some(TriVal::List { elements, exhaustive }) => {
                self.elements.extend(elements.iter().cloned());
                self.exhaustive &= exhaustive;
            }
            Some(v @ (TriVal::String(_) | TriVal::Unknown)) => self.elements.push(v.clone()),
            _ => {}
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::config::parse_config;
    use crate::policy::parse::word;

    fn grammars(toml: &str) -> Result<Grammars, String> {
        let config = parse_config(toml)?;
        Grammars::compile([&config.commands])
    }

    fn parse(toml: &str, line: &str) -> Parse {
        let g = grammars(toml).unwrap_or_else(|e| panic!("{e}"));
        let mut words = line.split_whitespace().map(word);
        let name = match words.next() {
            Some(Word::Known(n)) => n,
            _ => panic!("no name in {line}"),
        };
        g.parse(&name, &words.collect::<Vec<_>>())
    }

    fn s(v: &str) -> TriVal {
        TriVal::String(v.into())
    }

    fn strings(vals: &[&str]) -> TriVal {
        list(vals.iter().map(|v| s(v)).collect(), true)
    }

    fn open(vals: &[&str]) -> TriVal {
        list(vals.iter().map(|v| s(v)).collect(), false)
    }

    /// `(line, part, value)` rows over one grammar.
    fn check(toml: &str, rows: &[(&str, &str, TriVal)]) {
        for (line, part, want) in rows {
            let got = parse(toml, line).args.get(*part).cloned().unwrap_or(TriVal::Unknown);
            assert_eq!(&got, want, "{line}: {part}");
        }
    }

    const SORT: &str = r#"
        [commands.sort]
        parser = "gnu"
        arguments = ["files...", "--output|-o:", "--reverse|-r", "--numeric-sort|-n"]
    "#;

    #[test]
    fn the_gnu_parser_reads_options_anywhere_until_double_dash() {
        check(
            SORT,
            &[
                ("sort -o out a", "output", strings(&["out"])),
                ("sort -oout a", "output", strings(&["out"])),
                ("sort --output=out a", "output", strings(&["out"])),
                ("sort a -o out b", "files", strings(&["a", "b"])),
                ("sort -rno out a", "output", strings(&["out"])),
                ("sort -rno out a", "numeric_sort", TriVal::Bool(true)),
                ("sort --reverse=x a", "reverse", TriVal::Bool(true)),
                ("sort --reverse=x a", "files", strings(&["a"])),
                ("sort -- -o a", "files", strings(&["-o", "a"])),
                ("sort -- -o a", "output", strings(&[])),
                ("sort - a", "files", strings(&["-", "a"])),
            ],
        );
    }

    #[test]
    fn an_option_matches_only_its_whole_spelling() {
        check(
            SORT,
            &[
                ("sort --out x a", "output", strings(&[])),
                ("sort --out x a", "files", TriVal::Unknown),
                ("sort --rev", "reverse", TriVal::Bool(false)),
            ],
        );
    }

    #[test]
    fn the_posix_parser_stops_options_at_the_first_operand() {
        let toml = r#"
            [commands.p]
            parser = "posix"
            arguments = ["files...", "--force|-f"]
        "#;
        check(toml, &[("p -f a -f", "files", strings(&["a", "-f"])), ("p a -f", "force", TriVal::Bool(false))]);
    }

    #[test]
    fn parser_settings_copy_one_tool() {
        let head = r#"
            [commands.head]
            parser = { name = "gnu", leading_number = "lines" }
            arguments = ["files...", "--lines|-n:"]
        "#;
        let log = r#"
            [commands.log]
            parser = { name = "gnu", number = "max_count" }
            arguments = ["files...", "--max-count|-n:"]
        "#;
        let pflag = r#"
            [commands.k]
            parser = { name = "gnu", short_equals = true }
            arguments = ["files...", "--expr|-e:"]
        "#;
        check(head, &[("head -5 a", "lines", strings(&["5"])), ("head +5 a", "lines", strings(&["+5"]))]);
        check(head, &[("head a -5", "files", strings(&["a"])), ("head a -5", "lines", strings(&[]))]);
        check(log, &[("log a -5", "max_count", strings(&["5"]))]);
        check(pflag, &[("k -e=x", "expr", strings(&["x"]))]);
        check(SORT, &[("sort -o=x", "output", strings(&["=x"]))]);
    }

    #[test]
    fn the_find_parser_reads_single_dash_words_and_operators() {
        let toml = r#"
            [commands.find]
            parser = "find"
            arguments = ["files...", "-delete", "-name:", "-newer:", "follow=-L"]
            argument.files.default = ["."]
            marks.read = ["files", "newer"]
        "#;
        check(
            toml,
            &[
                ("find a b -name x", "files", strings(&["a", "b"])),
                ("find ( -name x -o ! -newer y ) -delete", "delete", TriVal::Bool(true)),
                ("find ( -name x -o -newer y )", "newer", strings(&["y"])),
                ("find -name x", "files", strings(&["."])),
                ("find -L a -name x", "files", strings(&["a"])),
                ("find . -exec rm {} ;", "files", TriVal::Unknown),
                ("find . -exec rm {} ;", "delete", TriVal::Bool(false)),
                ("find . -exec rm -delete ;", "delete", TriVal::Unknown),
            ],
        );
        assert_eq!(parse(toml, "find a -newer b").reads, strings(&["a", "b"]));
    }

    const CP: &str = r#"
        [commands.cp]
        parser = "gnu"
        arguments = ["files...", ["dest", "--target-directory|-t:"]]
        marks.read = ["files"]
        marks.write = ["dest", "target_directory"]
    "#;

    #[test]
    fn a_group_option_takes_the_place_of_its_operand() {
        check(
            CP,
            &[
                ("cp a b c", "files", strings(&["a", "b"])),
                ("cp a b c", "dest", s("c")),
                ("cp -t d a b", "files", strings(&["a", "b"])),
                ("cp -t d a b", "dest", TriVal::Null),
                ("cp a b -t d", "files", strings(&["a", "b"])),
            ],
        );
        let p = parse(CP, "cp -t/tmp/x a");
        assert_eq!((p.reads, p.writes), (strings(&["a"]), strings(&["/tmp/x"])));
    }

    const UNIQ: &str = r#"
        [commands.u]
        parser = "gnu"
        arguments = ["input", "output", "--force|-f", "--skip|-s:"]
        marks.read = ["input"]
        marks.write = ["output"]
    "#;

    /// A word the grammar cannot place leaves the operands Unknown; options
    /// stay known unless a word could hide one.
    #[test]
    fn an_extra_operand_leaves_options_known() {
        check(
            UNIQ,
            &[
                ("u a b", "input", s("a")),
                ("u a b c", "input", TriVal::Unknown),
                ("u a b c", "force", TriVal::Bool(false)),
                ("u -f a b c", "force", TriVal::Bool(true)),
                ("u a b c", "skip", strings(&[])),
            ],
        );
        assert_eq!(parse(UNIQ, "u a b").writes, strings(&["b"]));
        assert_eq!(parse(UNIQ, "u a b c").writes, list(vec![TriVal::Unknown], false));
    }

    /// After an undeclared option, a later word may be its value: what is
    /// given from there reads Unknown, and what no word gives stays known.
    #[test]
    fn an_undeclared_option_leaves_what_follows_it_unknown() {
        check(
            UNIQ,
            &[
                ("u -x a b", "input", TriVal::Unknown),
                ("u -x a b", "force", TriVal::Bool(false)),
                ("u -x a -f b", "force", TriVal::Unknown),
                ("u -f -x a", "force", TriVal::Bool(true)),
                ("u -x a -s v", "skip", open(&[])),
                ("u -xf a", "force", TriVal::Unknown),
                ("u a b -x", "input", s("a")),
                ("u a b --x=y", "input", TriVal::Unknown),
            ],
        );
        assert_eq!(parse(UNIQ, "u a b -x").reads, strings(&["a"]));
        assert_eq!(parse(UNIQ, "u -x a b").reads, list(vec![TriVal::Unknown], false));
    }

    /// A word that cannot be known may be any option, `--` or several words.
    #[test]
    fn an_unknown_word_leaves_everything_it_can_reach_unknown() {
        check(
            UNIQ,
            &[
                ("u a $X", "force", TriVal::Unknown),
                ("u a $X", "output", TriVal::Unknown),
                ("u a -s \"$X\"", "skip", list(vec![TriVal::Unknown], true)),
                ("u a -s \"$X\"", "force", TriVal::Bool(false)),
                ("u a -s $X", "skip", open(&[])),
            ],
        );
    }

    #[test]
    fn an_operand_with_a_match_takes_only_a_matching_word() {
        let toml = r#"
            [commands.cargo]
            parser = "gnu"
            arguments = ["toolchain", "subcommand", "rest..."]
            argument.toolchain.match = "+*"
            argument.subcommand.words = ["build"]
        "#;
        check(
            toml,
            &[
                ("cargo +nightly build x", "toolchain", s("+nightly")),
                ("cargo build x", "toolchain", TriVal::Null),
                ("cargo build x", "rest", strings(&["x"])),
            ],
        );
    }

    const GIT: &str = r#"
        [commands.git]
        parser = "gnu"
        arguments = ["dir=-C:", "config=-c:", "subcommand"]
        argument.dir.default = ["."]
        argument.subcommand.words = ["add"]
        marks.cwd = ["dir"]

        [commands.git.log]
        parser = { name = "gnu", number = "max_count" }
        arguments = ["files...", "--output:", "find_copies=-C", "--max-count|-n:"]
        marks.read = ["files"]
        marks.write = ["output"]

        [commands.git.stash]
        arguments = ["subcommand", "names..."]
        argument.subcommand.words = ["list", "drop"]

        [commands.cargo]
        parser = "gnu"
        arguments = ["subcommand", "rest..."]
        argument.subcommand.words = ["test"]
        global = ["--manifest-path:"]
        marks.read = ["manifest_path"]
    "#;

    #[test]
    fn subcommand_words_choose_the_level_that_parses_the_rest() {
        let rows = [
            ("git log", strings(&["log"])),
            ("git -C x stash drop y", strings(&["stash", "drop"])),
            ("git add x", strings(&["add"])),
            ("git", strings(&[])),
            ("git frob x", list(vec![TriVal::Unknown], true)),
            ("git $X log", list(vec![TriVal::Unknown], true)),
            ("git stash $X", list(vec![s("stash"), TriVal::Unknown], true)),
        ];
        for (line, want) in rows {
            assert_eq!(parse(GIT, line).subcommands, want, "{line}");
        }
        check(
            GIT,
            &[
                ("git -C x log -C", "find_copies", TriVal::Bool(true)),
                ("git -C x log -C", "dir", strings(&["x"])),
                ("git log -C x", "dir", strings(&["."])),
                ("git log -C x", "files", strings(&["x"])),
                ("git stash drop a b", "names", strings(&["a", "b"])),
                ("git frob x", "dir", strings(&["."])),
                ("git frob x", "config", strings(&[])),
                ("git $X log", "config", open(&[])),
                ("git --frob log", "config", strings(&[])),
                ("git --frob -c x log", "config", open(&[])),
                ("cargo test --manifest-path m", "manifest_path", strings(&["m"])),
            ],
        );
    }

    #[test]
    fn marks_turn_parts_into_paths_relative_to_the_marked_directory() {
        let p = parse(GIT, "git -C a -C b log --output o f");
        assert_eq!((p.cwd, p.reads, p.writes), (s("a/b"), strings(&["f", "."]), strings(&["o"])));
        let p = parse(GIT, "git -C / log");
        assert_eq!((p.cwd, p.reads), (s("/"), strings(&["."])));
        let p = parse(GIT, "git log $X");
        assert_eq!((p.cwd, p.reads), (s("."), open(&["."])));
        let p = parse(GIT, "git frob");
        assert_eq!((p.cwd, p.reads), (s("."), open(&["."])));
        assert_eq!(parse(GIT, "git $X log").cwd, TriVal::Unknown);
    }

    #[test]
    fn a_command_finds_its_grammar_by_its_last_path_component() {
        assert_eq!(parse(GIT, "/usr/bin/git log").subcommands, strings(&["log"]));
        assert_eq!(parse(GIT, "./bin/git log").subcommands, strings(&["log"]));
        assert_eq!(parse(GIT, "gti log").subcommands, strings(&[]));
    }

    #[test]
    fn a_rule_command_declares_the_names_of_its_levels() {
        let g = grammars(GIT).unwrap();
        let path = |p: &str| p.split(' ').map(String::from).collect::<Vec<_>>();
        let names = g.declared(&path("git stash")).unwrap();
        assert!(names.contains("dir") && names.contains("names") && !names.contains("output"));
        assert!(g.declared(&path("git add")).is_ok());
        assert!(g.declared(&path("git add x")).is_err());
        assert!(g.declared(&path("git frob")).is_err());
        assert!(g.declared(&path("bench x")).is_err());
        assert!(g.declared(&path("bench")).unwrap().is_empty());
    }

    #[test]
    fn a_grammar_that_breaks_the_format_does_not_load() {
        let rows = [
            ("arguments = [\"x\"]", "needs `parser`"),
            ("parser = \"gnu\"\narguments = [\"--a|b\"]", "not an argument string"),
            ("parser = \"gnu\"\narguments = [\"x:\"]", "not an argument string"),
            ("parser = \"gnu\"\narguments = [\"force=--force\"]", "repeats the name"),
            ("parser = \"gnu\"\narguments = [\"-C:\"]", "not snake_case"),
            ("parser = \"gnu\"\narguments = [\"-newer:\"]", "single-dash word"),
            ("parser = \"gnu\"\narguments = [\"x\", \"x...\"]", "declared twice"),
            ("parser = \"gnu\"\nglobal = [\"x\"]", "`global` holds options"),
            ("parser = \"gnu\"\narguments = [[\"x\", \"--y\"]]", "a group is"),
            ("parser = \"gnu\"\narguments = [[\"x...\", \"--y:\"]]", "a group is"),
            ("parser = \"gnu\"\narguments = [\"subcommand...\"]", "takes no `...`"),
            ("parser = \"gnu\"\narguments = [\"subcommand\", \"subcommand\"]", "twice"),
            ("parser = \"gnu\"\narguments = [\"x...\", \"subcommand\"]\nargument.subcommand.words = [\"a\"]", "list operand"),
            ("parser = \"gnu\"\narguments = [\"subcommand\"]", "no words and no tables"),
            ("parser = \"gnu\"\narguments = [\"x\"]\n[commands.t.sub]\narguments = []", "without `subcommand`"),
            (
                "parser = \"gnu\"\narguments = [\"subcommand\"]\nargument.subcommand.words = [\"a\"]\n[commands.t.a]\narguments = []",
                "both in `words` and a table",
            ),
            ("parser = \"gnu\"\narguments = [\"subcommand\"]\n[commands.t.with]\narguments = []", "reserved"),
            ("parser = \"gnu\"\narguments = [\"x\"]\nargument.x.words = [\"a\"]", "only for `subcommand`"),
            ("parser = \"gnu\"\narguments = [\"--x:\"]\nargument.x.match = \"*\"", "only for an operand"),
            ("parser = \"gnu\"\narguments = [\"--x\"]\nargument.x.default = [\"a\"]", "only for an operand or an option"),
            ("parser = \"gnu\"\narguments = [\"x\"]\nargument.y.default = [\"a\"]", "names no argument"),
            ("parser = \"gnu\"\narguments = [\"x\"]\nmarks.read = [\"y\"]", "does not declare"),
            (
                "parser = \"gnu\"\narguments = [\"subcommand\"]\nglobal = [\"--g\"]\n[commands.t.a]\narguments = [\"other=--g\"]",
                "already a global option",
            ),
            ("parser = \"gnu\"\narguments = [\"subcommand\", \"x\"]\n[commands.t.a]\narguments = [\"x\"]", "declared twice along"),
        ];
        for (body, want) in rows {
            let toml = format!("[commands.t]\n{body}");
            match grammars(&toml) {
                Ok(_) => panic!("loaded:\n{toml}"),
                Err(err) => assert!(err.contains(want), "{toml}\n  error: {err}\n  wanted: {want}"),
            }
        }
    }
}
