/// Command linting for command_run.
///
/// Catches common anti-patterns where the LLM pipes to head/tail/grep
/// instead of using built-in parameters. Using the params keeps full
/// output in history for subsequent searches.

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
    if let Some(err) = lint_tail_pipe(command, "command_run") {
        return Err(err);
    }
    if let Some(err) = lint_head_pipe(command, "command_run") {
        return Err(err);
    }
    if let Some(err) = lint_grep_pipe(command, "command_run") {
        return Err(err);
    }
    if let Some(err) = lint_stderr_redirect(command) {
        return Err(err);
    }
    if let Some(err) = lint_tee_pipe(command) {
        return Err(err);
    }
    if let Some(err) = lint_background_job(command) {
        return Err(err);
    }
    Ok(())
}

fn lint_tail_pipe(command: &str, tool: &str) -> Option<LintError> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\|\s*tail\b(.*)$").unwrap());
    let caps = RE.captures(command)?;
    let base = command[..caps.get(0).unwrap().start()].trim_end();

    // Try to extract -N or -n N
    let tail_args = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
    let n = extract_line_count(tail_args).unwrap_or(20);

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

fn lint_head_pipe(command: &str, tool: &str) -> Option<LintError> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\|\s*head\b(.*)$").unwrap());
    let caps = RE.captures(command)?;
    let base = command[..caps.get(0).unwrap().start()].trim_end();

    let head_args = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
    let n = extract_line_count(head_args).unwrap_or(20);

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

fn lint_grep_pipe(command: &str, tool: &str) -> Option<LintError> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"\|\s*(?:grep|rg|egrep|fgrep)\s+(.+)$").unwrap());
    let caps = RE.captures(command)?;
    let base = command[..caps.get(0).unwrap().start()].trim_end();

    // Extract the pattern (strip common flags like -i, -E, etc.)
    let raw_pattern = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("pattern");
    let pattern = strip_grep_flags(raw_pattern);

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

fn lint_tee_pipe(command: &str) -> Option<LintError> {
    static RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\|\s*tee\b").unwrap());
    if !RE.is_match(command) {
        return None;
    }
    let base = command[..RE.find(command).unwrap().start()].trim_end();

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

/// Strip common grep flags to extract the search pattern.
fn strip_grep_flags(args: &str) -> String {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^(-[iEFwvcn]+\s+)*").unwrap());
    let stripped = RE.replace(args, "").trim().to_string();
    // Remove surrounding quotes
    stripped
        .strip_prefix('\'').and_then(|s| s.strip_suffix('\''))
        .or_else(|| stripped.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(&stripped)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Tail ---

    #[test]
    fn tail_pipe_rejected() {
        let err = lint_command_run("ls | tail -5").unwrap_err();
        assert!(err.message.contains("command_run(command=\"ls\", tail=5)"), "{}", err);
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

    // --- Head ---

    #[test]
    fn head_pipe_rejected() {
        let err = lint_command_run("ls | head -20").unwrap_err();
        assert!(err.message.contains("command_run(command=\"ls\", head=20)"), "{}", err);
    }

    #[test]
    fn head_without_pipe_allowed() {
        assert!(lint_command_run("head -20 file.txt").is_ok());
    }

    // --- Grep ---

    #[test]
    fn grep_pipe_rejected() {
        let err = lint_command_run("ps aux | grep python").unwrap_err();
        assert!(err.message.contains("search=\"python\""), "{}", err);
        assert!(err.message.contains("command_run(command=\"ps aux\""), "{}", err);
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

    // --- Helpers ---

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

    #[test]
    fn strip_grep_flags_variants() {
        assert_eq!(strip_grep_flags("pattern"), "pattern");
        assert_eq!(strip_grep_flags("-i error"), "error");
        assert_eq!(strip_grep_flags("-E 'foo|bar'"), "foo|bar");
        assert_eq!(strip_grep_flags("-iv \"test\""), "test");
    }
}
