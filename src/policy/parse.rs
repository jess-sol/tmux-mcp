//! Command parsing: brush-parser → CommandInfo tree extraction.
//!
//! Parses a command string into a recursive tree of `CommandInfo` nodes,
//! handling pipelines, logical operators, command substitutions, compound
//! commands, and wrapper unwrapping (sudo, env, ssh, xargs, etc.).

use brush_parser::ast;
use brush_parser::word::WordPiece;

// --- Types ---

/// Extracted information about a single command in a pipeline/tree.
#[derive(Debug, Clone)]
pub struct CommandInfo {
    /// Command name (always a literal string — structural check denies expansions as names).
    pub name: String,
    /// Literal arguments we can see.
    pub args: Vec<String>,
    /// False if dynamic/unknown args possible (expansions, stdin args, etc.).
    pub args_complete: bool,
    /// User context after wrapper processing.
    pub effective_user: Effective,
    /// Host context after wrapper processing.
    pub effective_host: Effective,
    /// True if this command receives piped stdin in a pipeline.
    pub is_pipe_target: bool,
    /// Inner commands: from pipelines, &&/||, $(), sh -c, wrappers.
    pub inner: Vec<CommandInfo>,
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

/// Error returned when brush-parser can't parse the command.
#[derive(Debug)]
pub struct ParseError {
    pub message: String,
}

// --- Public API ---

/// Parse a command string into a list of all CommandInfo nodes.
///
/// Returns the top-level commands (pipeline segments, &&/|| branches).
/// Each CommandInfo may have `inner` children from substitutions, wrappers, etc.
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
    for complete_cmd in &program.complete_commands {
        extract_from_compound_list(complete_cmd, out);
    }
}

fn extract_from_compound_list(list: &ast::CompoundList, out: &mut Vec<CommandInfo>) {
    for item in &list.0 {
        extract_from_and_or_list(&item.0, out);
    }
}

fn extract_from_and_or_list(and_or: &ast::AndOrList, out: &mut Vec<CommandInfo>) {
    extract_from_pipeline(&and_or.first, false, out);
    for additional in &and_or.additional {
        let pipeline = match additional {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => p,
        };
        extract_from_pipeline(pipeline, false, out);
    }
}

fn extract_from_pipeline(
    pipeline: &ast::Pipeline,
    _parent_is_pipe_target: bool,
    out: &mut Vec<CommandInfo>,
) {
    for (i, command) in pipeline.seq.iter().enumerate() {
        let is_pipe_target = i > 0;
        extract_from_command(command, is_pipe_target, out);
    }
}

fn extract_from_command(command: &ast::Command, is_pipe_target: bool, out: &mut Vec<CommandInfo>) {
    match command {
        ast::Command::Simple(simple) => {
            if let Some(info) = extract_from_simple_command(simple, is_pipe_target) {
                out.push(info);
            }
        }
        ast::Command::Compound(compound, _redirects) => {
            extract_from_compound_command(compound, out);
        }
        ast::Command::Function(_) | ast::Command::ExtendedTest(_) => {
            // Function definitions and [[ ]] tests don't execute external commands
        }
    }
}

fn extract_from_simple_command(
    simple: &ast::SimpleCommand,
    is_pipe_target: bool,
) -> Option<CommandInfo> {
    let name_word = simple.word_or_name.as_ref()?;
    let name = &name_word.value;

    // Check if command name contains expansions (will be caught by structural check)
    let name_has_expansion = word_has_expansion(&name_word.value);

    // Collect args from suffix
    let mut args = Vec::new();
    let mut args_complete = !name_has_expansion;
    let mut inner = Vec::new();

    if let Some(suffix) = &simple.suffix {
        for item in &suffix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    let word_expansion = word_has_expansion(&word.value);
                    if word_expansion {
                        args_complete = false;
                    }
                    args.push(word.value.clone());

                    // Check for command substitutions inside args
                    extract_command_subs_from_word(&word.value, &mut inner);
                }
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    extract_from_compound_list(&subshell.list, &mut inner);
                    args_complete = false;
                }
                _ => {}
            }
        }
    }

    // Check prefix for assignments with expansions and process substitutions
    if let Some(prefix) = &simple.prefix {
        for item in &prefix.0 {
            match item {
                ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, subshell) => {
                    extract_from_compound_list(&subshell.list, &mut inner);
                    args_complete = false;
                }
                ast::CommandPrefixOrSuffixItem::Word(word) => {
                    if word_has_expansion(&word.value) {
                        args_complete = false;
                    }
                    extract_command_subs_from_word(&word.value, &mut inner);
                }
                _ => {}
            }
        }
    }

    let mut info = CommandInfo {
        name: name.clone(),
        args,
        args_complete,
        effective_user: Effective::Unchanged,
        effective_host: Effective::Unchanged,
        is_pipe_target,
        inner,
    };

    // Try wrapper unwrapping
    try_unwrap_wrapper(&mut info);

    Some(info)
}

fn extract_from_compound_command(compound: &ast::CompoundCommand, out: &mut Vec<CommandInfo>) {
    match compound {
        ast::CompoundCommand::Subshell(subshell) => {
            extract_from_compound_list(&subshell.list, out);
        }
        ast::CompoundCommand::BraceGroup(group) => {
            extract_from_compound_list(&group.list, out);
        }
        ast::CompoundCommand::IfClause(if_cmd) => {
            extract_from_compound_list(&if_cmd.condition, out);
            extract_from_compound_list(&if_cmd.then, out);
            if let Some(elses) = &if_cmd.elses {
                for else_clause in elses {
                    if let Some(cond) = &else_clause.condition {
                        extract_from_compound_list(cond, out);
                    }
                    extract_from_compound_list(&else_clause.body, out);
                }
            }
        }
        ast::CompoundCommand::WhileClause(cmd) | ast::CompoundCommand::UntilClause(cmd) => {
            extract_from_compound_list(&cmd.0, out);
            extract_from_compound_list(&cmd.1.list, out);
        }
        ast::CompoundCommand::ForClause(for_cmd) => {
            extract_from_compound_list(&for_cmd.body.list, out);
        }
        ast::CompoundCommand::CaseClause(case_cmd) => {
            for case_item in &case_cmd.cases {
                if let Some(cmd) = &case_item.cmd {
                    extract_from_compound_list(cmd, out);
                }
            }
        }
        ast::CompoundCommand::ArithmeticForClause(for_cmd) => {
            extract_from_compound_list(&for_cmd.body.list, out);
        }
        ast::CompoundCommand::Arithmetic(_) => {
            // (( expr )) — no external commands
        }
    }
}

// --- Word analysis ---

/// Check if a word value contains shell expansions ($VAR, $(cmd), etc.).
fn word_has_expansion(word_value: &str) -> bool {
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
        Err(_) => true, // can't parse word → treat as having expansion (conservative)
    }
}

/// Extract command substitutions from a word value and parse their contents.
fn extract_command_subs_from_word(word_value: &str, out: &mut Vec<CommandInfo>) {
    let options = brush_parser::ParserOptions::default();
    let pieces = match brush_parser::word::parse(word_value, &options) {
        Ok(p) => p,
        Err(_) => return,
    };

    for piece_ws in &pieces {
        extract_command_subs_from_piece(&piece_ws.piece, out);
    }
}

fn extract_command_subs_from_piece(piece: &WordPiece, out: &mut Vec<CommandInfo>) {
    match piece {
        WordPiece::CommandSubstitution(cmd_str) => {
            if let Ok(inner_cmds) = parse_command(cmd_str) {
                out.extend(inner_cmds);
            }
        }
        WordPiece::BackquotedCommandSubstitution(cmd_str) => {
            if let Ok(inner_cmds) = parse_command(cmd_str) {
                out.extend(inner_cmds);
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

// --- Wrapper unwrapping ---

/// Try to unwrap known wrapper commands, modifying the CommandInfo in place.
/// If the command is a wrapper, the inner command becomes the primary and the
/// wrapper's context modifications (effective_user, effective_host, etc.) are applied.
fn try_unwrap_wrapper(info: &mut CommandInfo) {
    match info.name.as_str() {
        // Transparent wrappers
        "command" | "builtin" | "time" => unwrap_transparent(info),

        // Flag-skip wrappers
        "nice" => unwrap_flag_skip(info, &["-n"], &["-n"]),
        "nohup" => unwrap_transparent(info), // nohup just takes the rest
        "timeout" => unwrap_timeout(info),
        "strace" => unwrap_flag_skip(info, &["-f", "-v", "-e", "-s", "-o", "-p", "-c"], &["-e", "-s", "-o", "-p"]),
        "watch" => unwrap_flag_skip(info, &["-n", "-d", "-t", "-b", "-e", "-g", "-x"], &["-n"]),

        // User-modifying wrappers
        "sudo" => unwrap_sudo(info),
        "su" => unwrap_su(info),
        "doas" => unwrap_doas(info),

        // Env-style
        "env" => unwrap_env(info),

        // String-eval wrappers
        "sh" | "bash" | "zsh" | "dash" => unwrap_string_eval(info),

        // Exec-extract (find -exec)
        "find" => unwrap_find(info),

        // Args-from-stdin
        "xargs" => unwrap_xargs(info),

        // Host-modifying wrappers
        "ssh" => unwrap_ssh(info),
        "podman" => unwrap_subcommand_exec(info, "podman"),
        "kubectl" => unwrap_kubectl(info),
        "docker" => unwrap_subcommand_exec(info, "docker"),

        _ => {} // not a wrapper
    }
}

fn unwrap_transparent(info: &mut CommandInfo) {
    if info.args.is_empty() {
        return;
    }
    info.name = info.args.remove(0);
    try_unwrap_wrapper(info); // recurse for chained wrappers
}

fn unwrap_timeout(info: &mut CommandInfo) {
    // timeout [options] duration command [args...]
    let mut i = 0;
    while i < info.args.len() {
        let arg = &info.args[i];
        if arg.starts_with('-') {
            // Skip flags like --signal, --preserve-status, etc.
            if arg == "-s" || arg == "--signal" || arg == "-k" || arg == "--kill-after" {
                i += 2; // flag + value
            } else {
                i += 1;
            }
        } else {
            // This is the duration, skip it
            i += 1;
            break;
        }
    }
    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
        }
        try_unwrap_wrapper(info);
    }
}

fn unwrap_flag_skip(info: &mut CommandInfo, flags: &[&str], flags_with_arg: &[&str]) {
    let mut i = 0;
    while i < info.args.len() {
        let arg = &info.args[i];
        if arg.starts_with('-') {
            if flags_with_arg.iter().any(|f| arg == *f) {
                i += 2; // flag + its argument
            } else if flags.iter().any(|f| arg == *f) || arg.starts_with('-') {
                i += 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
        }
        try_unwrap_wrapper(info);
    }
}

fn unwrap_sudo(info: &mut CommandInfo) {
    let mut effective_user = Effective::Known("root".to_string());
    let mut i = 0;

    while i < info.args.len() {
        let arg = &info.args[i];
        match arg.as_str() {
            "-u" => {
                if i + 1 < info.args.len() {
                    let user_arg = &info.args[i + 1];
                    if word_has_expansion(user_arg) {
                        effective_user = Effective::Unknown;
                    } else {
                        effective_user = Effective::Known(user_arg.clone());
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            "-i" | "-s" => {
                // Interactive shell or shell — could have remaining command
                i += 1;
            }
            arg if arg.starts_with('-') => {
                // Skip other flags: -E, -H, -n, -P, etc.
                // Flags that take an arg: -C, -g, -r, -t, -U, -D
                if ["-C", "-g", "-r", "-t", "-U", "-D"].contains(&arg) {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => break, // first non-flag is the command
        }
    }

    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
            info.effective_user = effective_user;
        }
        try_unwrap_wrapper(info);
    } else {
        // sudo with no command (interactive shell)
        info.effective_user = effective_user;
    }
}

fn unwrap_su(info: &mut CommandInfo) {
    let mut effective_user = Effective::Known("root".to_string());
    let mut command_str: Option<String> = None;
    let mut i = 0;

    while i < info.args.len() {
        let arg = &info.args[i];
        match arg.as_str() {
            "-c" => {
                if i + 1 < info.args.len() {
                    command_str = Some(info.args[i + 1].clone());
                }
                i += 2;
            }
            "-" | "-l" | "--login" => {
                i += 1;
            }
            arg if arg.starts_with('-') => {
                i += 1;
            }
            _ => {
                // Non-flag argument is the user
                let user_arg = &info.args[i];
                if word_has_expansion(user_arg) {
                    effective_user = Effective::Unknown;
                } else {
                    effective_user = Effective::Known(user_arg.clone());
                }
                i += 1;
            }
        }
    }

    info.effective_user = effective_user;

    if let Some(cmd) = command_str {
        // su -c "command" — re-parse the command string
        if let Ok(inner_cmds) = parse_command(&cmd) {
            info.inner.extend(inner_cmds);
        }
    }
}

fn unwrap_doas(info: &mut CommandInfo) {
    let mut effective_user = Effective::Known("root".to_string());
    let mut i = 0;

    while i < info.args.len() {
        let arg = &info.args[i];
        match arg.as_str() {
            "-u" => {
                if i + 1 < info.args.len() {
                    let user_arg = &info.args[i + 1];
                    if word_has_expansion(user_arg) {
                        effective_user = Effective::Unknown;
                    } else {
                        effective_user = Effective::Known(user_arg.clone());
                    }
                    i += 2;
                } else {
                    i += 1;
                }
            }
            arg if arg.starts_with('-') => {
                i += 1;
            }
            _ => break,
        }
    }

    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
            info.effective_user = effective_user;
        }
        try_unwrap_wrapper(info);
    } else {
        info.effective_user = effective_user;
    }
}

fn unwrap_env(info: &mut CommandInfo) {
    let mut i = 0;
    while i < info.args.len() {
        let arg = &info.args[i];
        if arg.starts_with('-') {
            // Flags: -i, -0, -u NAME, etc.
            if arg == "-u" || arg == "-S" {
                i += 2;
            } else {
                i += 1;
            }
        } else if arg.contains('=') {
            // KEY=VALUE pair
            i += 1;
        } else {
            break;
        }
    }
    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
        }
        try_unwrap_wrapper(info);
    }
}

fn unwrap_string_eval(info: &mut CommandInfo) {
    // sh/bash/zsh/dash -c "command string"
    if let Some(c_pos) = info.args.iter().position(|a| a == "-c") {
        if c_pos + 1 < info.args.len() {
            // The arg value from brush-parser includes shell quotes.
            // Unquote it before re-parsing.
            let raw_arg = &info.args[c_pos + 1];
            let cmd_str = brush_parser::unquote_str(raw_arg);
            if word_has_expansion(&cmd_str) {
                // Can't know what the expansion resolves to
                info.args_complete = false;
            } else if let Ok(inner_cmds) = parse_command(&cmd_str) {
                info.inner.extend(inner_cmds);
            }
        }
    }
    // If no -c flag, it's an interactive shell invocation — keep as-is
}

fn unwrap_find(info: &mut CommandInfo) {
    let exec_flags = ["-exec", "-execdir", "-ok", "-okdir"];
    let mut i = 0;
    while i < info.args.len() {
        if exec_flags.contains(&info.args[i].as_str()) {
            // Extract the command between -exec and \; or +
            let cmd_start = i + 1;
            let mut cmd_end = cmd_start;
            while cmd_end < info.args.len() {
                if info.args[cmd_end] == ";" || info.args[cmd_end] == "+" {
                    break;
                }
                cmd_end += 1;
            }
            if cmd_start < cmd_end {
                let exec_cmd = &info.args[cmd_start];
                let exec_args: Vec<String> = info.args[cmd_start + 1..cmd_end]
                    .iter()
                    .cloned()
                    .collect();
                let mut inner = CommandInfo {
                    name: exec_cmd.clone(),
                    args: exec_args,
                    args_complete: false, // {} is dynamic
                    effective_user: Effective::Unchanged,
                    effective_host: Effective::Unchanged,
                    is_pipe_target: false,
                    inner: Vec::new(),
                };
                try_unwrap_wrapper(&mut inner);
                info.inner.push(inner);
            }
            i = cmd_end + 1;
        } else {
            i += 1;
        }
    }
}

fn unwrap_xargs(info: &mut CommandInfo) {
    // Skip xargs flags, remainder is the command
    let mut i = 0;
    while i < info.args.len() {
        let arg = &info.args[i];
        if arg.starts_with('-') {
            // Flags that take an argument: -d, -I, -L, -n, -P, -s, -E
            if ["-d", "-I", "-L", "-n", "-P", "-s", "-E"].contains(&arg.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
        } else {
            break;
        }
    }
    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
            info.args_complete = false; // args come from stdin
        }
        try_unwrap_wrapper(info);
    }
    // xargs with no command defaults to echo — but we keep it as "xargs"
    // so the CEL rule for xargs can match it
}

fn unwrap_ssh(info: &mut CommandInfo) {
    let mut effective_host = Effective::Unchanged;
    let mut i = 0;

    while i < info.args.len() {
        let arg = &info.args[i];
        if arg.starts_with('-') {
            // Flags that take an argument
            if ["-b", "-c", "-D", "-E", "-e", "-F", "-I", "-i", "-J",
                "-L", "-l", "-m", "-O", "-o", "-p", "-Q", "-R", "-S",
                "-W", "-w"].contains(&arg.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
        } else if matches!(effective_host, Effective::Unchanged) {
            // First non-flag is the host (or user@host)
            let host_arg = &info.args[i];
            if word_has_expansion(host_arg) {
                effective_host = Effective::Unknown;
            } else {
                // Parse user@host
                let host = if let Some(at_pos) = host_arg.find('@') {
                    &host_arg[at_pos + 1..]
                } else {
                    host_arg.as_str()
                };
                effective_host = Effective::Known(host.to_string());
            }
            i += 1;
        } else {
            // Everything after the host is the remote command
            break;
        }
    }

    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
            info.effective_host = effective_host;
        }
        try_unwrap_wrapper(info);
    } else {
        // ssh with no remote command (interactive session)
        info.effective_host = effective_host;
    }
}

/// Unwrap commands like `podman exec` or `docker exec`.
fn unwrap_subcommand_exec(info: &mut CommandInfo, _wrapper_name: &str) {
    // Check if first arg is "exec"
    if info.args.first().map(|s| s.as_str()) != Some("exec") {
        return; // not "X exec", treat as regular command
    }
    info.args.remove(0); // remove "exec"

    let mut effective_host = Effective::Unchanged;
    let mut i = 0;

    while i < info.args.len() {
        let arg = &info.args[i];
        if arg.starts_with('-') {
            // Skip flags like -it, --detach, --env, --user, etc.
            if ["-e", "--env", "-u", "--user", "-w", "--workdir"].contains(&arg.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
        } else if matches!(effective_host, Effective::Unchanged) {
            // First non-flag is the container/pod name
            if word_has_expansion(arg) {
                effective_host = Effective::Unknown;
            } else {
                effective_host = Effective::Known(arg.clone());
            }
            i += 1;
        } else {
            break;
        }
    }

    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
            info.effective_host = effective_host;
        }
        try_unwrap_wrapper(info);
    } else {
        info.effective_host = effective_host;
    }
}

fn unwrap_kubectl(info: &mut CommandInfo) {
    // kubectl exec [flags] POD -- COMMAND [args...]
    if info.args.first().map(|s| s.as_str()) != Some("exec") {
        return;
    }
    info.args.remove(0); // remove "exec"

    let mut effective_host = Effective::Unchanged;
    let mut i = 0;

    while i < info.args.len() {
        let arg = &info.args[i];
        if arg == "--" {
            i += 1; // skip the --
            break;
        } else if arg.starts_with('-') {
            // Flags that take an argument: -n, -c, --namespace, --container
            if ["-n", "-c", "--namespace", "--container"].contains(&arg.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
        } else if matches!(effective_host, Effective::Unchanged) {
            // First non-flag before -- is the pod name
            if word_has_expansion(arg) {
                effective_host = Effective::Unknown;
            } else {
                effective_host = Effective::Known(arg.clone());
            }
            i += 1;
        } else {
            i += 1;
        }
    }

    if i < info.args.len() {
        let remaining: Vec<String> = info.args.drain(i..).collect();
        info.args.clear();
        if let Some((cmd, args)) = remaining.split_first() {
            info.name = cmd.clone();
            info.args = args.to_vec();
            info.effective_host = effective_host;
        }
        try_unwrap_wrapper(info);
    } else {
        info.effective_host = effective_host;
    }
}

// --- Dependency contract tests ---

#[cfg(test)]
mod tests {
    use super::*;

    // --- Dependency contract: brush-parser ---
    // Assert specific behaviors we rely on, so version upgrades don't break us.

    fn parse_program(input: &str) -> ast::Program {
        parse_with_brush(input).unwrap_or_else(|e| panic!("Failed to parse '{}': {}", input, e.message))
    }

    #[test]
    fn brush_parses_simple_command() {
        let prog = parse_program("ls -la");
        assert_eq!(prog.complete_commands.len(), 1);
        let cmds = &prog.complete_commands[0].0;
        assert_eq!(cmds.len(), 1);
        let pipeline = &cmds[0].0.first;
        assert_eq!(pipeline.seq.len(), 1);
        match &pipeline.seq[0] {
            ast::Command::Simple(s) => {
                assert_eq!(s.word_or_name.as_ref().unwrap().value, "ls");
            }
            _ => panic!("expected SimpleCommand"),
        }
    }

    #[test]
    fn brush_parses_pipeline() {
        let prog = parse_program("ls | grep foo");
        let cmds = &prog.complete_commands[0].0;
        let pipeline = &cmds[0].0.first;
        assert_eq!(pipeline.seq.len(), 2);
    }

    #[test]
    fn brush_parses_and_or_list() {
        let prog = parse_program("a && b || c");
        let and_or = &prog.complete_commands[0].0[0].0;
        assert_eq!(and_or.additional.len(), 2);
    }

    #[test]
    fn brush_parses_command_substitution_in_word() {
        let pieces = brush_parser::word::parse(
            "$(date)",
            &brush_parser::ParserOptions::default(),
        ).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::CommandSubstitution(_))));
    }

    #[test]
    fn brush_parses_subshell() {
        let prog = parse_program("(ls)");
        let pipeline = &prog.complete_commands[0].0[0].0.first;
        match &pipeline.seq[0] {
            ast::Command::Compound(ast::CompoundCommand::Subshell(_), _) => {}
            other => panic!("expected Subshell, got {:?}", other),
        }
    }

    #[test]
    fn brush_parses_brace_group() {
        let prog = parse_program("{ ls; }");
        let pipeline = &prog.complete_commands[0].0[0].0.first;
        match &pipeline.seq[0] {
            ast::Command::Compound(ast::CompoundCommand::BraceGroup(_), _) => {}
            other => panic!("expected BraceGroup, got {:?}", other),
        }
    }

    #[test]
    fn brush_parses_if_clause() {
        let prog = parse_program("if true; then echo yes; fi");
        let pipeline = &prog.complete_commands[0].0[0].0.first;
        match &pipeline.seq[0] {
            ast::Command::Compound(ast::CompoundCommand::IfClause(_), _) => {}
            other => panic!("expected IfClause, got {:?}", other),
        }
    }

    #[test]
    fn brush_parses_for_clause() {
        let prog = parse_program("for x in a b; do echo $x; done");
        let pipeline = &prog.complete_commands[0].0[0].0.first;
        match &pipeline.seq[0] {
            ast::Command::Compound(ast::CompoundCommand::ForClause(_), _) => {}
            other => panic!("expected ForClause, got {:?}", other),
        }
    }

    #[test]
    fn brush_parses_quoted_string_not_split() {
        let prog = parse_program("echo 'hello | world'");
        let pipeline = &prog.complete_commands[0].0[0].0.first;
        // Should be one command, not a pipeline
        assert_eq!(pipeline.seq.len(), 1);
    }

    #[test]
    fn brush_rejects_unclosed_quote() {
        let result = parse_with_brush("echo 'hello");
        assert!(result.is_err());
    }

    #[test]
    fn brush_detects_expansion_in_word() {
        let pieces = brush_parser::word::parse(
            "$VAR",
            &brush_parser::ParserOptions::default(),
        ).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::ParameterExpansion(_))));
    }

    #[test]
    fn brush_detects_command_sub_in_word() {
        let pieces = brush_parser::word::parse(
            "$(cmd)",
            &brush_parser::ParserOptions::default(),
        ).unwrap();
        assert!(pieces.iter().any(|p| matches!(&p.piece, WordPiece::CommandSubstitution(_))));
    }

    // --- parse_command tests ---

    #[test]
    fn simple_command_extracts_name_and_args() {
        let cmds = parse_command("ls -la /tmp").unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].args, vec!["-la", "/tmp"]);
        assert!(cmds[0].args_complete);
    }

    #[test]
    fn command_with_no_args() {
        let cmds = parse_command("pwd").unwrap();
        assert_eq!(cmds[0].name, "pwd");
        assert!(cmds[0].args.is_empty());
    }

    #[test]
    fn pipeline_extracts_each_segment() {
        let cmds = parse_command("ls | grep foo").unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[1].name, "grep");
    }

    #[test]
    fn pipeline_marks_non_first_as_pipe_target() {
        let cmds = parse_command("ls | grep foo").unwrap();
        assert!(!cmds[0].is_pipe_target);
        assert!(cmds[1].is_pipe_target);
    }

    #[test]
    fn pipeline_three_segments() {
        let cmds = parse_command("ls | grep foo | wc -l").unwrap();
        assert_eq!(cmds.len(), 3);
        assert_eq!(cmds[2].name, "wc");
    }

    #[test]
    fn quoted_pipe_is_not_pipeline() {
        let cmds = parse_command("echo 'hello | world'").unwrap();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "echo");
    }

    #[test]
    fn and_list_extracts_both() {
        let cmds = parse_command("ls && echo done").unwrap();
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[1].name, "echo");
    }

    #[test]
    fn or_list_extracts_both() {
        let cmds = parse_command("ls || echo failed").unwrap();
        assert_eq!(cmds.len(), 2);
    }

    #[test]
    fn arg_with_command_substitution_has_inner() {
        let cmds = parse_command("echo $(date)").unwrap();
        assert_eq!(cmds[0].name, "echo");
        assert!(!cmds[0].inner.is_empty());
        assert_eq!(cmds[0].inner[0].name, "date");
    }

    #[test]
    fn command_substitution_sets_args_incomplete() {
        let cmds = parse_command("rm $(find . -name '*.tmp')").unwrap();
        assert_eq!(cmds[0].name, "rm");
        assert!(!cmds[0].args_complete);
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

    // --- Wrapper tests ---

    #[test]
    fn command_builtin_unwraps() {
        let cmds = parse_command("command ls -la").unwrap();
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].args, vec!["-la"]);
    }

    #[test]
    fn time_unwraps() {
        let cmds = parse_command("time make -j4").unwrap();
        // time is handled as a keyword by brush-parser, so the command is make
        assert_eq!(cmds[0].name, "make");
    }

    #[test]
    fn sudo_extracts_root() {
        let cmds = parse_command("sudo rm foo").unwrap();
        assert_eq!(cmds[0].name, "rm");
        assert_eq!(cmds[0].effective_user, Effective::Known("root".to_string()));
    }

    #[test]
    fn sudo_u_extracts_user() {
        let cmds = parse_command("sudo -u postgres psql").unwrap();
        assert_eq!(cmds[0].name, "psql");
        assert_eq!(cmds[0].effective_user, Effective::Known("postgres".to_string()));
    }

    #[test]
    fn no_wrapper_user_unchanged() {
        let cmds = parse_command("ls").unwrap();
        assert_eq!(cmds[0].effective_user, Effective::Unchanged);
    }

    #[test]
    fn env_skips_key_val() {
        let cmds = parse_command("env FOO=bar cargo test").unwrap();
        assert_eq!(cmds[0].name, "cargo");
        assert_eq!(cmds[0].args, vec!["test"]);
    }

    #[test]
    fn env_multiple_vars() {
        let cmds = parse_command("env A=1 B=2 make").unwrap();
        assert_eq!(cmds[0].name, "make");
    }

    #[test]
    fn xargs_extracts_command() {
        let cmds = parse_command("xargs cat").unwrap();
        assert_eq!(cmds[0].name, "cat");
        assert!(!cmds[0].args_complete);
    }

    #[test]
    fn xargs_with_known_and_unknown_args() {
        let cmds = parse_command("xargs rm -v").unwrap();
        assert_eq!(cmds[0].name, "rm");
        assert_eq!(cmds[0].args, vec!["-v"]);
        assert!(!cmds[0].args_complete);
    }

    #[test]
    fn ssh_extracts_host() {
        let cmds = parse_command("ssh prod-server ls").unwrap();
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].effective_host, Effective::Known("prod-server".to_string()));
    }

    #[test]
    fn ssh_user_at_host() {
        let cmds = parse_command("ssh user@10.0.0.1 ls").unwrap();
        assert_eq!(cmds[0].effective_host, Effective::Known("10.0.0.1".to_string()));
    }

    #[test]
    fn ssh_with_flags() {
        let cmds = parse_command("ssh -i key.pem user@host ls -la").unwrap();
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].effective_host, Effective::Known("host".to_string()));
    }

    #[test]
    fn kubectl_exec_extracts_pod() {
        let cmds = parse_command("kubectl exec mypod -- ls").unwrap();
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].effective_host, Effective::Known("mypod".to_string()));
    }

    #[test]
    fn kubectl_exec_with_namespace() {
        let cmds = parse_command("kubectl exec -n prod mypod -- ls").unwrap();
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].effective_host, Effective::Known("mypod".to_string()));
    }

    #[test]
    fn podman_exec_extracts_container() {
        let cmds = parse_command("podman exec myapp ls").unwrap();
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].effective_host, Effective::Known("myapp".to_string()));
    }

    #[test]
    fn docker_exec_extracts_container() {
        let cmds = parse_command("docker exec mycontainer ls").unwrap();
        assert_eq!(cmds[0].name, "ls");
        assert_eq!(cmds[0].effective_host, Effective::Known("mycontainer".to_string()));
    }

    // --- Chained wrappers ---

    #[test]
    fn sudo_env_cargo() {
        let cmds = parse_command("sudo env FOO=bar cargo test").unwrap();
        assert_eq!(cmds[0].name, "cargo");
        assert_eq!(cmds[0].effective_user, Effective::Known("root".to_string()));
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

    // --- Parse failures ---

    #[test]
    fn empty_string_parses_as_empty() {
        let cmds = parse_command("").unwrap();
        assert!(cmds.is_empty());
    }

    #[test]
    fn unclosed_quote_fails() {
        assert!(parse_command("echo 'hello").is_err());
    }

    // --- String-eval ---

    #[test]
    fn sh_c_literal_reparses() {
        let cmds = parse_command("sh -c 'echo hello'").unwrap();
        assert_eq!(cmds[0].name, "sh");
        assert!(!cmds[0].inner.is_empty());
        assert_eq!(cmds[0].inner[0].name, "echo");
    }

    // --- find -exec ---

    #[test]
    fn find_exec_extracts_command() {
        let cmds = parse_command("find . -exec grep foo {} ;").unwrap();
        assert_eq!(cmds[0].name, "find");
        assert!(!cmds[0].inner.is_empty());
        assert_eq!(cmds[0].inner[0].name, "grep");
        assert!(!cmds[0].inner[0].args_complete);
    }

    #[test]
    fn nice_skips_flags() {
        let cmds = parse_command("nice -n 10 cargo build").unwrap();
        assert_eq!(cmds[0].name, "cargo");
        assert_eq!(cmds[0].args, vec!["build"]);
    }

    #[test]
    fn timeout_skips_duration() {
        let cmds = parse_command("timeout 30 curl example.com").unwrap();
        assert_eq!(cmds[0].name, "curl");
    }
}
