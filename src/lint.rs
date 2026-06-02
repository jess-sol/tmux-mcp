/// Command linting for command_run.
///
/// Catches common anti-patterns where the LLM pipes to head/tail/grep
/// instead of using built-in parameters. Using the params keeps full
/// output in history for subsequent searches.
///
/// Pipe lints use brush-parser's AST to correctly handle quoting and
/// pipeline structure. Simpler lints (2>&1, background &, cd-to-cwd)
/// use regex where the pattern has no quoting ambiguity.

use brush_parser::ast;
use regex::Regex;
use std::sync::LazyLock;

pub struct LintError {
    pub message: String,
}

impl std::fmt::Display for LintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

/// Lint a command before execution in command_run.
pub fn lint_command_run(command: &str) -> Result<(), LintError> {
    // AST-based pipe lints — parse once, check all patterns
    if let Ok(program) = parse_with_brush(command) {
        if let Some(err) = lint_pipe_to(&program, command, "command_run", "tail") {
            return Err(err);
        }
        if let Some(err) = lint_pipe_to(&program, command, "command_run", "head") {
            return Err(err);
        }
        for name in &["grep", "egrep", "fgrep", "rg"] {
            if let Some(err) = lint_pipe_to(&program, command, "command_run", name) {
                return Err(err);
            }
        }
        if let Some(err) = lint_pipe_to(&program, command, "command_run", "tee") {
            return Err(err);
        }
        if let Some(err) = lint_exit_status_echo(&program, command) {
            return Err(err);
        }
    }

    // Regex-based lints — simple patterns without quoting issues
    if let Some(err) = lint_stderr_redirect(command) {
        return Err(err);
    }
    if let Some(err) = lint_background_job(command) {
        return Err(err);
    }
    Ok(())
}

// --- AST helpers ---

fn parse_with_brush(command: &str) -> Result<ast::Program, ()> {
    use std::io::BufReader;
    let reader = BufReader::new(command.as_bytes());
    let mut parser = brush_parser::Parser::new(
        reader,
        &brush_parser::ParserOptions::default(),
        &brush_parser::SourceInfo::default(),
    );
    parser.parse_program().map_err(|_| ())
}

/// Info about a pipeline whose last command matched a target name.
struct PipelineEnd {
    args: Vec<String>,
    base: String,
}

/// Walk the AST to find a pipeline ending with a command named `target`.
/// Only matches when the pipeline has >= 2 segments (i.e. there's a pipe).
fn find_pipeline_ending_with(program: &ast::Program, target: &str) -> Option<PipelineEnd> {
    for cc in &program.complete_commands {
        if let Some(hit) = walk_compound_list(cc, target) {
            return Some(hit);
        }
    }
    None
}

fn walk_compound_list(list: &ast::CompoundList, target: &str) -> Option<PipelineEnd> {
    for item in &list.0 {
        if let Some(hit) = walk_and_or(&item.0, target) {
            return Some(hit);
        }
    }
    None
}

fn walk_and_or(and_or: &ast::AndOrList, target: &str) -> Option<PipelineEnd> {
    if let Some(hit) = check_pipeline(&and_or.first, target) {
        return Some(hit);
    }
    for additional in &and_or.additional {
        let pipeline = match additional {
            ast::AndOr::And(p) | ast::AndOr::Or(p) => p,
        };
        if let Some(hit) = check_pipeline(pipeline, target) {
            return Some(hit);
        }
    }
    None
}

fn check_pipeline(pipeline: &ast::Pipeline, target: &str) -> Option<PipelineEnd> {
    if pipeline.seq.len() < 2 {
        return None;
    }
    let last = pipeline.seq.last().unwrap();
    let simple = match last {
        ast::Command::Simple(s) => s,
        _ => return None,
    };
    let name = simple.word_or_name.as_ref()?.value.as_str();
    if name != target {
        return None;
    }

    // Collect args from suffix
    let args: Vec<String> = simple
        .suffix
        .as_ref()
        .map(|s| {
            s.0.iter()
                .filter_map(|item| match item {
                    ast::CommandPrefixOrSuffixItem::Word(w) => Some(w.value.clone()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Render the pipeline without the last segment
    let base = render_pipeline_prefix(&pipeline.seq[..pipeline.seq.len() - 1]);

    Some(PipelineEnd { args, base })
}

/// Format a sub-slice of pipeline commands as "cmd1 | cmd2 | ...".
fn render_pipeline_prefix(seq: &[ast::Command]) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    for (i, cmd) in seq.iter().enumerate() {
        if i > 0 {
            out.push_str(" | ");
        }
        let _ = write!(out, "{cmd}");
    }
    out
}

// --- AST-based pipe lints ---

fn lint_pipe_to(
    program: &ast::Program,
    command: &str,
    tool: &str,
    target: &str,
) -> Option<LintError> {
    let hit = find_pipeline_ending_with(program, target)?;
    let base = &hit.base;

    match target {
        "tail" => {
            let args_str = hit.args.join(" ");
            let n = extract_line_count(&args_str).unwrap_or(20);
            Some(LintError {
                message: format!(
                    "Don't pipe to tail — use the tail parameter instead.\n\
                     \n\
                     Instead of:  {tool}(command=\"{command}\")\n\
                     Try:         {tool}(command=\"{base}\", tail={n})\n\
                     \n\
                     This preserves full output in history. Use command_read(search=...) to search it later."
                ),
            })
        }
        "head" => {
            let args_str = hit.args.join(" ");
            let n = extract_line_count(&args_str).unwrap_or(20);
            Some(LintError {
                message: format!(
                    "Don't pipe to head — use the head parameter instead.\n\
                     \n\
                     Instead of:  {tool}(command=\"{command}\")\n\
                     Try:         {tool}(command=\"{base}\", head={n})\n\
                     \n\
                     This preserves full output in history. Use command_read(search=...) to search it later."
                ),
            })
        }
        "grep" | "egrep" | "fgrep" | "rg" => {
            let pattern = extract_grep_pattern(&hit.args);
            Some(LintError {
                message: format!(
                    "Don't pipe to grep — use the search parameter instead.\n\
                     \n\
                     Instead of:  {tool}(command=\"{command}\")\n\
                     Try:         {tool}(command=\"{base}\", search=\"{pattern}\")\n\
                     \n\
                     search supports full regex. This preserves full output for multiple different searches."
                ),
            })
        }
        "tee" => {
            Some(LintError {
                message: format!(
                    "Don't pipe to tee — output is already saved in command history.\n\
                     \n\
                     Instead of:  command_run(command=\"{command}\")\n\
                     Try:         command_run(command=\"{base}\")\n\
                     \n\
                     Use command_read to access full output as many times as needed."
                ),
            })
        }
        _ => None,
    }
}

/// Lint: discourage reading back the exit code (`$?`) with `echo`/`printf`.
///
/// command_run captures each command's exit status out-of-band (via OSC 133
/// markers) and reports it itself; command_history keeps it for past commands.
/// So echoing `$?` is redundant. Two shapes get two nudges:
///   - `cmd; echo "EXIT=$?"` — the echo is appended to a real command, whose
///     status is already in this run's result; suggest dropping the echo.
///   - `echo $?` on its own — the agent is re-running a command just to read a
///     prior result; point at command_history, which has it authoritatively
///     (a bare `echo $?` is also fragile, since the shell prompt runs between
///     command_run calls and can reset `$?`).
///
/// To avoid blocking genuine `$?` uses, we fire only when the trailing command
/// is a bare (non-piped) `echo`/`printf` referencing `$?`/`${?}` with no output
/// redirect. So `RC=$?; ...`, `cmd; [ $? -eq 0 ] && ...`, and `echo $? > rc`
/// (persisting the value) all pass untouched.
fn lint_exit_status_echo(program: &ast::Program, command: &str) -> Option<LintError> {
    let cc = program.complete_commands.last()?;
    let item = cc.0.last()?;
    let and_or = &item.0;

    // The last pipeline executed in this list: the final `&&`/`||` segment if
    // any, otherwise the first.
    let pipeline = match and_or.additional.last() {
        Some(ast::AndOr::And(p) | ast::AndOr::Or(p)) => p,
        None => &and_or.first,
    };
    // Only a bare trailing command, not the receiving end of a pipe.
    if pipeline.seq.len() != 1 {
        return None;
    }
    let simple = match pipeline.seq.last()? {
        ast::Command::Simple(s) => s,
        _ => return None,
    };
    let name = simple.word_or_name.as_ref()?.value.as_str();
    if name != "echo" && name != "printf" {
        return None;
    }

    let suffix = simple.suffix.as_ref()?;
    // A redirect means the value is being persisted somewhere — leave it alone.
    let has_redirect = suffix
        .0
        .iter()
        .any(|i| matches!(i, ast::CommandPrefixOrSuffixItem::IoRedirect(_)));
    if has_redirect {
        return None;
    }
    let refs_status = suffix.0.iter().any(|i| match i {
        ast::CommandPrefixOrSuffixItem::Word(w) => {
            w.value.contains("$?") || w.value.contains("${?}")
        }
        _ => false,
    });
    if !refs_status {
        return None;
    }

    // Whether a command runs before this echo decides which nudge applies.
    let has_preceding = program.complete_commands.len() > 1
        || cc.0.len() > 1
        || !and_or.additional.is_empty();

    if has_preceding {
        // An echo appended to a real command — the status is already in this
        // run's result. Suggest the command without the trailing echo.
        let base = render_without_trailing_command(program)?;
        Some(LintError {
            message: format!(
                "Don't echo $? — command_run already reports each command's exit code.\n\
                 \n\
                 Instead of:  command_run(command=\"{command}\")\n\
                 Try:         command_run(command=\"{base}\")\n\
                 \n\
                 The exit status of the last command is captured and shown automatically; \
                 reading it back with echo just duplicates it. To branch on the status, \
                 use it directly (e.g. `cmd && on_success || on_failure`)."
            ),
        })
    } else {
        // A standalone `echo $?` to inspect a previous run — command_history is
        // the authoritative source.
        Some(LintError {
            message: format!(
                "Don't re-run a command to read $? — use the command_history tool.\n\
                 \n\
                 Rejected:    command_run(command=\"{command}\")\n\
                 \n\
                 command_history lists recent commands with their exit codes, captured \
                 directly when each command finished. A bare `echo $?` in a new command_run \
                 is fragile: the shell prompt runs between runs and can reset $?."
            ),
        })
    }
}

/// Re-render `program` with its final command removed, for use as a suggested
/// replacement. Mirrors the navigation in [`lint_trailing_exit_echo`].
fn render_without_trailing_command(program: &ast::Program) -> Option<String> {
    let mut prog = program.clone();
    let cc = prog.complete_commands.last_mut()?;
    let item = cc.0.last_mut()?;
    if item.0.additional.is_empty() {
        // Trailing command was its own list item — drop the whole item.
        cc.0.pop();
        if cc.0.is_empty() {
            prog.complete_commands.pop();
        }
    } else {
        // Trailing command was the last `&&`/`||` segment — drop just that.
        item.0.additional.pop();
    }
    let base = format!("{prog}");
    let base = base.trim();
    if base.is_empty() {
        None
    } else {
        Some(base.to_string())
    }
}

/// Extract the search pattern from grep args, skipping flag arguments.
fn extract_grep_pattern(args: &[String]) -> String {
    for arg in args {
        if !arg.starts_with('-') {
            return brush_parser::unquote_str(arg);
        }
    }
    "pattern".to_string()
}

// --- Regex-based lints ---

fn lint_stderr_redirect(command: &str) -> Option<LintError> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"2>&1\s*$").unwrap());
    if !RE.is_match(command) {
        return None;
    }
    let base = RE.replace(command, "").trim().to_string();

    Some(LintError {
        message: format!(
            "Don't redirect stderr — it's already captured.\n\
             \n\
             Instead of:  command_run(command=\"{command}\")\n\
             Try:         command_run(command=\"{base}\")\n\
             \n\
             Both stdout and stderr are visible in the terminal and captured automatically."
        ),
    })
}

fn lint_background_job(command: &str) -> Option<LintError> {
    // Match trailing & but not && or &> or &>>
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|[^&])\s*&\s*$").unwrap());
    if !RE.is_match(command) {
        return None;
    }

    Some(LintError {
        message: format!(
            "Don't background commands with & — use a separate pane instead.\n\
             \n\
             Rejected:    command_run(command=\"{command}\")\n\
             \n\
             Background jobs bypass output capture and can't be monitored reliably. \
             Use list_panes to find or create another pane and run the command there."
        ),
    })
}

/// Lint: reject `cd <path> && ...` when `<path>` resolves to the pane's cwd.
///
/// Catches the common AI pattern of prefixing commands with a redundant cd
/// to the directory the pane is already in, which just triggers a needless
/// policy prompt.
pub fn lint_cd_to_cwd(command: &str, cwd: &str) -> Result<(), LintError> {
    static RE: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^cd\s+(\S+)\s*(?:&&|;)\s*(.+)$").unwrap()
    });
    let Some(caps) = RE.captures(command) else {
        return Ok(());
    };
    let cd_target = caps.get(1).unwrap().as_str().trim_end_matches('/');
    let rest = caps.get(2).unwrap().as_str();
    let cwd_clean = cwd.trim_end_matches('/');

    let is_cwd = cd_target == "." || cd_target == cwd_clean;

    if is_cwd {
        Err(LintError {
            message: format!(
                "Redundant cd — the pane is already in that directory.\n\
                 \n\
                 Instead of:  command_run(command=\"{command}\")\n\
                 Try:         command_run(command=\"{rest}\")\n\
                 \n\
                 The pane's working directory is already {cwd}."
            ),
        })
    } else {
        Ok(())
    }
}

/// Extract line count from tail/head args like "-5", "-n 10", "-n10".
fn extract_line_count(args: &str) -> Option<u64> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"-n?\s*(\d+)|-(\d+)").unwrap());
    let caps = RE.captures(args)?;
    caps.get(1)
        .or(caps.get(2))
        .and_then(|m| m.as_str().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Tail ---

    #[test]
    fn tail_pipe_rejected() {
        let err = lint_command_run("ls | tail -5").unwrap_err();
        assert!(err.message.contains("tail=5"), "{}", err);
    }

    #[test]
    fn tail_pipe_with_n_flag() {
        let err = lint_command_run("cat file | tail -n 10").unwrap_err();
        assert!(err.message.contains("tail=10"), "{}", err);
    }

    #[test]
    fn tail_without_pipe_allowed() {
        assert!(lint_command_run("tail -f /var/log/syslog").is_ok());
    }

    #[test]
    fn tail_mid_pipeline_allowed() {
        assert!(lint_command_run("ls | tail -5 | sort").is_ok());
    }

    // --- Head ---

    #[test]
    fn head_pipe_rejected() {
        let err = lint_command_run("ls | head -20").unwrap_err();
        assert!(err.message.contains("head=20"), "{}", err);
    }

    #[test]
    fn head_without_pipe_allowed() {
        assert!(lint_command_run("head -20 file.txt").is_ok());
    }

    #[test]
    fn head_mid_pipeline_allowed() {
        assert!(lint_command_run("ls | head -20 | sort").is_ok());
        assert!(lint_command_run("cat file | head -5 | wc -l").is_ok());
    }

    // --- Grep ---

    #[test]
    fn grep_pipe_rejected() {
        let err = lint_command_run("ps aux | grep python").unwrap_err();
        assert!(err.message.contains("search=\"python\""), "{}", err);
    }

    #[test]
    fn grep_pipe_with_flags() {
        let err = lint_command_run("make | grep -i error").unwrap_err();
        assert!(err.message.contains("search=\"error\""), "{}", err);
    }

    #[test]
    fn grep_pipe_with_quoted_pattern() {
        let err = lint_command_run("cat file | egrep 'foo|bar'").unwrap_err();
        assert!(err.message.contains("search=\"foo|bar\""), "{}", err);
    }

    #[test]
    fn rg_pipe_rejected() {
        assert!(lint_command_run("ls -la | rg pattern").is_err());
    }

    #[test]
    fn grep_without_pipe_allowed() {
        assert!(lint_command_run("grep pattern file.txt").is_ok());
        assert!(lint_command_run("rg pattern .").is_ok());
    }

    #[test]
    fn grep_mid_pipeline_allowed() {
        assert!(lint_command_run("ps aux | grep python | wc -l").is_ok());
    }

    // --- Stderr ---

    #[test]
    fn stderr_redirect_rejected() {
        let err = lint_command_run("make 2>&1").unwrap_err();
        assert!(err.message.contains("command_run(command=\"make\")"), "{}", err);
    }

    #[test]
    fn stderr_redirect_piped_allowed() {
        assert!(lint_command_run("make 2>&1 | sort").is_ok());
    }

    // --- Tee ---

    #[test]
    fn tee_pipe_rejected() {
        let err = lint_command_run("ls | tee out.txt").unwrap_err();
        assert!(err.message.contains("command_run(command=\"ls\")"), "{}", err);
    }

    #[test]
    fn tee_without_pipe_allowed() {
        assert!(lint_command_run("tee output.log").is_ok());
    }

    #[test]
    fn tee_mid_pipeline_allowed() {
        assert!(lint_command_run("make | tee build.log | wc -l").is_ok());
    }

    // --- Trailing echo $? ---

    #[test]
    fn trailing_echo_exit_code_rejected() {
        let err = lint_command_run("helmfile -e localsync; echo \"HELMFILE_EXIT=$?\"").unwrap_err();
        assert!(err.message.contains("command_run(command=\"helmfile -e localsync\")"), "{}", err);
    }

    #[test]
    fn trailing_echo_bare_status_rejected() {
        let err = lint_command_run("make; echo $?").unwrap_err();
        assert!(err.message.contains("command_run(command=\"make\")"), "{}", err);
    }

    #[test]
    fn trailing_echo_after_and_rejected() {
        let err = lint_command_run("cargo test && echo \"rc=$?\"").unwrap_err();
        assert!(err.message.contains("command_run(command=\"cargo test\")"), "{}", err);
    }

    #[test]
    fn trailing_echo_with_midcommand_stderr_redirect_rejected() {
        // The 2>&1 is mid-string (not trailing) so the stderr lint misses it;
        // this lint still catches the redundant exit-code echo.
        let err = lint_command_run("helmfile -e localsync 2>&1; echo \"HELMFILE_EXIT=$?\"").unwrap_err();
        assert!(err.message.contains("echo $?"), "{}", err);
    }

    #[test]
    fn trailing_printf_status_rejected() {
        assert!(lint_command_run("make; printf 'exit %d\\n' $?").is_err());
    }

    #[test]
    fn trailing_echo_braced_status_rejected() {
        assert!(lint_command_run("make; echo \"${?}\"").is_err());
    }

    #[test]
    fn bare_echo_status_nudges_command_history() {
        // No preceding command — the agent is re-running just to read a prior
        // result; point it at command_history instead.
        let err = lint_command_run("echo $?").unwrap_err();
        assert!(err.message.contains("command_history"), "{}", err);
    }

    #[test]
    fn bare_printf_status_nudges_command_history() {
        let err = lint_command_run("printf '%d\\n' $?").unwrap_err();
        assert!(err.message.contains("command_history"), "{}", err);
    }

    #[test]
    fn bare_echo_status_redirected_allowed() {
        // Even standalone, persisting the code to a file is legitimate.
        assert!(lint_command_run("echo $? > /tmp/rc").is_ok());
    }

    #[test]
    fn echo_status_redirected_allowed() {
        // Persisting the exit code to a file is a legitimate use.
        assert!(lint_command_run("make; echo $? > /tmp/rc").is_ok());
    }

    #[test]
    fn echo_without_status_allowed() {
        assert!(lint_command_run("make; echo done").is_ok());
    }

    #[test]
    fn mid_sequence_echo_status_allowed() {
        // $? echoed mid-script, with real work after — not a trailing report.
        assert!(lint_command_run("make; echo $?; deploy").is_ok());
    }

    #[test]
    fn status_in_condition_allowed() {
        // Branching on $? — trailing command is `echo ok`, which has no $?.
        assert!(lint_command_run("make; [ $? -eq 0 ] && echo ok").is_ok());
    }

    #[test]
    fn status_assignment_allowed() {
        // Capturing into a variable for later use is fine.
        assert!(lint_command_run("make; RC=$?; echo $RC").is_ok());
    }

    // --- Background ---

    #[test]
    fn background_trailing_ampersand_rejected() {
        let err = lint_command_run("sleep 60 &").unwrap_err();
        assert!(err.message.contains("separate pane"), "{}", err);
    }

    #[test]
    fn background_no_space_rejected() {
        assert!(lint_command_run("make&").is_err());
    }

    #[test]
    fn background_trailing_whitespace_rejected() {
        assert!(lint_command_run("cargo build &  ").is_err());
    }

    #[test]
    fn logical_and_allowed() {
        assert!(lint_command_run("make && make install").is_ok());
    }

    #[test]
    fn redirect_ampersand_allowed() {
        assert!(lint_command_run("make &> /dev/null").is_ok());
        assert!(lint_command_run("make &>> log.txt").is_ok());
    }

    #[test]
    fn ampersand_in_url_allowed() {
        assert!(lint_command_run("curl 'http://example.com?a=1&b=2'").is_ok());
    }

    // --- Valid ---

    #[test]
    fn valid_commands() {
        assert!(lint_command_run("ls -la").is_ok());
        assert!(lint_command_run("cargo build").is_ok());
        assert!(lint_command_run("git status").is_ok());
        assert!(lint_command_run("make -j8").is_ok());
    }

    // --- cd to cwd ---

    #[test]
    fn cd_to_cwd_absolute_rejected() {
        let err = lint_cd_to_cwd("cd /home/user/project && cargo test", "/home/user/project").unwrap_err();
        assert!(err.message.contains("command_run(command=\"cargo test\")"), "{}", err);
    }

    #[test]
    fn cd_to_cwd_trailing_slash_rejected() {
        assert!(lint_cd_to_cwd("cd /home/user/project/ && cargo test", "/home/user/project").is_err());
    }

    #[test]
    fn cd_to_cwd_dot_rejected() {
        assert!(lint_cd_to_cwd("cd . && make", "/any/dir").is_err());
    }

    #[test]
    fn cd_to_cwd_semicolon_rejected() {
        assert!(lint_cd_to_cwd("cd /proj && make", "/proj").is_err());
    }

    #[test]
    fn cd_to_different_dir_allowed() {
        assert!(lint_cd_to_cwd("cd /tmp && ls", "/home/user/project").is_ok());
    }

    #[test]
    fn cd_to_subdir_allowed() {
        assert!(lint_cd_to_cwd("cd src && cargo test", "/home/user/project").is_ok());
    }

    #[test]
    fn cd_standalone_allowed() {
        assert!(lint_cd_to_cwd("cd /home/user/project", "/home/user/project").is_ok());
    }

    #[test]
    fn cd_cwd_trailing_slash_normalized() {
        assert!(lint_cd_to_cwd("cd /proj && ls", "/proj/").is_err());
    }

    // --- Helpers ---

    #[test]
    fn extract_line_count_variants() {
        assert_eq!(extract_line_count("-5"), Some(5));
        assert_eq!(extract_line_count("-n 10"), Some(10));
        assert_eq!(extract_line_count("-n10"), Some(10));
        assert_eq!(extract_line_count(""), None);
    }
}
