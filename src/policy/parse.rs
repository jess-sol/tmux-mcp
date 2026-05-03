//! Command parsing: brush-parser → flat list of CommandInfo with parent references.
//!
//! Parses a command string into a flat list of `CommandInfo` nodes. Each node
//! represents a command that will execute (wrapper or leaf). Wrappers have their
//! inner commands extracted with a `parent` reference back to the wrapper.
//! Pipelines, logical operators, and command substitutions produce additional
//! independent entries.

use std::sync::Arc;

use brush_parser::ast;
use brush_parser::word::WordPiece;

// --- Types ---

/// Information about a single command that will execute.
/// Every wrapper and every leaf gets its own entry in the flat list.
/// Parent chain is navigable via `parent` references.
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// Command name (always a literal string — structural check denies expansions).
    pub name: String,
    /// Literal arguments we can see.
    pub args: Vec<String>,
    /// False if dynamic/unknown args possible (expansions, stdin args, etc.).
    pub args_complete: bool,
    /// User context — propagated down from user-modifying wrappers.
    pub effective_user: Effective,
    /// Host context — propagated down from host-modifying wrappers.
    pub effective_host: Effective,
    /// CWD context — set by wrappers with `changes_cwd = true`.
    pub effective_cwd: Effective,
    /// True if this command receives piped stdin in a pipeline.
    pub is_pipe_target: bool,
    /// Shell redirects attached to this command.
    pub redirects: Vec<RedirectInfo>,
    /// Immediate parent wrapper (if any). Walk parent.parent for the full chain.
    pub parent: Option<Arc<CommandInfo>>,
    /// Whether this command is a wrapper with extracted inner commands.
    pub inner: InnerExtraction,
}

/// A shell redirect (>, >>, <, etc.) attached to a command.
#[derive(Debug, Clone)]
pub struct RedirectInfo {
    /// The target path/word.
    pub target: String,
    /// True for write redirects (>, >>, &>, >|). False for read (<, <<, <<<).
    pub is_write: bool,
    /// True if target contains expansions ($(), ${}, etc.).
    pub has_expansion: bool,
}

impl CommandInfo {
    #[cfg(test)]
    pub(crate) fn simple(name: &str) -> Self {
        Self {
            name: name.to_string(),
            args: Vec::new(),
            args_complete: true,
            effective_user: Effective::Unchanged,
            effective_host: Effective::Unchanged,
            effective_cwd: Effective::Unchanged,
            is_pipe_target: false,
            redirects: Vec::new(),
            parent: None,
            inner: InnerExtraction::None,
        }
    }
}

/// Three-state value tracking for wrapper-extracted context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effective {
    /// No wrapper modified this value — resolves to pane context value in CEL.
    Unchanged,
    /// Extracted concrete value from wrapper args.
    Known(String),
    /// Wrapper used but value unknowable (expansion in arg) — resolves to null in CEL.
    Unknown,
}

/// Whether a wrapper's inner commands were extracted, and how the engine should
/// treat the wrapper during evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InnerExtraction {
    /// Not a wrapper, or extraction failed. Evaluate normally.
    None,
    /// Inner commands extracted and wrapper is redundant — all effects (user/host)
    /// are captured on inner commands. Engine skips this wrapper during evaluation.
    Transparent,
    /// Inner commands extracted, but wrapper modifies uncaptured state (e.g. env vars).
    /// Both wrapper and inner commands are evaluated.
    Evaluated,
}

/// Error returned when brush-parser can't parse the command.
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
}

// --- Public API ---

/// Parse a command string using the default builtin wrapper registry.
pub fn parse_command(command: &str) -> Result<Vec<CommandInfo>, ParseError> {
    parse_command_with_registry(command, default_registry())
}

/// Parse a command string with a specific wrapper registry.
pub fn parse_command_with_registry(
    command: &str,
    registry: &WrapperRegistry,
) -> Result<Vec<CommandInfo>, ParseError> {
    let program = parse_with_brush(command)?;
    let mut commands = Vec::new();
    extract_from_program(&program, registry, &mut commands);
    Ok(commands)
}

// --- brush-parser integration ---

fn parse_with_brush(command: &str) -> Result<ast::Program, ParseError> {
    use std::io::BufReader;
    let reader = BufReader::new(command.as_bytes());
    let mut parser = brush_parser::Parser::new(
        reader,
        &brush_parser::ParserOptions::default(),
        &brush_parser::SourceInfo::default(),
    );
    parser
        .parse_program()
        .map_err(|e| ParseError { message: format!("{}", e) })
}

// --- AST walking ---

fn extract_from_program(program: &ast::Program, reg: &WrapperRegistry, out: &mut Vec<CommandInfo>) {
    for cc in &program.complete_commands {
        extract_from_compound_list(cc, reg, out);
    }
}

fn extract_from_compound_list(list: &ast::CompoundList, reg: &WrapperRegistry, out: &mut Vec<CommandInfo>) {
    for item in &list.0 {
        extract_from_and_or_list(&item.0, reg, out);
    }
}

fn extract_from_and_or_list(and_or: &ast::AndOrList, reg: &WrapperRegistry, out: &mut Vec<CommandInfo>) {
    extract_from_pipeline(&and_or.first, reg, out);
    for additional in &and_or.additional {
        let pipeline = match additional {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => p,
        };
        extract_from_pipeline(pipeline, reg, out);
    }
}

fn extract_from_pipeline(pipeline: &ast::Pipeline, reg: &WrapperRegistry, out: &mut Vec<CommandInfo>) {
    for (i, command) in pipeline.seq.iter().enumerate() {
        let is_pipe_target = i > 0;
        extract_from_command(command, is_pipe_target, reg, out);
    }
}

fn extract_from_command(command: &ast::Command, is_pipe_target: bool, reg: &WrapperRegistry, out: &mut Vec<CommandInfo>) {
    match command {
        ast::Command::Simple(simple) => {
            extract_from_simple_command(simple, is_pipe_target, None, reg, out);
        }
        ast::Command::Compound(compound, _) => {
            extract_from_compound_command(compound, reg, out);
        }
        ast::Command::Function(_) | ast::Command::ExtendedTest(_) => {}
    }
}

/// Extract commands from a SimpleCommand. If it's a wrapper, produce entries
/// for both the wrapper and the inner command(s), with parent references.
fn extract_from_simple_command(
    simple: &ast::SimpleCommand,
    is_pipe_target: bool,
    parent: Option<Arc<CommandInfo>>,
    reg: &WrapperRegistry,
    out: &mut Vec<CommandInfo>,
) {
    let name_word = match &simple.word_or_name {
        Some(w) => w,
        None => return,
    };

    let mut args = Vec::new();
    let mut args_complete = !word_has_expansion(&name_word.value);
    let mut cmd_sub_commands = Vec::new();

    // Collect args from suffix
    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    if word_has_expansion(&word.value) {
                        args_complete = false;
                    }
                    args.push(word.value.clone());
                    extract_command_subs_from_word(&word.value, &mut cmd_sub_commands);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    extract_from_compound_list(&subshell.list, reg, &mut cmd_sub_commands);
                    args_complete = false;
                }
                _ => {}
            }
        }
    }

    // Check prefix for process substitutions
    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    extract_from_compound_list(&subshell.list, reg, &mut cmd_sub_commands);
                    args_complete = false;
                }
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    if word_has_expansion(&word.value) {
                        args_complete = false;
                    }
                    extract_command_subs_from_word(&word.value, &mut cmd_sub_commands);
                }
                _ => {}
            }
        }
    }

    // Inherit effective_user/host from parent
    let effective_user = parent.as_ref()
        .map(|p| p.effective_user.clone())
        .unwrap_or(Effective::Unchanged);
    let effective_host = parent.as_ref()
        .map(|p| p.effective_host.clone())
        .unwrap_or(Effective::Unchanged);

    // Extract redirects from prefix and suffix
    let mut redirects = Vec::new();
    if let Some(prefix) = &simple.prefix {
        extract_redirects(&prefix.0, &mut redirects);
    }
    if let Some(suffix) = &simple.suffix {
        extract_redirects(&suffix.0, &mut redirects);
    }

    let mut info = CommandInfo {
        name: name_word.value.clone(),
        args,
        args_complete,
        effective_user,
        effective_host,
        effective_cwd: Effective::Unchanged,
        is_pipe_target,
        redirects,
        parent,
        inner: InnerExtraction::None,
    };

    // Try wrapper extraction — produces inner commands with this as parent
    info.inner = extract_wrapper_children(&info, reg, out);

    // Add this command to the output
    out.push(info);

    // Add command substitution commands (from args of this command or its wrapper)
    out.extend(cmd_sub_commands);
}

fn extract_from_compound_command(compound: &ast::CompoundCommand, reg: &WrapperRegistry, out: &mut Vec<CommandInfo>) {
    match compound {
        ast::CompoundCommand::Subshell(s) => extract_from_compound_list(&s.list, reg, out),
        ast::CompoundCommand::BraceGroup(g) => extract_from_compound_list(&g.list, reg, out),
        ast::CompoundCommand::IfClause(c) => {
            extract_from_compound_list(&c.condition, reg, out);
            extract_from_compound_list(&c.then, reg, out);
            if let Some(elses) = &c.elses {
                for el in elses {
                    if let Some(cond) = &el.condition { extract_from_compound_list(cond, reg, out); }
                    extract_from_compound_list(&el.body, reg, out);
                }
            }
        }
        ast::CompoundCommand::WhileClause(c) | ast::CompoundCommand::UntilClause(c) => {
            extract_from_compound_list(&c.0, reg, out);
            extract_from_compound_list(&c.1.list, reg, out);
        }
        ast::CompoundCommand::ForClause(c) => extract_from_compound_list(&c.body.list, reg, out),
        ast::CompoundCommand::CaseClause(c) => {
            for item in &c.cases {
                if let Some(cmd) = &item.cmd { extract_from_compound_list(cmd, reg, out); }
            }
        }
        ast::CompoundCommand::ArithmeticForClause(c) => extract_from_compound_list(&c.body.list, reg, out),
        ast::CompoundCommand::Arithmetic(_) => {}
    }
}

// --- Redirect extraction ---

/// Extract redirect info from prefix/suffix items.
fn extract_redirects(items: &[ast::CommandPrefixOrSuffixItem], out: &mut Vec<RedirectInfo>) {
    for item in items {
        if let ast::CommandPrefixOrSuffixItem::IoRedirect(redirect) = item {
            match redirect {
                ast::IoRedirect::File(_, kind, target) => {
                    let is_write = matches!(
                        kind,
                        ast::IoFileRedirectKind::Write
                            | ast::IoFileRedirectKind::Append
                            | ast::IoFileRedirectKind::Clobber
                            | ast::IoFileRedirectKind::ReadAndWrite
                    );
                    if let ast::IoFileRedirectTarget::Filename(word) = target {
                        out.push(RedirectInfo {
                            target: word.value.clone(),
                            is_write,
                            has_expansion: word_has_expansion(&word.value),
                        });
                    }
                }
                ast::IoRedirect::OutputAndError(word, _) => {
                    out.push(RedirectInfo {
                        target: word.value.clone(),
                        is_write: true,
                        has_expansion: word_has_expansion(&word.value),
                    });
                }
                ast::IoRedirect::HereDocument(_, _) | ast::IoRedirect::HereString(_, _) => {
                    // Here-docs/strings are input, not file targets — skip
                }
            }
        }
    }
}

// --- Word analysis ---

/// Check if a word value contains shell expansions ($VAR, $(cmd), etc.).
pub(crate) fn word_has_expansion(word_value: &str) -> bool {
    let options = brush_parser::ParserOptions::default();
    match brush_parser::word::parse(word_value, &options) {
        Ok(pieces) => pieces.iter().any(|p| match &p.piece {
            WordPiece::ParameterExpansion(_)
            | WordPiece::CommandSubstitution(_)
            | WordPiece::BackquotedCommandSubstitution(_)
            | WordPiece::ArithmeticExpression(_) => true,
            WordPiece::DoubleQuotedSequence(inner) => {
                inner.iter().any(|ip| matches!(
                    &ip.piece,
                    WordPiece::ParameterExpansion(_)
                    | WordPiece::CommandSubstitution(_)
                    | WordPiece::BackquotedCommandSubstitution(_)
                    | WordPiece::ArithmeticExpression(_)
                ))
            }
            _ => false,
        }),
        Err(_) => true,
    }
}

fn extract_command_subs_from_word(word_value: &str, out: &mut Vec<CommandInfo>) {
    let options = brush_parser::ParserOptions::default();
    if let Ok(pieces) = brush_parser::word::parse(word_value, &options) {
        for p in &pieces {
            extract_command_subs_from_piece(&p.piece, out);
        }
    }
}

fn extract_command_subs_from_piece(piece: &WordPiece, out: &mut Vec<CommandInfo>) {
    match piece {
        WordPiece::CommandSubstitution(cmd_str)
        | WordPiece::BackquotedCommandSubstitution(cmd_str) => {
            if let Ok(inner) = parse_command(cmd_str) {
                out.extend(inner);
            }
        }
        WordPiece::DoubleQuotedSequence(inner) => {
            for ip in inner {
                extract_command_subs_from_piece(&ip.piece, out);
            }
        }
        _ => {}
    }
}

// --- Wrapper extraction engine ---
//
// Declarative wrapper rules (defined in TOML) drive extraction.
// Each rule has a CEL `when` expression to match, a `getopt` field for
// arg parsing, and CEL expressions for `inner`, `capture_user`, `capture_host`.
//
// The engine iterates the registry (first match wins), evaluates the `when`
// expression, runs getopt if configured, evaluates `inner` to produce inner
// commands, and applies capture expressions for effective_user/host.

use std::collections::HashMap;
use cel_parser::ast as cel;
use super::args::ArgSpec;
use super::config::TaggedWrapper;

/// A compiled wrapper rule, ready for matching and extraction.
pub struct CompiledWrapper {
    pub name: String,
    pub when: cel::Expr,
    pub getopt: Option<ArgSpec>,
    pub inner: cel::Expr,
    pub capture_user: Option<cel::Expr>,
    pub capture_host: Option<cel::Expr>,
    pub skip_wrapper: bool,
    pub args_complete: bool,
    /// When true, inner commands have uncertain cwd (effective_cwd = Unknown).
    pub changes_cwd: bool,
}

/// Ordered list of compiled wrapper rules. First match wins.
pub struct WrapperRegistry {
    pub wrappers: Vec<CompiledWrapper>,
}

impl WrapperRegistry {
    pub fn empty() -> Self {
        Self { wrappers: Vec::new() }
    }
}

fn parse_cel(expr: &str) -> Result<cel::Expr, String> {
    let parser = cel_parser::Parser::new();
    parser.parse(expr)
        .map(|e| e.expr)
        .map_err(|e| format!("{:?}", e))
}

/// Compile wrapper configs into a registry. Logs and skips invalid entries.
pub fn compile_wrappers(tagged: &[TaggedWrapper]) -> WrapperRegistry {
    let mut wrappers = Vec::new();

    for tw in tagged {
        let w = &tw.config;
        let when = match parse_cel(&w.when) {
            Ok(expr) => expr,
            Err(e) => {
                tracing::warn!("Wrapper '{}': invalid `when` CEL: {}", w.name, e);
                continue;
            }
        };
        let inner = match parse_cel(&w.inner) {
            Ok(expr) => expr,
            Err(e) => {
                tracing::warn!("Wrapper '{}': invalid `inner` CEL: {}", w.name, e);
                continue;
            }
        };
        let capture_user = match &w.capture_user {
            Some(expr) => match parse_cel(expr) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!("Wrapper '{}': invalid `capture_user` CEL: {}", w.name, e);
                    None
                }
            },
            None => None,
        };
        let capture_host = match &w.capture_host {
            Some(expr) => match parse_cel(expr) {
                Ok(e) => Some(e),
                Err(e) => {
                    tracing::warn!("Wrapper '{}': invalid `capture_host` CEL: {}", w.name, e);
                    None
                }
            },
            None => None,
        };
        let getopt = if let Some(g) = &w.getopt {
            Some(g.to_arg_spec(super::args::ArgStyle::Posix))
        } else if let Some(g) = &w.getopt_gnu {
            Some(g.to_arg_spec(super::args::ArgStyle::Gnu))
        } else {
            None
        };

        wrappers.push(CompiledWrapper {
            name: w.name.clone(),
            when,
            getopt,
            inner,
            capture_user,
            capture_host,
            skip_wrapper: w.skip_wrapper,
            args_complete: w.args_complete,
            changes_cwd: w.changes_cwd,
        });
    }

    WrapperRegistry { wrappers }
}

/// Default wrapper registry from built-in TOML. Used by `parse_command()`.
fn default_registry() -> &'static WrapperRegistry {
    use std::sync::LazyLock;
    static DEFAULT: LazyLock<WrapperRegistry> = LazyLock::new(|| {
        let config = super::config::parse_config(
            include_str!("builtin_rules.toml")
        ).expect("built-in TOML invalid");
        let tagged: Vec<TaggedWrapper> = config.wrappers.into_iter().enumerate()
            .map(|(i, w)| TaggedWrapper {
                config: w,
                source: super::config::RuleSource::Builtin,
                source_index: i,
            })
            .collect();
        compile_wrappers(&tagged)
    });
    &DEFAULT
}

fn extract_wrapper_children(
    wrapper: &CommandInfo,
    registry: &WrapperRegistry,
    out: &mut Vec<CommandInfo>,
) -> InnerExtraction {
    use super::rules::{self, TriVal, TriBool};

    for cw in &registry.wrappers {
        // Build evaluation context: command.name, command.args
        let mut cmd_map = HashMap::new();
        cmd_map.insert("name".into(), TriVal::String(wrapper.name.clone()));
        let args: Vec<TriVal> = wrapper.args.iter()
            .map(|a| TriVal::String(a.clone()))
            .collect();
        cmd_map.insert("args".into(), TriVal::List {
            elements: args.clone(),
            exhaustive: wrapper.args_complete,
        });

        // Pre-run getopt if configured, inject result as command.getopt
        if let Some(ref spec) = cw.getopt {
            let args_trivals: Vec<TriVal> = wrapper.args.iter()
                .map(|a| TriVal::String(a.clone()))
                .collect();
            let parsed = super::args::parse_args(&args_trivals, spec, wrapper.args_complete);
            cmd_map.insert("getopt".into(), parsed_args_to_trival(&parsed));
        }

        let mut ctx = HashMap::new();
        ctx.insert("command".into(), TriVal::Map(cmd_map));

        // Evaluate `when` expression
        let when_result = rules::eval_expr(&cw.when, &ctx);
        if when_result.is_truthy() != TriBool::True {
            continue;
        }

        // Evaluate `inner` expression
        let inner_result = rules::eval_expr(&cw.inner, &ctx);

        // Evaluate capture expressions
        let effective_user = match &cw.capture_user {
            Some(expr) => trival_to_effective(rules::eval_expr(expr, &ctx)),
            None => wrapper.effective_user.clone(),
        };
        let effective_host = match &cw.capture_host {
            Some(expr) => trival_to_effective(rules::eval_expr(expr, &ctx)),
            None => wrapper.effective_host.clone(),
        };
        let effective_cwd = if cw.changes_cwd {
            Effective::Unknown
        } else {
            wrapper.effective_cwd.clone()
        };

        // Interpret inner result type
        let extracted = match inner_result {
            // List[String] → single inner command
            TriVal::List { ref elements, .. } if !elements.is_empty() => {
                // Check if it's a list-of-lists (List[List[String]]) for find -exec
                if matches!(&elements[0], TriVal::List { .. }) {
                    // Multiple inner commands
                    let mut found = false;
                    for block in elements {
                        if let TriVal::List { elements: block_args, .. } = block {
                            let str_args: Vec<String> = block_args.iter().filter_map(|a| {
                                if let TriVal::String(s) = a { Some(s.clone()) } else { None }
                            }).collect();
                            if !str_args.is_empty() {
                                push_inner(wrapper, &str_args, effective_user.clone(),
                                    effective_host.clone(), effective_cwd.clone(),
                                    cw.args_complete, registry, out);
                                found = true;
                            }
                        }
                    }
                    found
                } else {
                    // Single inner command
                    let str_args: Vec<String> = elements.iter().filter_map(|a| {
                        if let TriVal::String(s) = a { Some(s.clone()) } else { None }
                    }).collect();
                    if str_args.is_empty() {
                        false
                    } else {
                        push_inner(wrapper, &str_args, effective_user, effective_host,
                            effective_cwd, cw.args_complete, registry, out)
                    }
                }
            }
            // String → reparse as command string (su -c, bash -c)
            TriVal::String(ref cmd_str) => {
                let unquoted = brush_parser::unquote_str(cmd_str);
                if word_has_expansion(&unquoted) {
                    false
                } else if let Ok(mut inner_cmds) = parse_command_with_registry(&unquoted, registry) {
                    let parent_arc = Arc::new(wrapper.clone());
                    for c in &mut inner_cmds {
                        c.effective_user = effective_user.clone();
                        c.effective_host = effective_host.clone();
                        c.effective_cwd = effective_cwd.clone();
                        c.parent = Some(parent_arc.clone());
                    }
                    let found = !inner_cmds.is_empty();
                    out.extend(inner_cmds);
                    found
                } else {
                    false
                }
            }
            _ => false,
        };

        if !extracted {
            return InnerExtraction::None;
        }
        return if cw.skip_wrapper {
            InnerExtraction::Transparent
        } else {
            InnerExtraction::Evaluated
        };
    }

    InnerExtraction::None
}

/// Convert ParsedArgs into a TriVal::Map for use in CEL expressions.
pub(super) fn parsed_args_to_trival(parsed: &super::args::ParsedArgs) -> super::rules::TriVal {
    use super::rules::TriVal;
    let mut map = HashMap::new();
    map.insert("operands".into(), TriVal::List {
        elements: parsed.operands.clone(),
        exhaustive: parsed.exhaustive,
    });
    map.insert("flags".into(), TriVal::Map(parsed.flags.clone()));
    map.insert("exhaustive".into(), TriVal::Bool(parsed.exhaustive));

    TriVal::Map(map)
}

fn trival_to_effective(val: super::rules::TriVal) -> Effective {
    use super::rules::TriVal;
    match val {
        TriVal::String(s) => Effective::Known(s),
        TriVal::Unknown => Effective::Unknown,
        _ => Effective::Unchanged,
    }
}

/// Create an inner CommandInfo and add it (and any recursive inner commands) to out.
fn push_inner(
    parent: &CommandInfo,
    inner_args: &[String],
    user: Effective,
    host: Effective,
    cwd: Effective,
    args_complete: bool,
    registry: &WrapperRegistry,
    out: &mut Vec<CommandInfo>,
) -> bool {
    let (cmd_name, cmd_args) = match inner_args.split_first() {
        Some((name, args)) => (name.clone(), args.to_vec()),
        None => return false,
    };

    let parent_arc = Arc::new(parent.clone());
    let mut inner = CommandInfo {
        name: cmd_name,
        args: cmd_args,
        args_complete,
        effective_user: user,
        effective_host: host,
        effective_cwd: cwd,
        is_pipe_target: false,
        redirects: Vec::new(),
        parent: Some(parent_arc),
        inner: InnerExtraction::None,
    };

    inner.inner = extract_wrapper_children(&inner, registry, out);
    out.push(inner);
    true
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    // --- Dependency contract: brush-parser ---

    fn parse_program(input: &str) -> ast::Program {
        parse_with_brush(input).unwrap_or_else(|e| panic!("Failed to parse '{}': {}", input, e.message))
    }

    #[test]
    fn brush_parses_simple_command() {
        let prog = parse_program("ls -la");
        let pipeline = &prog.complete_commands[0].0[0].0.first;
        assert_eq!(pipeline.seq.len(), 1);
        match &pipeline.seq[0] {
            ast::Command::Simple(s) => assert_eq!(s.word_or_name.as_ref().unwrap().value, "ls"),
            _ => panic!("expected SimpleCommand"),
        }
    }

    #[test]
    fn brush_parses_pipeline() {
        let prog = parse_program("ls | grep foo");
        assert_eq!(prog.complete_commands[0].0[0].0.first.seq.len(), 2);
    }

    #[test]
    fn brush_parses_and_or_list() {
        let and_or = &parse_program("a && b || c").complete_commands[0].0[0].0;
        assert_eq!(and_or.additional.len(), 2);
    }

    #[test]
    fn brush_parses_command_substitution_in_word() {
        let pieces = brush_parser::word::parse("$(date)", &brush_parser::ParserOptions::default()).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::CommandSubstitution(_))));
    }

    #[test]
    fn brush_parses_subshell() {
        let pipeline = &parse_program("(ls)").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::Subshell(_), _)));
    }

    #[test]
    fn brush_parses_brace_group() {
        let pipeline = &parse_program("{ ls; }").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::BraceGroup(_), _)));
    }

    #[test]
    fn brush_parses_if_clause() {
        let pipeline = &parse_program("if true; then echo yes; fi").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::IfClause(_), _)));
    }

    #[test]
    fn brush_parses_for_clause() {
        let pipeline = &parse_program("for x in a b; do echo $x; done").complete_commands[0].0[0].0.first;
        assert!(matches!(&pipeline.seq[0], ast::Command::Compound(ast::CompoundCommand::ForClause(_), _)));
    }

    #[test]
    fn brush_parses_quoted_string_not_split() {
        let pipeline = &parse_program("echo 'hello | world'").complete_commands[0].0[0].0.first;
        assert_eq!(pipeline.seq.len(), 1);
    }

    #[test]
    fn brush_rejects_unclosed_quote() {
        assert!(parse_with_brush("echo 'hello").is_err());
    }

    #[test]
    fn brush_detects_expansion_in_word() {
        let pieces = brush_parser::word::parse("$VAR", &brush_parser::ParserOptions::default()).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::ParameterExpansion(_))));
    }

    #[test]
    fn brush_detects_command_sub_in_word() {
        let pieces = brush_parser::word::parse("$(cmd)", &brush_parser::ParserOptions::default()).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::CommandSubstitution(_))));
    }

    // --- Helper to find command by name in flat list ---

    fn find_cmd<'a>(cmds: &'a [CommandInfo], name: &str) -> &'a CommandInfo {
        cmds.iter().find(|c| c.name == name)
            .unwrap_or_else(|| panic!("command '{}' not found in {:?}", name, cmds.iter().map(|c| &c.name).collect::<Vec<_>>()))
    }

    // --- Simple commands ---

    #[test]
    fn simple_command_extracts_name_and_args() {
        let cmds = parse_command("ls -la /tmp").unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].args, vec!["-la", "/tmp"]);
        assert!(cmds[0].args_complete);
        assert!(cmds[0].parent.is_none());
    }

    #[test]
    fn command_with_no_args() {
        let cmds = parse_command("pwd").unwrap();
        assert_eq!(cmds[0].name, "pwd");
        assert!(cmds[0].args.is_empty());
    }

    // --- Pipelines ---

    #[test]
    fn pipeline_extracts_each_segment() {
        let cmds = parse_command("ls | grep foo").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "grep"));
    }

    #[test]
    fn pipeline_marks_pipe_target() {
        let cmds = parse_command("ls | grep foo").unwrap();
        let ls = find_cmd(&cmds, "ls");
        let grep = find_cmd(&cmds, "grep");
        assert!(!ls.is_pipe_target);
        assert!(grep.is_pipe_target);
    }

    #[test]
    fn pipeline_three_segments() {
        let cmds = parse_command("ls | grep foo | wc -l").unwrap();
        assert!(cmds.iter().any(|c| c.name == "wc"));
    }

    #[test]
    fn quoted_pipe_is_not_pipeline() {
        let cmds = parse_command("echo 'hello | world'").unwrap();
        assert_eq!(cmds.iter().filter(|c| c.parent.is_none()).count(), 1);
    }

    // --- Logical operators ---

    #[test]
    fn and_list_extracts_both() {
        let cmds = parse_command("ls && echo done").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "echo"));
    }

    #[test]
    fn or_list_extracts_both() {
        let cmds = parse_command("ls || echo failed").unwrap();
        assert_eq!(cmds.iter().filter(|c| c.parent.is_none()).count(), 2);
    }

    // --- Command substitution ---

    #[test]
    fn arg_with_command_substitution() {
        let cmds = parse_command("echo $(date)").unwrap();
        assert!(cmds.iter().any(|c| c.name == "echo"));
        assert!(cmds.iter().any(|c| c.name == "date"));
    }

    #[test]
    fn command_substitution_sets_args_incomplete() {
        let cmds = parse_command("rm $(find . -name '*.tmp')").unwrap();
        let rm = find_cmd(&cmds, "rm");
        assert!(!rm.args_complete);
    }

    #[test]
    fn expansion_in_arg_marks_incomplete() {
        let cmds = parse_command("rm -rf $DIR").unwrap();
        assert!(!cmds[0].args_complete);
    }

    #[test]
    fn literal_args_are_complete() {
        let cmds = parse_command("rm -rf /tmp").unwrap();
        assert!(cmds[0].args_complete);
    }

    // --- Compound commands ---

    #[test]
    fn subshell_extracts_inner() {
        let cmds = parse_command("(ls && pwd)").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "pwd"));
    }

    #[test]
    fn brace_group_extracts_inner() {
        let cmds = parse_command("{ ls; pwd; }").unwrap();
        assert!(cmds.iter().any(|c| c.name == "ls"));
        assert!(cmds.iter().any(|c| c.name == "pwd"));
    }

    // --- Wrappers: parent is preserved, inner extracted ---

    #[test]
    fn command_builtin_parent() {
        let cmds = parse_command("command ls -la").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.args, vec!["-la"]);
        assert_eq!(ls.parent.as_ref().unwrap().name, "command");
    }

    #[test]
    fn sudo_parent_with_root() {
        let cmds = parse_command("sudo rm foo").unwrap();
        let rm = find_cmd(&cmds, "rm");
        assert_eq!(rm.parent.as_ref().unwrap().name, "sudo");
        assert_eq!(rm.effective_user, Effective::Known("root".to_string()));
    }

    #[test]
    fn sudo_u_parent_with_user() {
        let cmds = parse_command("sudo -u postgres psql").unwrap();
        let psql = find_cmd(&cmds, "psql");
        assert_eq!(psql.parent.as_ref().unwrap().name, "sudo");
        assert_eq!(psql.effective_user, Effective::Known("postgres".to_string()));
    }

    #[test]
    fn no_wrapper_no_parent() {
        let cmds = parse_command("ls").unwrap();
        assert!(cmds[0].parent.is_none());
        assert_eq!(cmds[0].effective_user, Effective::Unchanged);
    }

    #[test]
    fn env_parent() {
        let cmds = parse_command("env FOO=bar cargo test").unwrap();
        let cargo = find_cmd(&cmds, "cargo");
        assert_eq!(cargo.parent.as_ref().unwrap().name, "env");
        assert_eq!(cargo.args, vec!["test"]);
    }

    #[test]
    fn env_multiple_vars() {
        let cmds = parse_command("env A=1 B=2 make").unwrap();
        let make = find_cmd(&cmds, "make");
        assert_eq!(make.parent.as_ref().unwrap().name, "env");
    }

    #[test]
    fn xargs_parent_with_incomplete_args() {
        let cmds = parse_command("xargs cat").unwrap();
        let cat = find_cmd(&cmds, "cat");
        assert_eq!(cat.parent.as_ref().unwrap().name, "xargs");
        assert!(!cat.args_complete);
    }

    #[test]
    fn xargs_inner_has_known_and_unknown_args() {
        let cmds = parse_command("xargs rm -v").unwrap();
        let rm = find_cmd(&cmds, "rm");
        assert_eq!(rm.args, vec!["-v"]);
        assert!(!rm.args_complete);
    }

    #[test]
    fn ssh_parent_with_host() {
        let cmds = parse_command("ssh prod-server ls").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.parent.as_ref().unwrap().name, "ssh");
        assert_eq!(ls.effective_host, Effective::Known("prod-server".to_string()));
    }

    #[test]
    fn ssh_user_at_host() {
        let cmds = parse_command("ssh user@10.0.0.1 ls").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.effective_host, Effective::Known("10.0.0.1".to_string()));
    }

    #[test]
    fn ssh_with_flags() {
        let cmds = parse_command("ssh -i key.pem user@host ls -la").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.parent.as_ref().unwrap().name, "ssh");
        assert_eq!(ls.effective_host, Effective::Known("host".to_string()));
    }

    #[test]
    fn kubectl_exec_pod() {
        let cmds = parse_command("kubectl exec mypod -- ls").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.parent.as_ref().unwrap().name, "kubectl");
        assert_eq!(ls.effective_host, Effective::Known("mypod".to_string()));
    }

    #[test]
    fn kubectl_exec_with_namespace() {
        let cmds = parse_command("kubectl exec -n prod mypod -- ls").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.effective_host, Effective::Known("mypod".to_string()));
    }

    #[test]
    fn podman_exec_container() {
        let cmds = parse_command("podman exec myapp ls").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.parent.as_ref().unwrap().name, "podman");
        assert_eq!(ls.effective_host, Effective::Known("myapp".to_string()));
    }

    #[test]
    fn docker_exec_container() {
        let cmds = parse_command("docker exec mycontainer ls").unwrap();
        let ls = find_cmd(&cmds, "ls");
        assert_eq!(ls.effective_host, Effective::Known("mycontainer".to_string()));
    }

    // --- Chained wrappers ---

    #[test]
    fn sudo_env_cargo_chain() {
        let cmds = parse_command("sudo env FOO=bar cargo test").unwrap();
        // All three appear in flat list
        assert!(cmds.iter().any(|c| c.name == "sudo"));
        assert!(cmds.iter().any(|c| c.name == "env"));
        assert!(cmds.iter().any(|c| c.name == "cargo"));
        // cargo's parent is env, env's parent is sudo
        let cargo = find_cmd(&cmds, "cargo");
        let env_parent = cargo.parent.as_ref().unwrap();
        assert_eq!(env_parent.name, "env");
        let sudo_parent = env_parent.parent.as_ref().unwrap();
        assert_eq!(sudo_parent.name, "sudo");
        // effective_user propagates
        assert_eq!(cargo.effective_user, Effective::Known("root".to_string()));
    }

    // --- String eval ---

    #[test]
    fn sh_c_literal_reparses() {
        let cmds = parse_command("sh -c 'echo hello'").unwrap();
        assert!(cmds.iter().any(|c| c.name == "sh"));
        let echo = find_cmd(&cmds, "echo");
        assert_eq!(echo.parent.as_ref().unwrap().name, "sh");
    }

    // --- find -exec ---

    #[test]
    fn find_exec_extracts_command() {
        let cmds = parse_command("find . -exec grep foo {} ;").unwrap();
        assert!(cmds.iter().any(|c| c.name == "find"));
        let grep = find_cmd(&cmds, "grep");
        assert_eq!(grep.parent.as_ref().unwrap().name, "find");
        assert!(!grep.args_complete);
    }

    #[test]
    fn find_exec_extracts_args() {
        let cmds = parse_command("find . -exec grep -r TODO {} ;").unwrap();
        let grep = find_cmd(&cmds, "grep");
        assert_eq!(grep.args, vec!["-r", "TODO", "{}"]);
    }

    #[test]
    fn find_multiple_exec_blocks() {
        // \; is needed — bare ; is a shell command separator
        let cmds = parse_command("find . -exec chmod 644 {} \\; -exec chown root {} \\;").unwrap();
        let names: Vec<&str> = cmds.iter().map(|c| c.name.as_str()).collect();
        assert!(cmds.iter().any(|c| c.name == "chmod"), "chmod not found in {:?}", names);
        assert!(cmds.iter().any(|c| c.name == "chown"), "chown not found in {:?}", names);
        let chmod = find_cmd(&cmds, "chmod");
        assert!(chmod.args.contains(&"644".to_string()));
        let chown = find_cmd(&cmds, "chown");
        assert!(chown.args.contains(&"root".to_string()));
    }

    #[test]
    fn find_exec_with_plus_terminator() {
        let cmds = parse_command("find . -exec rm {} +").unwrap();
        let rm = find_cmd(&cmds, "rm");
        assert_eq!(rm.args, vec!["{}"]);
        assert!(!rm.args_complete);
    }

    #[test]
    fn find_execdir_extracts() {
        let cmds = parse_command("find /tmp -execdir rm {} ;").unwrap();
        let rm = find_cmd(&cmds, "rm");
        assert_eq!(rm.parent.as_ref().unwrap().name, "find");
    }

    #[test]
    fn find_exec_sh_c_recursive() {
        // -exec sh -c '...' should recursively extract via shell-eval wrapper
        let cmds = parse_command("find . -exec sh -c 'echo hello' ;").unwrap();
        assert!(cmds.iter().any(|c| c.name == "sh"));
        assert!(cmds.iter().any(|c| c.name == "echo"));
    }

    #[test]
    fn find_exec_with_predicates() {
        // Predicates before -exec shouldn't interfere
        let cmds = parse_command("find /var -name '*.log' -mtime +30 -exec rm {} ;").unwrap();
        let rm = find_cmd(&cmds, "rm");
        assert_eq!(rm.parent.as_ref().unwrap().name, "find");
    }

    #[test]
    fn find_without_exec_no_extraction() {
        let cmds = parse_command("find . -name '*.rs' -type f").unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "find");
        assert_eq!(cmds[0].inner, InnerExtraction::None);
    }

    // --- Flag-skip wrappers ---

    #[test]
    fn nice_extracts_inner() {
        let cmds = parse_command("nice -n 10 cargo build").unwrap();
        let cargo = find_cmd(&cmds, "cargo");
        assert_eq!(cargo.parent.as_ref().unwrap().name, "nice");
        assert_eq!(cargo.args, vec!["build"]);
    }

    #[test]
    fn nohup_extracts_inner() {
        let cmds = parse_command("nohup make").unwrap();
        let make = find_cmd(&cmds, "make");
        assert_eq!(make.parent.as_ref().unwrap().name, "nohup");
    }

    #[test]
    fn timeout_extracts_inner() {
        let cmds = parse_command("timeout 30 curl example.com").unwrap();
        let curl = find_cmd(&cmds, "curl");
        assert_eq!(curl.parent.as_ref().unwrap().name, "timeout");
    }

    // --- InnerExtraction ---

    #[test]
    fn wrapper_is_transparent() {
        let cmds = parse_command("sudo rm -rf /").unwrap();
        assert_eq!(find_cmd(&cmds, "sudo").inner, InnerExtraction::Transparent);
    }

    #[test]
    fn leaf_is_none() {
        let cmds = parse_command("sudo rm -rf /").unwrap();
        assert_eq!(find_cmd(&cmds, "rm").inner, InnerExtraction::None);
    }

    #[test]
    fn non_wrapper_is_none() {
        let cmds = parse_command("ls -la").unwrap();
        assert_eq!(cmds[0].inner, InnerExtraction::None);
    }

    #[test]
    fn wrapper_no_inner_is_none() {
        let cmds = parse_command("sudo -i").unwrap();
        assert_eq!(find_cmd(&cmds, "sudo").inner, InnerExtraction::None);
    }

    #[test]
    fn env_is_evaluated() {
        let cmds = parse_command("env FOO=bar cargo test").unwrap();
        assert_eq!(find_cmd(&cmds, "env").inner, InnerExtraction::Evaluated);
    }

    #[test]
    fn chained_wrappers_inner_types() {
        let cmds = parse_command("env FOO=bar sudo cargo test").unwrap();
        assert_eq!(find_cmd(&cmds, "env").inner, InnerExtraction::Evaluated);
        assert_eq!(find_cmd(&cmds, "sudo").inner, InnerExtraction::Transparent);
    }

    // --- Parse failures ---

    #[test]
    fn empty_string_parses_as_empty() {
        assert!(parse_command("").unwrap().is_empty());
    }

    #[test]
    fn unclosed_quote_fails() {
        assert!(parse_command("echo 'hello").is_err());
    }

    // --- time is a keyword ---

    #[test]
    fn time_is_keyword() {
        let cmds = parse_command("time make -j4").unwrap();
        assert_eq!(cmds[0].name, "make"); // brush-parser handles time as keyword
    }
}
