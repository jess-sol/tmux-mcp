//! Argument parsing for command wrappers.
//!
//! Implements POSIX getopt and GNU (interspersed) argument parsing, with
//! three-valued logic for conservative handling of unknowns.
//!
//! Spec references:
//! - POSIX getopt(): https://pubs.opengroup.org/onlinepubs/9699919799/functions/getopt.html
//! - POSIX Utility Syntax Guidelines: https://pubs.opengroup.org/onlinepubs/9699919799/basedefs/V1_chap12.html
//! - GNU Argument Syntax: https://www.gnu.org/software/libc/manual/html_node/Argument-Syntax.html
//! - Go pflag (cobra): https://pkg.go.dev/github.com/spf13/pflag

use std::collections::HashMap;
use super::rules::TriVal;

/// Argument parsing style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgStyle {
    /// POSIX getopt: option processing stops at first non-option argument.
    /// Used by: sudo, ssh, nice, timeout, strace, watch, xargs, env, su, doas
    Posix,
    /// GNU/interspersed (pflag/cobra): options recognized anywhere before `--`.
    /// Used by: kubectl, docker, podman
    Gnu,
}

/// Specification for how to parse a command's arguments.
#[derive(Debug, Clone)]
pub struct ArgSpec {
    pub style: ArgStyle,
    /// Flags that consume the next argument as their value.
    /// Supports short (`-u`), long (`--user`), and non-POSIX multi-char (`-name`).
    pub valued: Vec<String>,
    /// Flags that consume arguments until a sentinel token.
    /// E.g., find's `-exec` consumes until `;` or `+`.
    pub terminated: HashMap<String, Vec<String>>,
}

impl ArgSpec {
    pub fn posix(valued: &[&str]) -> Self {
        Self {
            style: ArgStyle::Posix,
            valued: valued.iter().map(|s| s.to_string()).collect(),
            terminated: HashMap::new(),
        }
    }

    pub fn gnu(valued: &[&str]) -> Self {
        Self {
            style: ArgStyle::Gnu,
            valued: valued.iter().map(|s| s.to_string()).collect(),
            terminated: HashMap::new(),
        }
    }
}

/// Result of parsing arguments.
#[derive(Debug)]
pub struct ParsedArgs {
    /// Non-option arguments, in order.
    pub operands: Vec<TriVal>,
    /// Flag values: flag name → value. Only flags in the valued list appear here.
    pub flags: HashMap<String, TriVal>,
    /// Terminated flag blocks: flag name → list of blocks (each block is a list of args).
    pub terminated_blocks: HashMap<String, Vec<Vec<TriVal>>>,
    /// False if unknown flags or other uncertainty degraded confidence in the parse.
    /// When false, absent flags/positionals should be treated as Unknown, not Null.
    pub exhaustive: bool,
}

impl ParsedArgs {
    /// Value of a flag, or Null (exhaustive) / Unknown (non-exhaustive) if absent.
    pub fn value(&self, flag: &str) -> TriVal {
        self.flags.get(flag).cloned().unwrap_or_else(|| self.absent())
    }

    /// Nth operand (0-indexed), or Null/Unknown if out of bounds.
    pub fn positional(&self, n: usize) -> TriVal {
        self.operands.get(n).cloned().unwrap_or_else(|| self.absent())
    }

    /// Operands from index n onward.
    pub fn operands_from(&self, n: usize) -> Vec<TriVal> {
        if n < self.operands.len() {
            self.operands[n..].to_vec()
        } else {
            Vec::new()
        }
    }

    /// All blocks for a terminated flag.
    pub fn values(&self, flag: &str) -> &[Vec<TriVal>] {
        self.terminated_blocks.get(flag).map(|v| v.as_slice()).unwrap_or(&[])
    }

    fn absent(&self) -> TriVal {
        if self.exhaustive { TriVal::Null } else { TriVal::Unknown }
    }
}

/// Parse arguments according to the given spec.
pub fn parse_args(args: &[TriVal], spec: &ArgSpec, exhaustive: bool) -> ParsedArgs {
    let mut result = ParsedArgs {
        operands: Vec::new(),
        flags: HashMap::new(),
        terminated_blocks: HashMap::new(),
        exhaustive,
    };
    let mut past_options = false; // POSIX: set on first non-option

    let mut i = 0;
    while i < args.len() {
        let arg_str = match &args[i] {
            TriVal::String(s) => s.clone(),
            TriVal::Unknown => {
                result.operands.push(TriVal::Unknown);
                result.exhaustive = false;
                i += 1;
                continue;
            }
            _ => { i += 1; continue; }
        };

        // Terminated flags: recognized anywhere (both modes).
        if let Some(terminators) = spec.terminated.get(&arg_str) {
            i += 1;
            let mut block = Vec::new();
            while i < args.len() {
                if let TriVal::String(s) = &args[i] {
                    if terminators.contains(s) {
                        i += 1;
                        break;
                    }
                }
                block.push(args[i].clone());
                i += 1;
            }
            result.terminated_blocks.entry(arg_str).or_default().push(block);
            continue;
        }

        // Option processing gate:
        //   POSIX: only before first non-option (past_options)
        //   GNU: always (until --)
        let in_option_region = match spec.style {
            ArgStyle::Posix => !past_options,
            ArgStyle::Gnu => true,
        };

        if in_option_region && arg_str.len() > 1 && arg_str.starts_with('-') {
            // "--" ends option processing in both modes
            if arg_str == "--" {
                i += 1;
                while i < args.len() {
                    result.operands.push(args[i].clone());
                    i += 1;
                }
                break;
            }

            // Long option: --flag or --flag=value
            if arg_str.starts_with("--") {
                if let Some(eq_pos) = arg_str.find('=') {
                    let flag_part = &arg_str[..eq_pos];
                    if spec.valued.iter().any(|f| f == flag_part) {
                        let val_str = &arg_str[eq_pos + 1..];
                        result.flags.entry(flag_part.to_string()).or_insert_with(|| str_to_trival(val_str));
                    }
                    // --unknown=val is self-contained; doesn't affect exhaustiveness
                    i += 1;
                } else if spec.valued.iter().any(|f| f == &arg_str) {
                    result.flags.entry(arg_str).or_insert_with(|| consume_value(args, i + 1));
                    i += 2;
                } else {
                    // Unknown long flag without = — might take a value
                    result.exhaustive = false;
                    i += 1;
                }
                continue;
            }

            // Short option: first check full string as valued (find's -name)
            if spec.valued.iter().any(|f| f == &arg_str) {
                result.flags.entry(arg_str).or_insert_with(|| consume_value(args, i + 1));
                i += 2;
                continue;
            }

            // POSIX short option group: -abc
            let chars: Vec<char> = arg_str[1..].chars().collect();
            let mut ci = 0;
            let mut consumed_next = false;
            let mut found_valued = false;
            while ci < chars.len() {
                let flag_name = format!("-{}", chars[ci]);
                if spec.valued.iter().any(|f| f == &flag_name) {
                    found_valued = true;
                    if ci + 1 < chars.len() {
                        // Attached: rest of string is value
                        let val_str: String = chars[ci + 1..].iter().collect();
                        result.flags.entry(flag_name).or_insert_with(|| str_to_trival(&val_str));
                    } else {
                        // Separate: next arg is value
                        result.flags.entry(flag_name).or_insert_with(|| consume_value(args, i + 1));
                        consumed_next = true;
                    }
                    break;
                }
                ci += 1;
            }
            if !found_valued {
                result.exhaustive = false;
            }
            i += if consumed_next { 2 } else { 1 };
            continue;
        }

        // Non-option argument = operand
        if spec.style == ArgStyle::Posix {
            past_options = true;
        }
        result.operands.push(str_to_trival(&arg_str));
        i += 1;
    }

    result
}

/// Convert a string to TriVal, checking for shell expansions.
fn str_to_trival(s: &str) -> TriVal {
    if super::parse::word_has_expansion(s) {
        TriVal::Unknown
    } else {
        TriVal::String(s.to_string())
    }
}

/// Extract the value for a flag from the next arg position.
fn consume_value(args: &[TriVal], next_i: usize) -> TriVal {
    if next_i >= args.len() {
        return TriVal::Null;
    }
    match &args[next_i] {
        TriVal::String(s) => str_to_trival(s),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(val: &str) -> TriVal { TriVal::String(val.to_string()) }
    fn tv_args(vals: &[&str]) -> Vec<TriVal> { vals.iter().map(|v| s(v)).collect() }

    fn operands(r: &ParsedArgs) -> Vec<String> {
        r.operands.iter().filter_map(|e| {
            if let TriVal::String(s) = e { Some(s.clone()) } else { None }
        }).collect()
    }

    fn flag(r: &ParsedArgs, name: &str) -> TriVal {
        r.value(name)
    }

    fn terminated(r: &ParsedArgs, name: &str) -> Vec<Vec<String>> {
        r.values(name).iter().map(|block| {
            block.iter().filter_map(|e| {
                if let TriVal::String(s) = e { Some(s.clone()) } else { None }
            }).collect()
        }).collect()
    }

    // ========================================================================
    // POSIX mode
    // ========================================================================

    #[test]
    fn posix_empty_args() {
        let r = parse_args(&[], &ArgSpec::posix(&[]), true);
        assert!(operands(&r).is_empty());
        assert!(r.exhaustive);
    }

    #[test]
    fn posix_all_operands() {
        let r = parse_args(&tv_args(&["ls", "-la", "/"]), &ArgSpec::posix(&[]), true);
        assert_eq!(operands(&r), vec!["ls", "-la", "/"]);
    }

    #[test]
    fn posix_valued_flag_separate() {
        let r = parse_args(&tv_args(&["-n", "10", "cmd"]), &ArgSpec::posix(&["-n"]), true);
        assert_eq!(flag(&r, "-n"), s("10"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_valued_flag_attached() {
        // -fvalue: -f takes "value" (attached, POSIX Guideline 7 form 2)
        let r = parse_args(&tv_args(&["-fvalue", "cmd"]), &ArgSpec::posix(&["-f"]), true);
        assert_eq!(flag(&r, "-f"), s("value"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_grouped_boolean_flags() {
        // -abc: three standalone flags, no valued
        let r = parse_args(&tv_args(&["-abc", "file"]), &ArgSpec::posix(&[]), true);
        assert_eq!(operands(&r), vec!["file"]);
    }

    #[test]
    fn posix_grouped_valued_last() {
        // -au root: -a standalone, -u valued (last char, separate value)
        let r = parse_args(
            &tv_args(&["-au", "root", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_grouped_valued_attached() {
        // -uroot: -u with attached value "root"
        let r = parse_args(
            &tv_args(&["-uroot", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_grouped_valued_mid_attached() {
        // -iuroot: -i standalone, -u valued with attached "root"
        let r = parse_args(
            &tv_args(&["-iuroot", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_bare_dash_is_operand() {
        // "-" is not an option per POSIX spec
        let r = parse_args(&tv_args(&["-v", "-", "file"]), &ArgSpec::posix(&[]), true);
        assert_eq!(operands(&r), vec!["-", "file"]);
    }

    #[test]
    fn posix_bare_dash_alone() {
        let r = parse_args(&tv_args(&["-"]), &ArgSpec::posix(&[]), true);
        assert_eq!(operands(&r), vec!["-"]);
    }

    #[test]
    fn posix_double_dash_ends_options() {
        let r = parse_args(
            &tv_args(&["-n", "5", "--", "-rf", "file"]),
            &ArgSpec::posix(&["-n"]),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("5"));
        assert_eq!(operands(&r), vec!["-rf", "file"]);
    }

    #[test]
    fn posix_double_dash_only() {
        let r = parse_args(&tv_args(&["--"]), &ArgSpec::posix(&[]), true);
        assert!(operands(&r).is_empty());
    }

    #[test]
    fn posix_double_dash_before_any_flags() {
        let r = parse_args(
            &tv_args(&["--", "-n", "10", "cmd"]),
            &ArgSpec::posix(&["-n"]),
            true,
        );
        assert!(matches!(flag(&r, "-n"), TriVal::Null));
        assert_eq!(operands(&r), vec!["-n", "10", "cmd"]);
    }

    #[test]
    fn posix_stop_at_first_operand() {
        // After "rm", "-rf" is an operand, not a flag
        let r = parse_args(
            &tv_args(&["-u", "root", "rm", "-rf", "/"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn posix_flags_after_operand_are_operands() {
        let r = parse_args(
            &tv_args(&["cmd", "-n", "10"]),
            &ArgSpec::posix(&["-n"]),
            true,
        );
        assert_eq!(operands(&r), vec!["cmd", "-n", "10"]);
        assert!(matches!(flag(&r, "-n"), TriVal::Null));
    }

    #[test]
    fn posix_double_dash_after_operand_is_operand() {
        // Per POSIX: after first non-option, getopt never runs again.
        // -- after an operand is just another operand.
        let r = parse_args(
            &tv_args(&["cmd", "--", "-rf"]),
            &ArgSpec::posix(&[]),
            true,
        );
        assert_eq!(operands(&r), vec!["cmd", "--", "-rf"]);
    }

    #[test]
    fn posix_valued_flag_missing_value() {
        let r = parse_args(&tv_args(&["-u"]), &ArgSpec::posix(&["-u"]), true);
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
    }

    #[test]
    fn posix_valued_flag_value_looks_like_flag() {
        // -u takes next arg even if it starts with -
        let r = parse_args(
            &tv_args(&["-u", "-1", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("-1"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_long_flag_separate() {
        let r = parse_args(
            &tv_args(&["--signal", "KILL", "cmd"]),
            &ArgSpec::posix(&["--signal"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_long_flag_equals() {
        let r = parse_args(
            &tv_args(&["--signal=KILL", "cmd"]),
            &ArgSpec::posix(&["--signal"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_multi_char_single_dash() {
        // find-style: -name is a valued flag with single dash
        let r = parse_args(
            &tv_args(&["-name", "*.rs", "."]),
            &ArgSpec::posix(&["-name"]),
            true,
        );
        assert_eq!(flag(&r, "-name"), s("*.rs"));
        assert_eq!(operands(&r), vec!["."]);
    }

    #[test]
    fn posix_multiple_valued_flags() {
        let r = parse_args(
            &tv_args(&["-u", "root", "-C", "3", "rm", "-rf"]),
            &ArgSpec::posix(&["-u", "-C"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(flag(&r, "-C"), s("3"));
        assert_eq!(operands(&r), vec!["rm", "-rf"]);
    }

    #[test]
    fn posix_absent_flag_is_null() {
        let r = parse_args(&tv_args(&["cmd"]), &ArgSpec::posix(&["-u"]), true);
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
    }

    // POSIX spec example: all 6 forms should produce the same result.
    // cmd -ao arg path path
    // cmd -a -o arg path path
    // cmd -o arg -a path path
    // cmd -a -o arg -- path path
    // cmd -a -oarg path path
    // cmd -aoarg path path

    #[test]
    fn posix_spec_separate() {
        let r = parse_args(
            &tv_args(&["-a", "-o", "arg", "path1", "path2"]),
            &ArgSpec::posix(&["-o"]),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_grouped_separate() {
        let r = parse_args(
            &tv_args(&["-ao", "arg", "path1", "path2"]),
            &ArgSpec::posix(&["-o"]),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_reordered() {
        let r = parse_args(
            &tv_args(&["-o", "arg", "-a", "path1", "path2"]),
            &ArgSpec::posix(&["-o"]),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_double_dash() {
        let r = parse_args(
            &tv_args(&["-a", "-o", "arg", "--", "path1", "path2"]),
            &ArgSpec::posix(&["-o"]),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_attached() {
        let r = parse_args(
            &tv_args(&["-a", "-oarg", "path1", "path2"]),
            &ArgSpec::posix(&["-o"]),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_grouped_attached() {
        let r = parse_args(
            &tv_args(&["-aoarg", "path1", "path2"]),
            &ArgSpec::posix(&["-o"]),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    // ========================================================================
    // GNU mode
    // ========================================================================

    #[test]
    fn gnu_flags_after_operand() {
        // GNU: flags recognized anywhere before --
        let r = parse_args(
            &tv_args(&["pod", "-c", "container", "extra"]),
            &ArgSpec::gnu(&["-c"]),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("container"));
        assert_eq!(operands(&r), vec!["pod", "extra"]);
    }

    #[test]
    fn gnu_double_dash_stops() {
        let r = parse_args(
            &tv_args(&["-c", "container", "pod", "--", "ls", "-la"]),
            &ArgSpec::gnu(&["-c"]),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("container"));
        assert_eq!(operands(&r), vec!["pod", "ls", "-la"]);
    }

    #[test]
    fn gnu_flags_interspersed() {
        // -a op -b op2 -c: all three flags processed
        let r = parse_args(
            &tv_args(&["-a", "op", "-b", "op2", "-c"]),
            &ArgSpec::gnu(&[]),
            true,
        );
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    #[test]
    fn gnu_valued_after_operand() {
        let r = parse_args(
            &tv_args(&["op1", "-n", "5", "op2"]),
            &ArgSpec::gnu(&["-n"]),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("5"));
        assert_eq!(operands(&r), vec!["op1", "op2"]);
    }

    #[test]
    fn gnu_grouped_short_options() {
        // Grouping works the same in GNU mode
        let r = parse_args(
            &tv_args(&["-au", "root", "cmd"]),
            &ArgSpec::gnu(&["-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn gnu_long_flag_equals() {
        let r = parse_args(
            &tv_args(&["op", "--signal=KILL", "op2"]),
            &ArgSpec::gnu(&["--signal"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    #[test]
    fn gnu_kubectl_pattern() {
        // kubectl exec pod -c container -- ls -la
        let r = parse_args(
            &tv_args(&["pod", "-c", "container", "--", "ls", "-la"]),
            &ArgSpec::gnu(&["-n", "-c", "--namespace", "--container"]),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("container"));
        assert_eq!(operands(&r), vec!["pod", "ls", "-la"]);
    }

    // ========================================================================
    // Terminated flags (both modes)
    // ========================================================================

    #[test]
    fn terminated_single_block() {
        let mut spec = ArgSpec::posix(&[]);
        spec.terminated.insert("-exec".into(), vec![";".into(), "+".into()]);
        let r = parse_args(
            &tv_args(&[".", "-exec", "grep", "foo", "{}", ";"]),
            &spec,
            true,
        );
        assert_eq!(terminated(&r, "-exec"), vec![vec!["grep", "foo", "{}"]]);
        assert_eq!(operands(&r), vec!["."]);
    }

    #[test]
    fn terminated_multiple_blocks() {
        let mut spec = ArgSpec::posix(&[]);
        spec.terminated.insert("-exec".into(), vec![";".into()]);
        let r = parse_args(
            &tv_args(&[".", "-exec", "grep", "{}", ";", "-exec", "rm", "{}", ";"]),
            &spec,
            true,
        );
        let blocks = terminated(&r, "-exec");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], vec!["grep", "{}"]);
        assert_eq!(blocks[1], vec!["rm", "{}"]);
    }

    #[test]
    fn terminated_after_operands() {
        let mut spec = ArgSpec::posix(&[]);
        spec.terminated.insert("-exec".into(), vec![";".into()]);
        let r = parse_args(
            &tv_args(&["/var", "/tmp", "-exec", "ls", "{}", ";"]),
            &spec,
            true,
        );
        assert_eq!(operands(&r), vec!["/var", "/tmp"]);
        assert_eq!(terminated(&r, "-exec"), vec![vec!["ls", "{}"]]);
    }

    #[test]
    fn terminated_unterminated() {
        // No sentinel — consumes rest of args
        let mut spec = ArgSpec::posix(&[]);
        spec.terminated.insert("-exec".into(), vec![";".into()]);
        let r = parse_args(
            &tv_args(&["-exec", "grep", "foo"]),
            &spec,
            true,
        );
        assert_eq!(terminated(&r, "-exec"), vec![vec!["grep", "foo"]]);
    }

    #[test]
    fn terminated_plus_terminator() {
        let mut spec = ArgSpec::posix(&[]);
        spec.terminated.insert("-exec".into(), vec![";".into(), "+".into()]);
        let r = parse_args(
            &tv_args(&["-exec", "rm", "{}", "+"]),
            &spec,
            true,
        );
        assert_eq!(terminated(&r, "-exec"), vec![vec!["rm", "{}"]]);
    }

    #[test]
    fn terminated_mixed_with_valued() {
        let mut spec = ArgSpec::posix(&["-name"]);
        spec.terminated.insert("-exec".into(), vec![";".into()]);
        let r = parse_args(
            &tv_args(&["-name", "*.rs", "-exec", "wc", "-l", "{}", ";"]),
            &spec,
            true,
        );
        assert_eq!(flag(&r, "-name"), s("*.rs"));
        assert_eq!(terminated(&r, "-exec"), vec![vec!["wc", "-l", "{}"]]);
    }

    #[test]
    fn terminated_absent_flag() {
        let mut spec = ArgSpec::posix(&[]);
        spec.terminated.insert("-exec".into(), vec![";".into()]);
        let r = parse_args(&tv_args(&[".", "-name", "*.rs"]), &spec, true);
        assert!(terminated(&r, "-exec").is_empty());
    }

    // ========================================================================
    // Exhaustiveness / three-valued logic
    // ========================================================================

    #[test]
    fn exhaustive_all_known() {
        let r = parse_args(
            &tv_args(&["-u", "root", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert!(r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_short_flag() {
        // -x not in valued list — might consume next arg
        let r = parse_args(
            &tv_args(&["-x", "cmd"]),
            &ArgSpec::posix(&[]),
            true,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_long_flag() {
        let r = parse_args(
            &tv_args(&["--unknown", "cmd"]),
            &ArgSpec::posix(&[]),
            true,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_long_with_equals() {
        // --unknown=val is self-contained, doesn't affect boundaries
        let r = parse_args(
            &tv_args(&["--unknown=val", "cmd"]),
            &ArgSpec::posix(&[]),
            true,
        );
        assert!(r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_trival() {
        let mut a = tv_args(&["-u", "root"]);
        a.push(TriVal::Unknown);
        a.push(s("cmd"));
        let r = parse_args(&a, &ArgSpec::posix(&["-u"]), true);
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_input_propagates() {
        let r = parse_args(
            &tv_args(&["-u", "root", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            false,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_absent_flag_null() {
        let r = parse_args(
            &tv_args(&["cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert!(matches!(r.value("-u"), TriVal::Null));
    }

    #[test]
    fn exhaustive_absent_flag_unknown_when_non_exhaustive() {
        let r = parse_args(
            &tv_args(&["-x", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert!(!r.exhaustive);
        assert!(matches!(r.value("-u"), TriVal::Unknown));
    }

    #[test]
    fn exhaustive_absent_positional() {
        let r = parse_args(&tv_args(&["a"]), &ArgSpec::posix(&[]), true);
        assert!(matches!(r.positional(5), TriVal::Null));
    }

    #[test]
    fn exhaustive_absent_positional_when_non_exhaustive() {
        let r = parse_args(&tv_args(&["a"]), &ArgSpec::posix(&[]), false);
        assert!(matches!(r.positional(5), TriVal::Unknown));
    }

    // ========================================================================
    // Auto-unknown (expansion detection)
    // ========================================================================

    #[test]
    fn expansion_flag_value_is_unknown() {
        let r = parse_args(
            &tv_args(&["-u", "$(whoami)", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Unknown));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn expansion_operand_is_unknown() {
        let r = parse_args(&tv_args(&["$HOME/file"]), &ArgSpec::posix(&[]), true);
        assert!(matches!(r.operands[0], TriVal::Unknown));
    }

    #[test]
    fn expansion_attached_value() {
        let r = parse_args(
            &tv_args(&["-u$(id)", "cmd"]),
            &ArgSpec::posix(&["-u"]),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Unknown));
    }

    #[test]
    fn expansion_long_equals() {
        let r = parse_args(
            &tv_args(&["--signal=$(trap)", "cmd"]),
            &ArgSpec::posix(&["--signal"]),
            true,
        );
        assert!(matches!(flag(&r, "--signal"), TriVal::Unknown));
    }

    // ========================================================================
    // Real command patterns
    // ========================================================================

    #[test]
    fn pattern_sudo() {
        let r = parse_args(
            &tv_args(&["-C", "3", "-u", "root", "rm", "-rf", "/"]),
            &ArgSpec::posix(&["-C", "-g", "-r", "-t", "-U", "-D", "-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(flag(&r, "-C"), s("3"));
        assert_eq!(operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn pattern_sudo_grouped() {
        // sudo -iu root rm
        let r = parse_args(
            &tv_args(&["-iu", "root", "rm", "-rf", "/"]),
            &ArgSpec::posix(&["-C", "-g", "-r", "-t", "-U", "-D", "-u"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn pattern_ssh() {
        let r = parse_args(
            &tv_args(&["-p", "22", "-i", "key.pem", "user@host", "ls", "-la"]),
            &ArgSpec::posix(&["-b","-c","-D","-E","-e","-F","-I","-i","-J","-L",
                              "-l","-m","-O","-o","-p","-Q","-R","-S","-W","-w"]),
            true,
        );
        assert_eq!(flag(&r, "-p"), s("22"));
        assert_eq!(flag(&r, "-i"), s("key.pem"));
        assert_eq!(operands(&r), vec!["user@host", "ls", "-la"]);
    }

    #[test]
    fn pattern_ssh_attached_port() {
        let r = parse_args(
            &tv_args(&["-p22", "-i", "key.pem", "host", "ls"]),
            &ArgSpec::posix(&["-p", "-i"]),
            true,
        );
        assert_eq!(flag(&r, "-p"), s("22"));
        assert_eq!(flag(&r, "-i"), s("key.pem"));
        assert_eq!(operands(&r), vec!["host", "ls"]);
    }

    #[test]
    fn pattern_timeout() {
        let r = parse_args(
            &tv_args(&["-s", "KILL", "30", "curl", "example.com"]),
            &ArgSpec::posix(&["-s", "--signal", "-k", "--kill-after"]),
            true,
        );
        assert_eq!(flag(&r, "-s"), s("KILL"));
        assert_eq!(operands(&r), vec!["30", "curl", "example.com"]);
    }

    #[test]
    fn pattern_xargs() {
        let r = parse_args(
            &tv_args(&["-n", "1", "-I", "{}", "rm", "-v"]),
            &ArgSpec::posix(&["-d", "-I", "-L", "-n", "-P", "-s", "-E"]),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("1"));
        assert_eq!(flag(&r, "-I"), s("{}"));
        assert_eq!(operands(&r), vec!["rm", "-v"]);
    }

    #[test]
    fn pattern_find() {
        let mut spec = ArgSpec::posix(&[]);
        for flag in ["-exec", "-execdir", "-ok", "-okdir"] {
            spec.terminated.insert(flag.into(), vec![";".into(), "+".into()]);
        }
        let r = parse_args(
            &tv_args(&[".", "-name", "*.rs", "-exec", "grep", "TODO", "{}", ";"]),
            &spec,
            true,
        );
        assert_eq!(operands(&r), vec![".", "-name", "*.rs"]);
        assert_eq!(terminated(&r, "-exec"), vec![vec!["grep", "TODO", "{}"]]);
    }

    #[test]
    fn pattern_kubectl() {
        let r = parse_args(
            &tv_args(&["pod", "-c", "container", "--", "ls", "-la"]),
            &ArgSpec::gnu(&["-n", "-c", "--namespace", "--container"]),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("container"));
        assert_eq!(operands(&r), vec!["pod", "ls", "-la"]);
    }

    #[test]
    fn pattern_docker_exec() {
        let r = parse_args(
            &tv_args(&["-u", "root", "-w", "/app", "mycontainer", "bash"]),
            &ArgSpec::gnu(&["-e", "--env", "-u", "--user", "-w", "--workdir"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(flag(&r, "-w"), s("/app"));
        assert_eq!(operands(&r), vec!["mycontainer", "bash"]);
    }

    #[test]
    fn pattern_env() {
        let r = parse_args(
            &tv_args(&["-u", "FOO", "cmd", "arg"]),
            &ArgSpec::posix(&["-u", "-S"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("FOO"));
        assert_eq!(operands(&r), vec!["cmd", "arg"]);
    }

    // ========================================================================
    // Accessor methods
    // ========================================================================

    #[test]
    fn operands_from_skips() {
        let r = parse_args(
            &tv_args(&["host", "ls", "-la"]),
            &ArgSpec::posix(&[]),
            true,
        );
        let from1: Vec<String> = r.operands_from(1).iter().filter_map(|e| {
            if let TriVal::String(s) = e { Some(s.clone()) } else { None }
        }).collect();
        assert_eq!(from1, vec!["ls", "-la"]);
    }

    #[test]
    fn operands_from_beyond_length() {
        let r = parse_args(&tv_args(&["a"]), &ArgSpec::posix(&[]), true);
        assert!(r.operands_from(5).is_empty());
    }

    #[test]
    fn positional_access() {
        let r = parse_args(
            &tv_args(&["host", "cmd"]),
            &ArgSpec::posix(&[]),
            true,
        );
        assert_eq!(r.positional(0), s("host"));
        assert_eq!(r.positional(1), s("cmd"));
        assert!(matches!(r.positional(2), TriVal::Null));
    }

    // ========================================================================
    // Fuzzer: compare against C getopt
    // ========================================================================

    fn compile_getopt_ref() -> std::path::PathBuf {
        use std::sync::Once;
        static COMPILE: Once = Once::new();
        let bin = std::env::temp_dir().join("tmux_mcp_getopt_ref");
        COMPILE.call_once(|| {
            let _ = std::fs::remove_file(&bin);
            let src = r#"
#include <stdio.h>
#include <unistd.h>
#include <string.h>

// Usage: getopt_ref <mode> <optstring> -- <args...>
// mode: "posix" or "gnu"
int main(int argc, char *argv[]) {
    if (argc < 4) {
        fprintf(stderr, "usage: getopt_ref <mode> <optstring> -- <args...>\n");
        return 1;
    }
    char *mode = argv[1];
    char optstring[256];
    // Prefix with '+' for POSIX mode
    if (strcmp(mode, "posix") == 0) {
        optstring[0] = '+';
        strncpy(optstring + 1, argv[2], sizeof(optstring) - 2);
    } else {
        strncpy(optstring, argv[2], sizeof(optstring) - 1);
    }
    optstring[sizeof(optstring) - 1] = '\0';

    // Find the -- separator
    int sep = -1;
    for (int i = 3; i < argc; i++) {
        if (strcmp(argv[i], "--") == 0) { sep = i; break; }
    }
    if (sep < 0) { fprintf(stderr, "missing -- separator\n"); return 1; }

    int fake_argc = argc - sep;
    char *fake_argv[256];
    fake_argv[0] = "cmd";
    for (int i = sep + 1; i < argc; i++) {
        fake_argv[i - sep] = argv[i];
    }

    optind = 1;
    opterr = 0;
    int c;
    while ((c = getopt(fake_argc, fake_argv, optstring)) != -1) {
        if (c == '?') {
            printf("UNKNOWN=%c\n", optopt);
        } else {
            printf("FLAG=%c", c);
            if (optarg) printf(" VALUE=%s", optarg);
            printf("\n");
        }
    }
    for (int i = optind; i < fake_argc; i++) {
        printf("OPERAND=%s\n", fake_argv[i]);
    }
    return 0;
}
"#;
            let src_path = std::env::temp_dir().join("tmux_mcp_getopt_ref.c");
            std::fs::write(&src_path, src).expect("write C source");
            let status = std::process::Command::new("cc")
                .args(["-o", bin.to_str().unwrap(), src_path.to_str().unwrap()])
                .status()
                .expect("cc failed");
            assert!(status.success(), "failed to compile getopt reference");
        });
        bin
    }

    struct RefResult {
        flags: Vec<(char, Option<String>)>,
        operands: Vec<String>,
    }

    fn run_ref(mode: &str, optstring: &str, argv: &[&str]) -> RefResult {
        let bin = compile_getopt_ref();
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg(mode).arg(optstring).arg("--");
        for a in argv { cmd.arg(a); }
        let output = cmd.output().expect("run getopt_ref");
        assert!(output.status.success(), "getopt_ref failed: {}",
            String::from_utf8_lossy(&output.stderr));

        let stdout = String::from_utf8(output.stdout).unwrap();
        let mut flags = Vec::new();
        let mut operands = Vec::new();
        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("FLAG=") {
                let mut parts = rest.splitn(2, " VALUE=");
                let ch = parts.next().unwrap().chars().next().unwrap();
                let val = parts.next().map(|s| s.to_string());
                flags.push((ch, val));
            } else if let Some(rest) = line.strip_prefix("OPERAND=") {
                operands.push(rest.to_string());
            }
        }
        RefResult { flags, operands }
    }

    fn optstring_to_valued(optstring: &str) -> Vec<String> {
        let mut valued = Vec::new();
        let chars: Vec<char> = optstring.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if i + 1 < chars.len() && chars[i + 1] == ':' {
                valued.push(format!("-{}", chars[i]));
                i += 2;
            } else {
                i += 1;
            }
        }
        valued
    }

    fn compare(mode: &str, optstring: &str, argv: &[&str]) {
        let ref_result = run_ref(mode, optstring, argv);
        let valued = optstring_to_valued(optstring);
        let style = if mode == "posix" { ArgStyle::Posix } else { ArgStyle::Gnu };
        let spec = ArgSpec { style, valued, terminated: HashMap::new() };
        let our = parse_args(&tv_args(argv), &spec, true);

        let our_ops = operands(&our);
        assert_eq!(
            our_ops, ref_result.operands,
            "OPERANDS differ for mode={} optstring={:?} argv={:?}\n  ours: {:?}\n  ref:  {:?}",
            mode, optstring, argv, our_ops, ref_result.operands,
        );

        // Track how many times each flag appears in C output.
        // We keep first occurrence; C keeps last. Only compare non-repeated flags.
        let mut flag_counts: HashMap<char, usize> = HashMap::new();
        for (ch, _) in &ref_result.flags {
            *flag_counts.entry(*ch).or_insert(0) += 1;
        }

        for (ch, ref_val) in &ref_result.flags {
            if flag_counts[ch] > 1 {
                continue; // Skip repeated flags — we intentionally keep first, C keeps last
            }
            let flag_name = format!("-{}", ch);
            let our_val = flag(&our, &flag_name);
            if let Some(rv) = ref_val {
                assert_eq!(
                    our_val, s(rv),
                    "FLAG -{} value differs for mode={} optstring={:?} argv={:?}",
                    ch, mode, optstring, argv,
                );
            }
        }
    }

    #[test]
    fn fuzz_posix_predefined() {
        let cases: Vec<(&str, Vec<&str>)> = vec![
            ("ab:c:", vec!["-a", "-b", "val", "-c", "val2", "op1", "op2"]),
            ("ab:c", vec!["-ac", "-b", "val", "op"]),
            ("f:", vec!["-fvalue", "op"]),
            ("ab:", vec!["-abval", "op"]),
            ("a", vec!["-a", "--", "-a", "op"]),
            ("a", vec!["-a", "-", "op"]),
            ("a", vec!["-x", "op"]),
            ("f:", vec!["-f", "-x", "op"]),
            ("", vec!["op1", "op2"]),
            ("abc", vec!["-a", "-b", "-c"]),
            ("a", vec!["op", "-a"]),
            ("ab:c", vec!["-ab", "val", "-c", "op"]),
            ("a:b:", vec!["-ab", "val", "op"]),
            ("p:", vec!["-p22", "host"]),
            ("abc:", vec![]),
            ("a", vec!["--"]),
            ("ab", vec!["--", "-a", "-b"]),
            ("f:", vec!["-f"]),
            ("a", vec!["-a"]),
        ];
        for (optstring, argv) in &cases {
            compare("posix", optstring, argv);
        }
    }

    #[test]
    fn fuzz_gnu_predefined() {
        let cases: Vec<(&str, Vec<&str>)> = vec![
            ("a", vec!["op", "-a"]),
            ("a:b", vec!["op1", "-a", "val", "-b", "op2"]),
            ("a", vec!["-a", "op", "--", "-a"]),
            ("f:", vec!["op", "-f", "val", "op2"]),
        ];
        for (optstring, argv) in &cases {
            compare("gnu", optstring, argv);
        }
    }

    #[test]
    fn fuzz_posix_random() {
        let flag_chars: Vec<char> = "abcdefghijklnoprstuvwxyz".chars().collect();
        let mut state: u32 = 0xDEAD_BEEF;
        let mut rand = || -> u32 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };

        for _ in 0..200 {
            let n_opts = (rand() % 5 + 2) as usize;
            let mut optstring = String::new();
            let mut used = std::collections::HashSet::new();
            for _ in 0..n_opts {
                let ch = flag_chars[rand() as usize % flag_chars.len()];
                if used.contains(&ch) { continue; }
                used.insert(ch);
                optstring.push(ch);
                if rand() % 3 == 0 { optstring.push(':'); }
            }

            let n_args = (rand() % 8 + 1) as usize;
            let mut argv: Vec<String> = Vec::new();
            for _ in 0..n_args {
                match rand() % 10 {
                    0..=2 => {
                        let ch = flag_chars[rand() as usize % flag_chars.len()];
                        argv.push(format!("-{}", ch));
                    }
                    3 => {
                        let n = (rand() % 2 + 2) as usize;
                        let mut group = String::from("-");
                        for _ in 0..n { group.push(flag_chars[rand() as usize % flag_chars.len()]); }
                        argv.push(group);
                    }
                    4 => argv.push("--".into()),
                    5 => argv.push("-".into()),
                    6 => {
                        let ch = flag_chars[rand() as usize % flag_chars.len()];
                        argv.push(format!("-{}val{}", ch, rand() % 100));
                    }
                    _ => argv.push(format!("op{}", rand() % 100)),
                }
            }

            let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            compare("posix", &optstring, &argv_refs);
        }
    }

    #[test]
    fn fuzz_gnu_random() {
        let flag_chars: Vec<char> = "abcdefghijklnoprstuvwxyz".chars().collect();
        let mut state: u32 = 0xBEEF_CAFE;
        let mut rand = || -> u32 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };

        for _ in 0..200 {
            let n_opts = (rand() % 5 + 2) as usize;
            let mut optstring = String::new();
            let mut used = std::collections::HashSet::new();
            for _ in 0..n_opts {
                let ch = flag_chars[rand() as usize % flag_chars.len()];
                if used.contains(&ch) { continue; }
                used.insert(ch);
                optstring.push(ch);
                if rand() % 3 == 0 { optstring.push(':'); }
            }

            let n_args = (rand() % 8 + 1) as usize;
            let mut argv: Vec<String> = Vec::new();
            for _ in 0..n_args {
                match rand() % 10 {
                    0..=2 => {
                        let ch = flag_chars[rand() as usize % flag_chars.len()];
                        argv.push(format!("-{}", ch));
                    }
                    3 => {
                        let n = (rand() % 2 + 2) as usize;
                        let mut group = String::from("-");
                        for _ in 0..n { group.push(flag_chars[rand() as usize % flag_chars.len()]); }
                        argv.push(group);
                    }
                    4 => argv.push("--".into()),
                    5 => argv.push("-".into()),
                    6 => {
                        let ch = flag_chars[rand() as usize % flag_chars.len()];
                        argv.push(format!("-{}val{}", ch, rand() % 100));
                    }
                    _ => argv.push(format!("op{}", rand() % 100)),
                }
            }

            let argv_refs: Vec<&str> = argv.iter().map(|s| s.as_str()).collect();
            compare("gnu", &optstring, &argv_refs);
        }
    }
}
