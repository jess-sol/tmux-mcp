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
pub use super::rules::TriVal;

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

/// A known option definition, matching C's struct option.
/// Short (1 char name) → `-x`, groupable, attachable value.
/// Long (2+ char name) → `--flag`, `--flag=val`.
#[derive(Debug, Clone)]
pub struct OptDef {
    pub name: String,
    pub has_arg: bool,
}

/// Specification for how to parse a command's arguments.
#[derive(Debug, Clone)]
pub struct ArgSpec {
    pub style: ArgStyle,
    pub options: Vec<OptDef>,
}

impl ArgSpec {
    /// Build from a C-style optstring: "isC:D:u:" → i, s standalone; C, D, u take values.
    pub fn from_optstring(style: ArgStyle, optstring: &str) -> Self {
        Self { style, options: parse_optstring(optstring) }
    }

    /// Build with short optstring + long options: long = ["signal:", "kill-after:"]
    pub fn from_optstring_long(style: ArgStyle, optstring: &str, long: &[&str]) -> Self {
        let mut options = parse_optstring(optstring);
        options.extend(parse_long_opts(long));
        Self { style, options }
    }

    /// Shorthand: POSIX mode from optstring. `ArgSpec::posix("u:C:")`
    pub fn posix(optstring: &str) -> Self {
        Self::from_optstring(ArgStyle::Posix, optstring)
    }

    /// Shorthand: GNU mode from optstring. `ArgSpec::gnu("n:c:")`
    pub fn gnu(optstring: &str) -> Self {
        Self::from_optstring(ArgStyle::Gnu, optstring)
    }

    /// Check if a flag name is a known option (short or long).
    fn find_opt(&self, name: &str) -> Option<&OptDef> {
        self.options.iter().find(|o| o.name == name)
    }

    /// Is this flag name known as a valued option?
    fn is_valued(&self, name: &str) -> bool {
        self.find_opt(name).is_some_and(|o| o.has_arg)
    }

    /// Is this flag name known at all?
    fn is_known(&self, name: &str) -> bool {
        self.find_opt(name).is_some()
    }
}

/// Parse C optstring: "isC:D:u:" → OptDefs
fn parse_optstring(s: &str) -> Vec<OptDef> {
    let chars: Vec<char> = s.chars().collect();
    let mut opts = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let has_arg = i + 1 < chars.len() && chars[i + 1] == ':';
        opts.push(OptDef { name: chars[i].to_string(), has_arg });
        i += if has_arg { 2 } else { 1 };
    }
    opts
}

/// Parse long options: ["signal:", "kill-after:"] → OptDefs
fn parse_long_opts(list: &[&str]) -> Vec<OptDef> {
    list.iter().map(|s| {
        if let Some(name) = s.strip_suffix(':') {
            OptDef { name: name.to_string(), has_arg: true }
        } else {
            OptDef { name: s.to_string(), has_arg: false }
        }
    }).collect()
}

/// Result of parsing arguments.
#[derive(Debug)]
pub struct ParsedArgs {
    /// Non-option arguments, in order.
    pub operands: Vec<TriVal>,
    /// Flag values: flag name → value. Only flags in the valued list appear here.
    pub flags: HashMap<String, TriVal>,
    /// False if unknown flags or other uncertainty degraded confidence in the parse.
    /// When false, absent flags/positionals should be treated as Unknown, not Null.
    pub exhaustive: bool,
}

impl ParsedArgs {
    /// Value of a flag, or Null (exhaustive) / Unknown (non-exhaustive) if absent.
    /// Accepts either bare name ("u") or dashed form ("-u", "--signal").
    pub fn value(&self, flag: &str) -> TriVal {
        let name = flag.trim_start_matches('-');
        self.flags.get(name).cloned().unwrap_or_else(|| self.absent())
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

    fn absent(&self) -> TriVal {
        if self.exhaustive { TriVal::Null } else { TriVal::Unknown }
    }
}

/// Parse arguments according to the given spec.
pub fn parse_args(args: &[TriVal], spec: &ArgSpec, exhaustive: bool) -> ParsedArgs {
    let mut result = ParsedArgs {
        operands: Vec::new(),
        flags: HashMap::new(),
        exhaustive,
    };
    let mut past_options = false;

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
                let opt_name = if let Some(eq_pos) = arg_str.find('=') {
                    &arg_str[2..eq_pos]
                } else {
                    &arg_str[2..]
                };

                if let Some(eq_pos) = arg_str.find('=') {
                    // --flag=value form
                    if spec.is_valued(opt_name) {
                        let val_str = &arg_str[eq_pos + 1..];
                        result.flags.entry(opt_name.to_string()).or_insert_with(|| str_to_trival(val_str));
                    }
                    // --unknown=val is self-contained; doesn't affect exhaustiveness
                    i += 1;
                } else if spec.is_valued(opt_name) {
                    // --flag value (separate)
                    result.flags.entry(opt_name.to_string()).or_insert_with(|| consume_value(args, i + 1));
                    i += 2;
                } else if spec.is_known(opt_name) {
                    // Known standalone long flag
                    i += 1;
                } else {
                    // Unknown long flag without = — might take a value
                    result.exhaustive = false;
                    i += 1;
                }
                continue;
            }

            // Short option: first check full string as multi-char valued (find's -name)
            let full_name = &arg_str[1..];
            if full_name.len() > 1 && spec.is_valued(full_name) {
                result.flags.entry(full_name.to_string()).or_insert_with(|| consume_value(args, i + 1));
                i += 2;
                continue;
            }

            // POSIX short option group: -abc
            let chars: Vec<char> = arg_str[1..].chars().collect();
            let mut ci = 0;
            let mut consumed_next = false;
            let mut all_known = true;
            while ci < chars.len() {
                let ch = chars[ci].to_string();
                if spec.is_valued(&ch) {
                    if ci + 1 < chars.len() {
                        // Attached: rest of string is value
                        let val_str: String = chars[ci + 1..].iter().collect();
                        result.flags.entry(ch).or_insert_with(|| str_to_trival(&val_str));
                    } else {
                        // Separate: next arg is value
                        result.flags.entry(ch).or_insert_with(|| consume_value(args, i + 1));
                        consumed_next = true;
                    }
                    break;
                } else if !spec.is_known(&ch) {
                    all_known = false;
                }
                ci += 1;
            }
            if !all_known {
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


    // ========================================================================
    // POSIX mode
    // ========================================================================

    #[test]
    fn posix_empty_args() {
        let r = parse_args(&[], &ArgSpec::posix(""), true);
        assert!(operands(&r).is_empty());
        assert!(r.exhaustive);
    }

    #[test]
    fn posix_all_operands() {
        let r = parse_args(&tv_args(&["ls", "-la", "/"]), &ArgSpec::posix(""), true);
        assert_eq!(operands(&r), vec!["ls", "-la", "/"]);
    }

    #[test]
    fn posix_valued_flag_separate() {
        let r = parse_args(&tv_args(&["-n", "10", "cmd"]), &ArgSpec::posix("n:"), true);
        assert_eq!(flag(&r, "-n"), s("10"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_valued_flag_attached() {
        // -fvalue: -f takes "value" (attached, POSIX Guideline 7 form 2)
        let r = parse_args(&tv_args(&["-fvalue", "cmd"]), &ArgSpec::posix("f:"), true);
        assert_eq!(flag(&r, "-f"), s("value"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_grouped_boolean_flags() {
        // -abc: three standalone flags, no valued
        let r = parse_args(&tv_args(&["-abc", "file"]), &ArgSpec::posix(""), true);
        assert_eq!(operands(&r), vec!["file"]);
    }

    #[test]
    fn posix_grouped_valued_last() {
        // -au root: -a standalone, -u valued (last char, separate value)
        let r = parse_args(
            &tv_args(&["-au", "root", "cmd"]),
            &ArgSpec::posix("u:"),
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
            &ArgSpec::posix("u:"),
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
            &ArgSpec::posix("u:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_bare_dash_is_operand() {
        // "-" is not an option per POSIX spec
        let r = parse_args(&tv_args(&["-v", "-", "file"]), &ArgSpec::posix(""), true);
        assert_eq!(operands(&r), vec!["-", "file"]);
    }

    #[test]
    fn posix_bare_dash_alone() {
        let r = parse_args(&tv_args(&["-"]), &ArgSpec::posix(""), true);
        assert_eq!(operands(&r), vec!["-"]);
    }

    #[test]
    fn posix_double_dash_ends_options() {
        let r = parse_args(
            &tv_args(&["-n", "5", "--", "-rf", "file"]),
            &ArgSpec::posix("n:"),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("5"));
        assert_eq!(operands(&r), vec!["-rf", "file"]);
    }

    #[test]
    fn posix_double_dash_only() {
        let r = parse_args(&tv_args(&["--"]), &ArgSpec::posix(""), true);
        assert!(operands(&r).is_empty());
    }

    #[test]
    fn posix_double_dash_before_any_flags() {
        let r = parse_args(
            &tv_args(&["--", "-n", "10", "cmd"]),
            &ArgSpec::posix("n:"),
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
            &ArgSpec::posix("u:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn posix_flags_after_operand_are_operands() {
        let r = parse_args(
            &tv_args(&["cmd", "-n", "10"]),
            &ArgSpec::posix("n:"),
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
            &ArgSpec::posix(""),
            true,
        );
        assert_eq!(operands(&r), vec!["cmd", "--", "-rf"]);
    }

    #[test]
    fn posix_valued_flag_missing_value() {
        let r = parse_args(&tv_args(&["-u"]), &ArgSpec::posix("u:"), true);
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
    }

    #[test]
    fn posix_valued_flag_value_looks_like_flag() {
        // -u takes next arg even if it starts with -
        let r = parse_args(
            &tv_args(&["-u", "-1", "cmd"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("-1"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_long_flag_separate() {
        let r = parse_args(
            &tv_args(&["--signal", "KILL", "cmd"]),
            &ArgSpec::from_optstring_long(ArgStyle::Posix, "", &["signal:"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_long_flag_equals() {
        let r = parse_args(
            &tv_args(&["--signal=KILL", "cmd"]),
            &ArgSpec::from_optstring_long(ArgStyle::Posix, "", &["signal:"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn posix_multi_char_single_dash() {
        // find-style: -name is a multi-char valued option (uses long option list)
        let r = parse_args(
            &tv_args(&["-name", "*.rs", "."]),
            &ArgSpec::from_optstring_long(ArgStyle::Posix, "", &["name:"]),
            true,
        );
        assert_eq!(flag(&r, "name"), s("*.rs"));
        assert_eq!(operands(&r), vec!["."]);
    }

    #[test]
    fn posix_multiple_valued_flags() {
        let r = parse_args(
            &tv_args(&["-u", "root", "-C", "3", "rm", "-rf"]),
            &ArgSpec::posix("u:C:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(flag(&r, "-C"), s("3"));
        assert_eq!(operands(&r), vec!["rm", "-rf"]);
    }

    #[test]
    fn posix_absent_flag_is_null() {
        let r = parse_args(&tv_args(&["cmd"]), &ArgSpec::posix("u:"), true);
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
            &ArgSpec::posix("o:"),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_grouped_separate() {
        let r = parse_args(
            &tv_args(&["-ao", "arg", "path1", "path2"]),
            &ArgSpec::posix("o:"),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_reordered() {
        let r = parse_args(
            &tv_args(&["-o", "arg", "-a", "path1", "path2"]),
            &ArgSpec::posix("o:"),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_double_dash() {
        let r = parse_args(
            &tv_args(&["-a", "-o", "arg", "--", "path1", "path2"]),
            &ArgSpec::posix("o:"),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_attached() {
        let r = parse_args(
            &tv_args(&["-a", "-oarg", "path1", "path2"]),
            &ArgSpec::posix("o:"),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["path1", "path2"]);
    }

    #[test]
    fn posix_spec_grouped_attached() {
        let r = parse_args(
            &tv_args(&["-aoarg", "path1", "path2"]),
            &ArgSpec::posix("o:"),
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
            &ArgSpec::gnu("c:"),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("container"));
        assert_eq!(operands(&r), vec!["pod", "extra"]);
    }

    #[test]
    fn gnu_double_dash_stops() {
        let r = parse_args(
            &tv_args(&["-c", "container", "pod", "--", "ls", "-la"]),
            &ArgSpec::gnu("c:"),
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
            &ArgSpec::gnu(""),
            true,
        );
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    #[test]
    fn gnu_valued_after_operand() {
        let r = parse_args(
            &tv_args(&["op1", "-n", "5", "op2"]),
            &ArgSpec::gnu("n:"),
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
            &ArgSpec::gnu("u:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn gnu_long_flag_equals() {
        let r = parse_args(
            &tv_args(&["op", "--signal=KILL", "op2"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "", &["signal:"]),
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
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "n:c:", &["namespace:", "container:"]),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("container"));
        assert_eq!(operands(&r), vec!["pod", "ls", "-la"]);
    }

    #[test]
    fn gnu_bare_dash_is_operand() {
        let r = parse_args(&tv_args(&["-v", "-", "file"]), &ArgSpec::gnu(""), true);
        assert_eq!(operands(&r), vec!["-", "file"]);
    }

    #[test]
    fn gnu_double_dash_after_operand_still_separates() {
        // GNU mode: -- is always recognized as separator, even after operands
        let r = parse_args(
            &tv_args(&["op", "--", "-a"]),
            &ArgSpec::gnu(""),
            true,
        );
        assert_eq!(operands(&r), vec!["op", "-a"]);
    }

    #[test]
    fn gnu_double_dash_between_flags() {
        let r = parse_args(
            &tv_args(&["-a", "--", "-b"]),
            &ArgSpec::gnu(""),
            true,
        );
        // -a is a flag, -- ends options, -b is an operand
        assert_eq!(operands(&r), vec!["-b"]);
    }

    #[test]
    fn gnu_missing_value() {
        let r = parse_args(&tv_args(&["-u"]), &ArgSpec::gnu("u:"), true);
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
    }

    #[test]
    fn gnu_missing_value_after_operand() {
        let r = parse_args(
            &tv_args(&["op", "-u"]),
            &ArgSpec::gnu("u:"),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
    }

    #[test]
    fn gnu_attached_value() {
        let r = parse_args(
            &tv_args(&["op", "-fvalue", "op2"]),
            &ArgSpec::gnu("f:"),
            true,
        );
        assert_eq!(flag(&r, "-f"), s("value"));
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    #[test]
    fn gnu_grouped_attached() {
        let r = parse_args(
            &tv_args(&["op", "-aoarg", "op2"]),
            &ArgSpec::gnu("o:"),
            true,
        );
        assert_eq!(flag(&r, "-o"), s("arg"));
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    #[test]
    fn gnu_multi_char_single_dash() {
        let r = parse_args(
            &tv_args(&["op", "-name", "*.rs", "op2"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "", &["name:"]),
            true,
        );
        assert_eq!(flag(&r, "name"), s("*.rs"));
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    #[test]
    fn gnu_multiple_valued_flags() {
        let r = parse_args(
            &tv_args(&["op1", "-u", "root", "op2", "-C", "3"]),
            &ArgSpec::gnu("u:C:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(flag(&r, "-C"), s("3"));
        assert_eq!(operands(&r), vec!["op1", "op2"]);
    }

    #[test]
    fn gnu_absent_flag_is_null() {
        let r = parse_args(&tv_args(&["op"]), &ArgSpec::gnu("u:"), true);
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
    }

    #[test]
    fn gnu_long_flag_separate_after_operand() {
        let r = parse_args(
            &tv_args(&["op", "--signal", "KILL", "op2"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "", &["signal:"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    #[test]
    fn gnu_long_flag_equals_after_operand() {
        let r = parse_args(
            &tv_args(&["op", "--signal=KILL", "op2"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "", &["signal:"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(operands(&r), vec!["op", "op2"]);
    }

    // GNU exhaustiveness

    #[test]
    fn gnu_exhaustive_all_known() {
        let r = parse_args(
            &tv_args(&["op", "-u", "root"]),
            &ArgSpec::gnu("u:"),
            true,
        );
        assert!(r.exhaustive);
    }

    #[test]
    fn gnu_exhaustive_unknown_flag_after_operand() {
        let r = parse_args(
            &tv_args(&["op", "-x", "op2"]),
            &ArgSpec::gnu(""),
            true,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn gnu_exhaustive_unknown_long_flag() {
        let r = parse_args(
            &tv_args(&["op", "--unknown", "op2"]),
            &ArgSpec::gnu(""),
            true,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn gnu_exhaustive_unknown_long_with_equals() {
        let r = parse_args(
            &tv_args(&["op", "--unknown=val", "op2"]),
            &ArgSpec::gnu(""),
            true,
        );
        assert!(r.exhaustive);
    }

    #[test]
    fn gnu_exhaustive_absent_flag_unknown_when_non_exhaustive() {
        let r = parse_args(
            &tv_args(&["op", "-x"]),
            &ArgSpec::gnu("u:"),
            true,
        );
        assert!(!r.exhaustive);
        assert!(matches!(flag(&r, "-u"), TriVal::Unknown));
    }

    // GNU expansion detection

    #[test]
    fn gnu_expansion_flag_value() {
        let r = parse_args(
            &tv_args(&["op", "-u", "$(whoami)"]),
            &ArgSpec::gnu("u:"),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Unknown));
    }

    #[test]
    fn gnu_expansion_attached_value() {
        let r = parse_args(
            &tv_args(&["op", "-u$(id)"]),
            &ArgSpec::gnu("u:"),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Unknown));
    }

    #[test]
    fn gnu_expansion_long_equals() {
        let r = parse_args(
            &tv_args(&["op", "--user=$(id)"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "", &["user:"]),
            true,
        );
        assert!(matches!(flag(&r, "--user"), TriVal::Unknown));
    }


    // GNU docker exec pattern (more realistic)

    #[test]
    fn gnu_docker_exec_flags_interspersed() {
        // docker exec -it mycontainer -u root bash
        // -i and -t are standalone flags (unknown but skipped), -u is valued
        let r = parse_args(
            &tv_args(&["-it", "mycontainer", "-u", "root", "bash"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "u:e:w:", &["user:", "env:", "workdir:"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["mycontainer", "bash"]);
        assert!(!r.exhaustive); // -i and -t are unknown flags
    }
    // ========================================================================
    // Exhaustiveness / three-valued logic
    // ========================================================================

    #[test]
    fn exhaustive_all_known() {
        let r = parse_args(
            &tv_args(&["-u", "root", "cmd"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert!(r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_short_flag() {
        // -x not in valued list — might consume next arg
        let r = parse_args(
            &tv_args(&["-x", "cmd"]),
            &ArgSpec::posix(""),
            true,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_long_flag() {
        let r = parse_args(
            &tv_args(&["--unknown", "cmd"]),
            &ArgSpec::posix(""),
            true,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_long_with_equals() {
        // --unknown=val is self-contained, doesn't affect boundaries
        let r = parse_args(
            &tv_args(&["--unknown=val", "cmd"]),
            &ArgSpec::posix(""),
            true,
        );
        assert!(r.exhaustive);
    }

    #[test]
    fn exhaustive_unknown_trival() {
        let mut a = tv_args(&["-u", "root"]);
        a.push(TriVal::Unknown);
        a.push(s("cmd"));
        let r = parse_args(&a, &ArgSpec::posix("u:"), true);
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_input_propagates() {
        let r = parse_args(
            &tv_args(&["-u", "root", "cmd"]),
            &ArgSpec::posix("u:"),
            false,
        );
        assert!(!r.exhaustive);
    }

    #[test]
    fn exhaustive_absent_flag_null() {
        let r = parse_args(
            &tv_args(&["cmd"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert!(matches!(r.value("-u"), TriVal::Null));
    }

    #[test]
    fn exhaustive_absent_flag_unknown_when_non_exhaustive() {
        let r = parse_args(
            &tv_args(&["-x", "cmd"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert!(!r.exhaustive);
        assert!(matches!(r.value("-u"), TriVal::Unknown));
    }

    #[test]
    fn exhaustive_absent_positional() {
        let r = parse_args(&tv_args(&["a"]), &ArgSpec::posix(""), true);
        assert!(matches!(r.positional(5), TriVal::Null));
    }

    #[test]
    fn exhaustive_absent_positional_when_non_exhaustive() {
        let r = parse_args(&tv_args(&["a"]), &ArgSpec::posix(""), false);
        assert!(matches!(r.positional(5), TriVal::Unknown));
    }

    // ========================================================================
    // Auto-unknown (expansion detection)
    // ========================================================================

    #[test]
    fn expansion_flag_value_is_unknown() {
        let r = parse_args(
            &tv_args(&["-u", "$(whoami)", "cmd"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Unknown));
        assert_eq!(operands(&r), vec!["cmd"]);
    }

    #[test]
    fn expansion_operand_is_unknown() {
        let r = parse_args(&tv_args(&["$HOME/file"]), &ArgSpec::posix(""), true);
        assert!(matches!(r.operands[0], TriVal::Unknown));
    }

    #[test]
    fn expansion_attached_value() {
        let r = parse_args(
            &tv_args(&["-u$(id)", "cmd"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Unknown));
    }

    #[test]
    fn expansion_long_equals() {
        let r = parse_args(
            &tv_args(&["--signal=$(trap)", "cmd"]),
            &ArgSpec::from_optstring_long(ArgStyle::Posix, "", &["signal:"]),
            true,
        );
        assert!(matches!(flag(&r, "--signal"), TriVal::Unknown));
    }

    // ========================================================================
    // Real command patterns — one for every wrapper we support
    // ========================================================================

    // --- Transparent (no getopt, all args are inner command) ---

    #[test]
    fn pattern_command_builtin() {
        // command/builtin: all args are the inner command, no flag processing
        let r = parse_args(&tv_args(&["ls", "-la", "/"]), &ArgSpec::posix(""), true);
        assert_eq!(operands(&r), vec!["ls", "-la", "/"]);
    }

    #[test]
    fn pattern_nohup() {
        let r = parse_args(&tv_args(&["make", "-j4"]), &ArgSpec::posix(""), true);
        assert_eq!(operands(&r), vec!["make", "-j4"]);
    }

    // --- POSIX wrappers ---

    #[test]
    fn pattern_nice() {
        let r = parse_args(
            &tv_args(&["-n", "10", "cargo", "build"]),
            &ArgSpec::posix("n:"),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("10"));
        assert_eq!(operands(&r), vec!["cargo", "build"]);
    }

    #[test]
    fn pattern_strace() {
        let r = parse_args(
            &tv_args(&["-e", "trace=open", "-o", "/tmp/trace.log", "ls", "-la"]),
            &ArgSpec::posix("e:s:o:p:"),
            true,
        );
        assert_eq!(flag(&r, "-e"), s("trace=open"));
        assert_eq!(flag(&r, "-o"), s("/tmp/trace.log"));
        assert_eq!(operands(&r), vec!["ls", "-la"]);
    }

    #[test]
    fn pattern_watch() {
        let r = parse_args(
            &tv_args(&["-n", "2", "df", "-h"]),
            &ArgSpec::posix("n:"),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("2"));
        assert_eq!(operands(&r), vec!["df", "-h"]);
    }

    #[test]
    fn pattern_timeout() {
        let r = parse_args(
            &tv_args(&["-s", "KILL", "30", "curl", "example.com"]),
            &ArgSpec::from_optstring_long(ArgStyle::Posix, "s:k:", &["signal:", "kill-after:"]),
            true,
        );
        assert_eq!(flag(&r, "-s"), s("KILL"));
        // 30 is first operand (duration), then inner command
        assert_eq!(operands(&r), vec!["30", "curl", "example.com"]);
    }

    #[test]
    fn pattern_timeout_long_flag() {
        let r = parse_args(
            &tv_args(&["--signal=KILL", "--kill-after", "5", "30", "cmd"]),
            &ArgSpec::from_optstring_long(ArgStyle::Posix, "s:k:", &["signal:", "kill-after:"]),
            true,
        );
        assert_eq!(flag(&r, "--signal"), s("KILL"));
        assert_eq!(flag(&r, "--kill-after"), s("5"));
        assert_eq!(operands(&r), vec!["30", "cmd"]);
    }

    #[test]
    fn pattern_sudo() {
        let r = parse_args(
            &tv_args(&["-C", "3", "-u", "root", "rm", "-rf", "/"]),
            &ArgSpec::posix("C:g:r:t:U:D:u:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(flag(&r, "-C"), s("3"));
        assert_eq!(operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn pattern_sudo_grouped() {
        // sudo -iu root rm — common real-world pattern
        let r = parse_args(
            &tv_args(&["-iu", "root", "rm", "-rf", "/"]),
            &ArgSpec::posix("C:g:r:t:U:D:u:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn pattern_sudo_no_user() {
        // sudo without -u defaults to root (handled by CEL, not getopt)
        let r = parse_args(
            &tv_args(&["rm", "-rf", "/"]),
            &ArgSpec::posix("C:g:r:t:U:D:u:"),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
        assert_eq!(operands(&r), vec!["rm", "-rf", "/"]);
    }

    #[test]
    fn pattern_doas() {
        let r = parse_args(
            &tv_args(&["-u", "www", "nginx", "-t"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("www"));
        assert_eq!(operands(&r), vec!["nginx", "-t"]);
    }

    #[test]
    fn pattern_doas_no_user() {
        let r = parse_args(
            &tv_args(&["reboot"]),
            &ArgSpec::posix("u:"),
            true,
        );
        assert!(matches!(flag(&r, "-u"), TriVal::Null));
        assert_eq!(operands(&r), vec!["reboot"]);
    }

    #[test]
    fn pattern_su() {
        // su -c "cmd string" user — -c takes the command string
        let r = parse_args(
            &tv_args(&["-c", "echo hello", "root"]),
            &ArgSpec::posix("c:"),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("echo hello"));
        assert_eq!(operands(&r), vec!["root"]);
    }

    #[test]
    fn pattern_su_login() {
        // su - root -c "cmd" — the - is a standalone flag (login shell)
        let r = parse_args(
            &tv_args(&["-", "root", "-c", "whoami"]),
            &ArgSpec::posix("c:"),
            true,
        );
        // "-" is an operand per POSIX, triggers POSIX stop
        // Everything after it is operands
        assert_eq!(operands(&r), vec!["-", "root", "-c", "whoami"]);
    }

    #[test]
    fn pattern_ssh() {
        let r = parse_args(
            &tv_args(&["-p", "22", "-i", "key.pem", "user@host", "ls", "-la"]),
            &ArgSpec::posix("b:c:D:E:e:F:I:i:J:L:l:m:O:o:p:Q:R:S:W:w:"),
            true,
        );
        assert_eq!(flag(&r, "-p"), s("22"));
        assert_eq!(flag(&r, "-i"), s("key.pem"));
        // First operand is host, rest is inner command
        assert_eq!(operands(&r), vec!["user@host", "ls", "-la"]);
    }

    #[test]
    fn pattern_ssh_attached_port() {
        let r = parse_args(
            &tv_args(&["-p22", "-i", "key.pem", "host", "ls"]),
            &ArgSpec::posix("p:i:"),
            true,
        );
        assert_eq!(flag(&r, "-p"), s("22"));
        assert_eq!(operands(&r), vec!["host", "ls"]);
    }

    #[test]
    fn pattern_ssh_verbose() {
        // ssh -vvv host cmd — -v -v -v grouped
        let r = parse_args(
            &tv_args(&["-vvv", "-p", "22", "host", "uptime"]),
            &ArgSpec::posix("p:i:o:"),
            true,
        );
        assert_eq!(flag(&r, "-p"), s("22"));
        assert_eq!(operands(&r), vec!["host", "uptime"]);
        assert!(!r.exhaustive); // -v is unknown
    }

    // --- Shell eval ---

    #[test]
    fn pattern_shell_eval() {
        // sh -c "echo hello" — -c takes the command string
        let r = parse_args(
            &tv_args(&["-c", "echo hello"]),
            &ArgSpec::posix("c:"),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("echo hello"));
        assert!(operands(&r).is_empty());
    }

    #[test]
    fn pattern_bash_c_with_args() {
        // bash -c 'echo $0 $1' arg0 arg1 — $0/$1 are expansions → Unknown
        let r = parse_args(
            &tv_args(&["-c", "echo $0 $1", "arg0", "arg1"]),
            &ArgSpec::posix("c:"),
            true,
        );
        assert!(matches!(flag(&r, "-c"), TriVal::Unknown));
        assert_eq!(operands(&r), vec!["arg0", "arg1"]);
    }

    #[test]
    fn pattern_bash_c_literal() {
        // bash -c 'echo hello' — no expansions
        let r = parse_args(
            &tv_args(&["-c", "echo hello"]),
            &ArgSpec::posix("c:"),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("echo hello"));
    }

    // --- Delegating wrappers (args_complete=false) ---

    #[test]
    fn pattern_xargs() {
        let r = parse_args(
            &tv_args(&["-n", "1", "-I", "{}", "rm", "-v"]),
            &ArgSpec::posix("d:I:L:n:P:s:E:"),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("1"));
        assert_eq!(flag(&r, "-I"), s("{}"));
        assert_eq!(operands(&r), vec!["rm", "-v"]);
    }

    #[test]
    fn pattern_xargs_no_flags() {
        let r = parse_args(
            &tv_args(&["rm", "-rf"]),
            &ArgSpec::posix("d:I:L:n:P:s:E:"),
            true,
        );
        assert_eq!(operands(&r), vec!["rm", "-rf"]);
    }


    // --- Environment ---

    #[test]
    fn pattern_env() {
        let r = parse_args(
            &tv_args(&["-u", "FOO", "cmd", "arg"]),
            &ArgSpec::posix("u:S:"),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("FOO"));
        assert_eq!(operands(&r), vec!["cmd", "arg"]);
    }

    #[test]
    fn pattern_env_no_flags() {
        let r = parse_args(
            &tv_args(&["cmd", "arg"]),
            &ArgSpec::posix("u:S:"),
            true,
        );
        assert_eq!(operands(&r), vec!["cmd", "arg"]);
    }

    // --- GNU mode wrappers ---

    #[test]
    fn pattern_kubectl() {
        // kubectl exec pod -c container -- ls -la
        let r = parse_args(
            &tv_args(&["pod", "-c", "container", "--", "ls", "-la"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "n:c:", &["namespace:", "container:"]),
            true,
        );
        assert_eq!(flag(&r, "-c"), s("container"));
        assert_eq!(operands(&r), vec!["pod", "ls", "-la"]);
    }

    #[test]
    fn pattern_kubectl_namespace() {
        // kubectl exec -n kube-system pod -- cmd
        let r = parse_args(
            &tv_args(&["-n", "kube-system", "pod", "--", "cmd"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "n:c:", &["namespace:", "container:"]),
            true,
        );
        assert_eq!(flag(&r, "-n"), s("kube-system"));
        assert_eq!(operands(&r), vec!["pod", "cmd"]);
    }

    #[test]
    fn pattern_docker_exec() {
        let r = parse_args(
            &tv_args(&["-u", "root", "-w", "/app", "mycontainer", "bash"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "e:u:w:", &["env:", "user:", "workdir:"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(flag(&r, "-w"), s("/app"));
        assert_eq!(operands(&r), vec!["mycontainer", "bash"]);
    }

    #[test]
    fn pattern_podman_exec() {
        // podman exec mycontainer -u root bash — flag after operand (GNU)
        let r = parse_args(
            &tv_args(&["mycontainer", "-u", "root", "bash"]),
            &ArgSpec::from_optstring_long(ArgStyle::Gnu, "e:u:w:", &["env:", "user:", "workdir:"]),
            true,
        );
        assert_eq!(flag(&r, "-u"), s("root"));
        assert_eq!(operands(&r), vec!["mycontainer", "bash"]);
    }

    // ========================================================================
    // Accessor methods
    // ========================================================================

    #[test]
    fn operands_from_skips() {
        let r = parse_args(
            &tv_args(&["host", "ls", "-la"]),
            &ArgSpec::posix(""),
            true,
        );
        let from1: Vec<String> = r.operands_from(1).iter().filter_map(|e| {
            if let TriVal::String(s) = e { Some(s.clone()) } else { None }
        }).collect();
        assert_eq!(from1, vec!["ls", "-la"]);
    }

    #[test]
    fn operands_from_beyond_length() {
        let r = parse_args(&tv_args(&["a"]), &ArgSpec::posix(""), true);
        assert!(r.operands_from(5).is_empty());
    }

    #[test]
    fn positional_access() {
        let r = parse_args(
            &tv_args(&["host", "cmd"]),
            &ArgSpec::posix(""),
            true,
        );
        assert_eq!(r.positional(0), s("host"));
        assert_eq!(r.positional(1), s("cmd"));
        assert!(matches!(r.positional(2), TriVal::Null));
    }

}
