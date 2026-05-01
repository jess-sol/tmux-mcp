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

/// Parse a command string into a flat list of all commands that will execute.
/// Each command has a `parent` reference to its wrapper (if any).
pub fn parse_command(command: &str) -> Result<Vec<CommandInfo>, ParseError> {
    let program = parse_with_brush(command)?;
    let mut commands = Vec::new();
    extract_from_program(&program, &mut commands);
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

fn extract_from_program(program: &ast::Program, out: &mut Vec<CommandInfo>) {
    for cc in &program.complete_commands {
        extract_from_compound_list(cc, out);
    }
}

fn extract_from_compound_list(list: &ast::CompoundList, out: &mut Vec<CommandInfo>) {
    for item in &list.0 {
        extract_from_and_or_list(&item.0, out);
    }
}

fn extract_from_and_or_list(and_or: &ast::AndOrList, out: &mut Vec<CommandInfo>) {
    extract_from_pipeline(&and_or.first, out);
    for additional in &and_or.additional {
        let pipeline = match additional {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => p,
        };
        extract_from_pipeline(pipeline, out);
    }
}

fn extract_from_pipeline(pipeline: &ast::Pipeline, out: &mut Vec<CommandInfo>) {
    for (i, command) in pipeline.seq.iter().enumerate() {
        let is_pipe_target = i > 0;
        extract_from_command(command, is_pipe_target, out);
    }
}

fn extract_from_command(command: &ast::Command, is_pipe_target: bool, out: &mut Vec<CommandInfo>) {
    match command {
        ast::Command::Simple(simple) => {
            extract_from_simple_command(simple, is_pipe_target, None, out);
        }
        ast::Command::Compound(compound, _) => {
            extract_from_compound_command(compound, out);
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
                    extract_from_compound_list(&subshell.list, &mut cmd_sub_commands);
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
                    extract_from_compound_list(&subshell.list, &mut cmd_sub_commands);
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
        is_pipe_target,
        redirects,
        parent,
        inner: InnerExtraction::None,
    };

    // Try wrapper extraction — produces inner commands with this as parent
    info.inner = extract_wrapper_children(&info, out);

    // Add this command to the output
    out.push(info);

    // Add command substitution commands (from args of this command or its wrapper)
    out.extend(cmd_sub_commands);
}

fn extract_from_compound_command(compound: &ast::CompoundCommand, out: &mut Vec<CommandInfo>) {
    match compound {
        ast::CompoundCommand::Subshell(s) => extract_from_compound_list(&s.list, out),
        ast::CompoundCommand::BraceGroup(g) => extract_from_compound_list(&g.list, out),
        ast::CompoundCommand::IfClause(c) => {
            extract_from_compound_list(&c.condition, out);
            extract_from_compound_list(&c.then, out);
            if let Some(elses) = &c.elses {
                for el in elses {
                    if let Some(cond) = &el.condition { extract_from_compound_list(cond, out); }
                    extract_from_compound_list(&el.body, out);
                }
            }
        }
        ast::CompoundCommand::WhileClause(c) | ast::CompoundCommand::UntilClause(c) => {
            extract_from_compound_list(&c.0, out);
            extract_from_compound_list(&c.1.list, out);
        }
        ast::CompoundCommand::ForClause(c) => extract_from_compound_list(&c.body.list, out),
        ast::CompoundCommand::CaseClause(c) => {
            for item in &c.cases {
                if let Some(cmd) = &item.cmd { extract_from_compound_list(cmd, out); }
            }
        }
        ast::CompoundCommand::ArithmeticForClause(c) => extract_from_compound_list(&c.body.list, out),
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

// --- Wrapper inner extraction ---
//
// Wrappers produce both themselves (added by caller) and their inner commands.
// Inner commands get `parent` set to the wrapper and inherit context
// (effective_user, effective_host).
//
// Returns the extraction result:
// - Transparent: inner extracted, wrapper effects fully captured (user/host).
//   Engine skips this wrapper during evaluation.
// - Evaluated: inner extracted, but wrapper modifies uncaptured state (env vars).
//   Both wrapper and inner are evaluated.
// - None: not a wrapper, or extraction failed.

fn extract_wrapper_children(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> InnerExtraction {
    let extracted = match wrapper.name.as_str() {
        // Noop wrappers — change nothing security-relevant
        "command" | "builtin" | "nohup" => extract_transparent(wrapper, out),
        "nice" => extract_skip_flags(wrapper, &["-n"], out),
        "timeout" => extract_timeout(wrapper, out),
        "strace" => extract_skip_flags(wrapper, &["-e", "-s", "-o", "-p"], out),
        "watch" => extract_skip_flags(wrapper, &["-n"], out),
        // Context-changing — modify user/host (captured via effective_user/host)
        "sudo" => extract_sudo(wrapper, out),
        "su" => extract_su(wrapper, out),
        "doas" => extract_doas(wrapper, out),
        "ssh" => extract_ssh(wrapper, out),
        "podman" | "docker" => extract_subcommand_exec(wrapper, out),
        "kubectl" => extract_kubectl(wrapper, out),
        // Shell eval — inner command fully extracted
        "sh" | "bash" | "zsh" | "dash" => extract_string_eval(wrapper, out),
        // Delegating — inner command extracted with args_complete=false
        "find" => extract_find(wrapper, out),
        "xargs" => extract_xargs(wrapper, out),
        // Environment — modifies env vars we don't capture
        "env" => {
            return if extract_env(wrapper, out) {
                InnerExtraction::Evaluated
            } else {
                InnerExtraction::None
            };
        }
        _ => return InnerExtraction::None,
    };
    if extracted { InnerExtraction::Transparent } else { InnerExtraction::None }
}

/// Create an inner CommandInfo and add it (and any recursive inner commands) to out.
/// Returns true if an inner command was produced.
fn push_inner(
    parent: &CommandInfo,
    inner_args: &[String],
    user: Effective,
    host: Effective,
    args_complete: bool,
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
        is_pipe_target: false,
        redirects: Vec::new(), // inner commands from wrappers don't carry redirects
        parent: Some(parent_arc),
        inner: InnerExtraction::None,
    };

    // Recursively extract if inner is also a wrapper
    inner.inner = extract_wrapper_children(&inner, out);
    out.push(inner);
    true
}

fn skip_flags(args: &[String], flags_with_arg: &[&str]) -> usize {
    let mut i = 0;
    while i < args.len() {
        if args[i].starts_with('-') {
            if flags_with_arg.iter().any(|f| args[i] == *f) { i += 2; } else { i += 1; }
        } else {
            break;
        }
    }
    i
}

fn extract_transparent(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    if wrapper.args.is_empty() { return false; }
    push_inner(wrapper, &wrapper.args, wrapper.effective_user.clone(), wrapper.effective_host.clone(), true, out)
}

fn extract_skip_flags(wrapper: &CommandInfo, flags_with_arg: &[&str], out: &mut Vec<CommandInfo>) -> bool {
    let start = skip_flags(&wrapper.args, flags_with_arg);
    if start >= wrapper.args.len() { return false; }
    push_inner(wrapper, &wrapper.args[start..], wrapper.effective_user.clone(), wrapper.effective_host.clone(), true, out)
}

fn extract_timeout(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let mut i = 0;
    while i < wrapper.args.len() {
        if wrapper.args[i].starts_with('-') {
            if ["-s", "--signal", "-k", "--kill-after"].contains(&wrapper.args[i].as_str()) { i += 2; } else { i += 1; }
        } else {
            i += 1; // skip duration
            break;
        }
    }
    if i >= wrapper.args.len() { return false; }
    push_inner(wrapper, &wrapper.args[i..], wrapper.effective_user.clone(), wrapper.effective_host.clone(), true, out)
}

fn extract_sudo(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let mut effective_user = Effective::Known("root".to_string());
    let mut i = 0;
    while i < wrapper.args.len() {
        match wrapper.args[i].as_str() {
            "-u" => {
                if i + 1 < wrapper.args.len() {
                    let u = &wrapper.args[i + 1];
                    effective_user = if word_has_expansion(u) { Effective::Unknown } else { Effective::Known(u.clone()) };
                    i += 2;
                } else { i += 1; }
            }
            "-i" | "-s" => { i += 1; }
            a if a.starts_with('-') => {
                if ["-C", "-g", "-r", "-t", "-U", "-D"].contains(&a) { i += 2; } else { i += 1; }
            }
            _ => break,
        }
    }
    if i >= wrapper.args.len() { return false; }
    push_inner(wrapper, &wrapper.args[i..], effective_user, wrapper.effective_host.clone(), true, out)
}

fn extract_su(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let mut effective_user = Effective::Known("root".to_string());
    let mut command_str: Option<String> = None;
    let mut i = 0;
    while i < wrapper.args.len() {
        match wrapper.args[i].as_str() {
            "-c" => {
                if i + 1 < wrapper.args.len() { command_str = Some(wrapper.args[i + 1].clone()); }
                i += 2;
            }
            "-" | "-l" | "--login" => { i += 1; }
            a if a.starts_with('-') => { i += 1; }
            _ => {
                let u = &wrapper.args[i];
                effective_user = if word_has_expansion(u) { Effective::Unknown } else { Effective::Known(u.clone()) };
                i += 1;
            }
        }
    }
    if let Some(cmd) = command_str {
        let cmd_str = brush_parser::unquote_str(&cmd);
        if let Ok(mut inner_cmds) = parse_command(&cmd_str) {
            let parent_arc = Arc::new(wrapper.clone());
            for c in &mut inner_cmds {
                c.effective_user = effective_user.clone();
                c.parent = Some(parent_arc.clone());
            }
            out.extend(inner_cmds);
            return true;
        }
    }
    false
}

fn extract_doas(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let mut effective_user = Effective::Known("root".to_string());
    let mut i = 0;
    while i < wrapper.args.len() {
        match wrapper.args[i].as_str() {
            "-u" => {
                if i + 1 < wrapper.args.len() {
                    let u = &wrapper.args[i + 1];
                    effective_user = if word_has_expansion(u) { Effective::Unknown } else { Effective::Known(u.clone()) };
                    i += 2;
                } else { i += 1; }
            }
            a if a.starts_with('-') => { i += 1; }
            _ => break,
        }
    }
    if i >= wrapper.args.len() { return false; }
    push_inner(wrapper, &wrapper.args[i..], effective_user, wrapper.effective_host.clone(), true, out)
}

fn extract_env(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let mut i = 0;
    while i < wrapper.args.len() {
        let a = &wrapper.args[i];
        if a.starts_with('-') {
            if a == "-u" || a == "-S" { i += 2; } else { i += 1; }
        } else if a.contains('=') {
            i += 1;
        } else {
            break;
        }
    }
    if i >= wrapper.args.len() { return false; }
    push_inner(wrapper, &wrapper.args[i..], wrapper.effective_user.clone(), wrapper.effective_host.clone(), true, out)
}

fn extract_string_eval(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let c_pos = match wrapper.args.iter().position(|a| a == "-c") {
        Some(p) => p,
        None => return false,
    };
    if c_pos + 1 >= wrapper.args.len() { return false; }
    let cmd_str = brush_parser::unquote_str(&wrapper.args[c_pos + 1]);
    if word_has_expansion(&cmd_str) { return false; }
    if let Ok(mut inner_cmds) = parse_command(&cmd_str) {
        let parent_arc = Arc::new(wrapper.clone());
        for c in &mut inner_cmds {
            c.parent = Some(parent_arc.clone());
        }
        out.extend(inner_cmds);
        return true;
    }
    false
}

fn extract_find(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let exec_flags = ["-exec", "-execdir", "-ok", "-okdir"];
    let mut found = false;
    let mut i = 0;
    while i < wrapper.args.len() {
        if exec_flags.contains(&wrapper.args[i].as_str()) {
            let start = i + 1;
            let mut end = start;
            while end < wrapper.args.len() && wrapper.args[end] != ";" && wrapper.args[end] != "+" {
                end += 1;
            }
            if start < end {
                push_inner(wrapper, &wrapper.args[start..end], wrapper.effective_user.clone(), wrapper.effective_host.clone(), false, out);
                found = true;
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    found
}

fn extract_xargs(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let start = skip_flags(&wrapper.args, &["-d", "-I", "-L", "-n", "-P", "-s", "-E"]);
    if start >= wrapper.args.len() { return false; }
    push_inner(wrapper, &wrapper.args[start..], wrapper.effective_user.clone(), wrapper.effective_host.clone(), false, out)
}

fn extract_ssh(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    let mut effective_host = Effective::Unchanged;
    let mut i = 0;
    while i < wrapper.args.len() {
        let a = &wrapper.args[i];
        if a.starts_with('-') {
            if ["-b","-c","-D","-E","-e","-F","-I","-i","-J","-L","-l","-m","-O","-o","-p","-Q","-R","-S","-W","-w"].contains(&a.as_str()) {
                i += 2;
            } else { i += 1; }
        } else if matches!(effective_host, Effective::Unchanged) {
            let h = &wrapper.args[i];
            effective_host = if word_has_expansion(h) {
                Effective::Unknown
            } else {
                let host = h.find('@').map(|p| &h[p + 1..]).unwrap_or(h.as_str());
                Effective::Known(host.to_string())
            };
            i += 1;
        } else {
            break;
        }
    }
    if i >= wrapper.args.len() { return false; }
    push_inner(wrapper, &wrapper.args[i..], wrapper.effective_user.clone(), effective_host, true, out)
}

fn extract_subcommand_exec(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    if wrapper.args.first().map(|s| s.as_str()) != Some("exec") { return false; }
    let args = &wrapper.args[1..];
    let mut effective_host = Effective::Unchanged;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') {
            if ["-e","--env","-u","--user","-w","--workdir"].contains(&a.as_str()) { i += 2; } else { i += 1; }
        } else if matches!(effective_host, Effective::Unchanged) {
            effective_host = if word_has_expansion(a) { Effective::Unknown } else { Effective::Known(a.clone()) };
            i += 1;
        } else {
            break;
        }
    }
    if i >= args.len() { return false; }
    push_inner(wrapper, &args[i..], wrapper.effective_user.clone(), effective_host, true, out)
}

fn extract_kubectl(wrapper: &CommandInfo, out: &mut Vec<CommandInfo>) -> bool {
    if wrapper.args.first().map(|s| s.as_str()) != Some("exec") { return false; }
    let args = &wrapper.args[1..];
    let mut effective_host = Effective::Unchanged;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" { i += 1; break; }
        else if a.starts_with('-') {
            if ["-n","-c","--namespace","--container"].contains(&a.as_str()) { i += 2; } else { i += 1; }
        } else if matches!(effective_host, Effective::Unchanged) {
            effective_host = if word_has_expansion(a) { Effective::Unknown } else { Effective::Known(a.clone()) };
            i += 1;
        } else { i += 1; }
    }
    if i >= args.len() { return false; }
    push_inner(wrapper, &args[i..], wrapper.effective_user.clone(), effective_host, true, out)
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
