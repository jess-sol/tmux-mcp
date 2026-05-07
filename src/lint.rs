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
