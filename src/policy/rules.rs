//! CEL rule compilation and evaluation.
//!
//! Rules are compiled once from TOML config, then evaluated against each
//! CommandInfo in the tree. Top-to-bottom, first match wins.

use std::sync::Arc;

use cel_interpreter::{Context, Program, Value};

use super::config::TaggedRule;
use super::parse::{CommandInfo, Effective};
use super::{Decision, PaneContext, PolicyResult};

// --- Types ---

/// A compiled rule ready for evaluation.
pub struct CompiledRule {
    pub description: String,
    pub program: Program,
    pub action: Decision,
    pub message: Option<String>,
}

/// A compiled rule set: ordered rules + default action.
pub struct RuleSet {
    pub rules: Vec<CompiledRule>,
    pub default: Decision,
}

// --- Public API ---

/// Compile tagged rules into a RuleSet ready for evaluation.
pub fn compile(tagged_rules: &[TaggedRule], default: Decision) -> Result<RuleSet, String> {
    let mut rules = Vec::with_capacity(tagged_rules.len());
    for tagged in tagged_rules {
        let program = Program::compile(&tagged.config.when)
            .map_err(|e| format!("CEL compile error in rule '{}': {}", tagged.config.description, e))?;
        let action = match tagged.config.action.as_str() {
            "allow" => Decision::Allow,
            "deny" => Decision::Deny,
            "ask" => Decision::Ask,
            other => return Err(format!(
                "Invalid action '{}' in rule '{}' (must be allow/ask/deny)",
                other, tagged.config.description
            )),
        };
        rules.push(CompiledRule {
            description: tagged.config.description.clone(),
            program,
            action,
            message: tagged.config.message.clone(),
        });
    }
    Ok(RuleSet { rules, default })
}

/// Evaluate a single CommandInfo against the rule set.
/// Returns the first matching rule's decision, or the default.
pub fn evaluate(cmd: &CommandInfo, ctx: &PaneContext, rules: &RuleSet) -> PolicyResult {
    let cel_ctx = build_context(cmd, ctx);

    for rule in &rules.rules {
        match rule.program.execute(&cel_ctx) {
            Ok(Value::Bool(true)) => {
                let rule_desc = match &rule.message {
                    Some(msg) => format!("{}: {}", rule.description, msg),
                    None => rule.description.clone(),
                };
                return PolicyResult {
                    decision: rule.action.clone(),
                    rule: rule_desc,
                };
            }
            Ok(_) | Err(_) => continue, // no match or error → try next rule
        }
    }

    PolicyResult {
        decision: rules.default.clone(),
        rule: "default".to_string(),
    }
}

// --- CEL context construction ---

fn build_context<'a>(cmd: &'a CommandInfo, pane: &'a PaneContext) -> Context<'a> {
    let mut context = Context::default();

    // command.* variables
    context.add_variable_from_value("command", build_command_map(cmd, pane));

    // pane.* variables
    context.add_variable_from_value("pane", build_pane_map(pane));

    // Custom functions
    context.add_function("glob", glob_match);

    context
}

fn build_command_map(cmd: &CommandInfo, pane: &PaneContext) -> std::collections::HashMap<String, Value> {
    let mut map = std::collections::HashMap::new();

    map.insert("name".into(), Value::String(Arc::new(cmd.name.clone())));

    let args: Vec<Value> = cmd.args.iter()
        .map(|a| Value::String(Arc::new(a.clone())))
        .collect();
    map.insert("args".into(), Value::List(Arc::new(args)));

    map.insert("args_complete".into(), Value::Bool(cmd.args_complete));
    map.insert("is_pipe_target".into(), Value::Bool(cmd.is_pipe_target));

    // effective_user: Unchanged → pane.user, Known → value, Unknown → null
    let effective_user = match &cmd.effective_user {
        Effective::Unchanged => pane.user.as_ref()
            .map(|u| Value::String(Arc::new(u.clone())))
            .unwrap_or(Value::Null),
        Effective::Known(user) => Value::String(Arc::new(user.clone())),
        Effective::Unknown => Value::Null,
    };
    map.insert("effective_user".into(), effective_user);

    // effective_host: Unchanged → pane.hostname, Known → value, Unknown → null
    let effective_host = match &cmd.effective_host {
        Effective::Unchanged => {
            let hostname = pane.hostname.as_deref().unwrap_or("");
            Value::String(Arc::new(hostname.to_string()))
        }
        Effective::Known(host) => Value::String(Arc::new(host.clone())),
        Effective::Unknown => Value::Null,
    };
    map.insert("effective_host".into(), effective_host);

    // parent: full parent CommandInfo as nested map, or null
    let parent_value = match &cmd.parent {
        Some(parent) => Value::from(build_command_map(parent, pane)),
        None => Value::Null,
    };
    map.insert("parent".into(), parent_value);

    map
}

fn build_pane_map(pane: &PaneContext) -> std::collections::HashMap<String, Value> {
    let mut map = std::collections::HashMap::new();

    map.insert("hostname".into(), Value::String(Arc::new(
        pane.hostname.clone().unwrap_or_default()
    )));
    map.insert("cwd".into(), Value::String(Arc::new(
        pane.cwd.clone().unwrap_or_default()
    )));
    map.insert("foreground".into(), Value::String(Arc::new(
        pane.foreground.clone().unwrap_or_default()
    )));
    map.insert("user".into(), Value::String(Arc::new(
        pane.user.clone().unwrap_or_default()
    )));

    map
}

// --- Custom CEL functions ---

fn glob_match(pattern: Arc<String>, text: Arc<String>) -> bool {
    globset::Glob::new(pattern.as_str())
        .map(|g| g.compile_matcher().is_match(text.as_str()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::parse::Effective;

    // --- Test harness helpers ---

    fn cmd(name: &str) -> CommandInfo {
        CommandInfo::simple(name)
    }

    fn cmd_with_args(name: &str, args: &[&str]) -> CommandInfo {
        CommandInfo {
            args: args.iter().map(|s| s.to_string()).collect(),
            ..CommandInfo::simple(name)
        }
    }

    fn local_pane() -> PaneContext {
        PaneContext {
            hostname: None,
            cwd: Some("/home/user".into()),
            foreground: Some("bash".into()),
            user: Some("jess".into()),
        }
    }

    fn remote_pane(host: &str) -> PaneContext {
        PaneContext {
            hostname: Some(host.into()),
            ..local_pane()
        }
    }

    fn compile_rules(specs: &[(&str, &str, &str)]) -> RuleSet {
        let tagged: Vec<TaggedRule> = specs.iter().enumerate().map(|(i, (desc, when, action))| {
            TaggedRule {
                config: crate::policy::config::RuleConfig {
                    description: desc.to_string(),
                    when: when.to_string(),
                    action: action.to_string(),
                    order: 0,
                    message: None,
                },
                source: crate::policy::config::RuleSource::Builtin,
                source_index: i,
            }
        }).collect();
        compile(&tagged, Decision::Ask).unwrap()
    }

    // --- Dependency contract: cel-interpreter ---

    #[test]
    fn cel_string_equality() {
        let prog = Program::compile(r#"x == "hello""#).unwrap();
        let mut ctx = Context::default();
        ctx.add_variable_from_value("x", "hello".to_string());
        assert_eq!(prog.execute(&ctx).unwrap(), Value::Bool(true));
    }

    #[test]
    fn cel_list_membership() {
        let prog = Program::compile(r#"x in ["a","b","c"]"#).unwrap();
        let mut ctx = Context::default();
        ctx.add_variable_from_value("x", "b".to_string());
        assert_eq!(prog.execute(&ctx).unwrap(), Value::Bool(true));
    }

    #[test]
    fn cel_exists_on_list() {
        let prog = Program::compile(r#"xs.exists(x, x == "a")"#).unwrap();
        let mut ctx = Context::default();
        ctx.add_variable_from_value("xs", vec!["a".to_string(), "b".to_string()]);
        assert_eq!(prog.execute(&ctx).unwrap(), Value::Bool(true));
    }

    #[test]
    fn cel_exists_on_empty_list() {
        let prog = Program::compile(r#"xs.exists(x, x == "a")"#).unwrap();
        let mut ctx = Context::default();
        ctx.add_variable_from_value("xs", Vec::<String>::new());
        assert_eq!(prog.execute(&ctx).unwrap(), Value::Bool(false));
    }

    #[test]
    fn cel_null_equality_is_false() {
        let prog = Program::compile(r#"x == "hello""#).unwrap();
        let mut ctx = Context::default();
        ctx.add_variable_from_value("x", None::<String>);
        // CEL: null == "hello" should be false
        match prog.execute(&ctx) {
            Ok(Value::Bool(false)) => {}
            Ok(Value::Bool(true)) => panic!("null == 'hello' should be false"),
            Err(_) => {} // execution error on null comparison is also acceptable
            other => panic!("unexpected result: {:?}", other),
        }
    }

    #[test]
    fn cel_boolean_and() {
        let prog = Program::compile("a && b").unwrap();
        let mut ctx = Context::default();
        ctx.add_variable_from_value("a", true);
        ctx.add_variable_from_value("b", false);
        assert_eq!(prog.execute(&ctx).unwrap(), Value::Bool(false));
    }

    #[test]
    fn cel_compile_error_detected() {
        assert!(Program::compile("invalid @@@ syntax").is_err());
    }

    // --- Basic matching ---

    #[test]
    fn name_equals_matches() {
        let rules = compile_rules(&[("ls rule", r#"command.name == "ls""#, "allow")]);
        let result = evaluate(&cmd("ls"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Allow);
    }

    #[test]
    fn name_equals_no_match() {
        let rules = compile_rules(&[("ls rule", r#"command.name == "ls""#, "allow")]);
        let result = evaluate(&cmd("cat"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Ask); // default
    }

    #[test]
    fn name_in_list_matches() {
        let rules = compile_rules(&[
            ("tools", r#"command.name in ["ls","cat","grep"]"#, "allow"),
        ]);
        let result = evaluate(&cmd("cat"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Allow);
    }

    #[test]
    fn args_exists_matches() {
        let rules = compile_rules(&[
            ("rm -rf", r#"command.name == "rm" && command.args.exists(a, a == "-rf")"#, "ask"),
        ]);
        let result = evaluate(&cmd_with_args("rm", &["-rf", "/"]), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Ask);
    }

    #[test]
    fn args_exists_no_match() {
        let rules = compile_rules(&[
            ("rm -rf", r#"command.name == "rm" && command.args.exists(a, a == "-rf")"#, "ask"),
        ]);
        let result = evaluate(&cmd_with_args("rm", &["-v", "file"]), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Ask); // doesn't match rule, falls to default (also ask)
    }

    // --- First match wins ---

    #[test]
    fn first_rule_wins_on_match() {
        let rules = compile_rules(&[
            ("allow ls", r#"command.name == "ls""#, "allow"),
            ("deny ls", r#"command.name == "ls""#, "deny"),
        ]);
        let result = evaluate(&cmd("ls"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Allow);
    }

    #[test]
    fn later_rule_used_if_first_doesnt_match() {
        let rules = compile_rules(&[
            ("allow cat", r#"command.name == "cat""#, "allow"),
            ("deny ls", r#"command.name == "ls""#, "deny"),
        ]);
        let result = evaluate(&cmd("ls"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn no_match_returns_default() {
        let rules = compile_rules(&[
            ("allow cat", r#"command.name == "cat""#, "allow"),
        ]);
        let result = evaluate(&cmd("ls"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Ask);
        assert_eq!(result.rule, "default");
    }

    // --- Unknowable user ---

    #[test]
    fn unknown_user_equals_returns_false() {
        let rules = compile_rules(&[
            ("root check", r#"command.effective_user == "root""#, "deny"),
        ]);
        let mut c = cmd("rm");
        c.effective_user = Effective::Unknown;
        let result = evaluate(&c, &local_pane(), &rules);
        // null == "root" → false → rule doesn't match → default
        assert_eq!(result.decision, Decision::Ask);
    }

    #[test]
    fn known_user_equals_matches() {
        let rules = compile_rules(&[
            ("root check", r#"command.effective_user == "root""#, "deny"),
        ]);
        let mut c = cmd("rm");
        c.effective_user = Effective::Known("root".into());
        let result = evaluate(&c, &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn unchanged_user_resolves_to_pane_user() {
        let rules = compile_rules(&[
            ("jess check", r#"command.effective_user == "jess""#, "allow"),
        ]);
        let result = evaluate(&cmd("ls"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Allow);
    }

    // --- Unknowable host ---

    #[test]
    fn unknown_host_equals_returns_false() {
        let rules = compile_rules(&[
            ("prod check", r#"command.effective_host == "prod""#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Unknown;
        let result = evaluate(&c, &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Ask); // null == "prod" → false
    }

    #[test]
    fn known_host_equals_matches() {
        let rules = compile_rules(&[
            ("prod check", r#"command.effective_host == "prod""#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("prod".into());
        let result = evaluate(&c, &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn unchanged_host_resolves_to_pane_hostname() {
        let rules = compile_rules(&[
            ("staging", r#"command.effective_host == "staging.example.com""#, "allow"),
        ]);
        let result = evaluate(&cmd("ls"), &remote_pane("staging.example.com"), &rules);
        assert_eq!(result.decision, Decision::Allow);
    }

    #[test]
    fn local_pane_host_is_empty_string() {
        let rules = compile_rules(&[
            ("local", r#"command.effective_host == """#, "allow"),
        ]);
        let result = evaluate(&cmd("ls"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Allow);
    }

    // --- Pipe target ---

    #[test]
    fn pipe_target_matches() {
        let rules = compile_rules(&[
            ("pipe to bash", r#"command.name == "bash" && command.is_pipe_target"#, "deny"),
        ]);
        let mut c = cmd("bash");
        c.is_pipe_target = true;
        let result = evaluate(&c, &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn non_pipe_target_no_match() {
        let rules = compile_rules(&[
            ("pipe to bash", r#"command.name == "bash" && command.is_pipe_target"#, "deny"),
        ]);
        let result = evaluate(&cmd("bash"), &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Ask); // is_pipe_target=false, doesn't match
    }

    // --- Glob function ---

    #[test]
    fn glob_matches_wildcard() {
        let rules = compile_rules(&[
            ("prod", r#"glob("*.prod.*", command.effective_host)"#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("web.prod.example.com".into());
        let result = evaluate(&c, &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn glob_no_match() {
        let rules = compile_rules(&[
            ("prod", r#"glob("*.prod.*", command.effective_host)"#, "deny"),
        ]);
        let mut c = cmd("ls");
        c.effective_host = Effective::Known("web.staging.example.com".into());
        let result = evaluate(&c, &local_pane(), &rules);
        assert_eq!(result.decision, Decision::Ask);
    }

    // --- Pane context in rules ---

    #[test]
    fn pane_hostname_matches() {
        let rules = compile_rules(&[
            ("prod host", r#"pane.hostname == "prod-server""#, "deny"),
        ]);
        let result = evaluate(&cmd("ls"), &remote_pane("prod-server"), &rules);
        assert_eq!(result.decision, Decision::Deny);
    }

    #[test]
    fn pane_cwd_matches() {
        let rules = compile_rules(&[
            ("etc", r#"pane.cwd == "/etc""#, "deny"),
        ]);
        let pane = PaneContext { cwd: Some("/etc".into()), ..local_pane() };
        let result = evaluate(&cmd("ls"), &pane, &rules);
        assert_eq!(result.decision, Decision::Deny);
    }

    // --- Compile errors ---

    #[test]
    fn compile_error_reported() {
        let tagged = vec![TaggedRule {
            config: crate::policy::config::RuleConfig {
                description: "bad rule".into(),
                when: "invalid @@@ syntax".into(),
                action: "allow".into(),
                order: 0,
                message: None,
            },
            source: crate::policy::config::RuleSource::Builtin,
            source_index: 0,
        }];
        match compile(&tagged, Decision::Ask) {
            Ok(_) => panic!("expected compile error"),
            Err(err) => assert!(err.contains("bad rule"), "error was: {}", err),
        }
    }

    #[test]
    fn invalid_action_reported() {
        let tagged = vec![TaggedRule {
            config: crate::policy::config::RuleConfig {
                description: "bad action".into(),
                when: "true".into(),
                action: "invalid".into(),
                order: 0,
                message: None,
            },
            source: crate::policy::config::RuleSource::Builtin,
            source_index: 0,
        }];
        match compile(&tagged, Decision::Ask) {
            Ok(_) => panic!("expected compile error"),
            Err(err) => assert!(err.contains("invalid"), "error was: {}", err),
        }
    }

    // --- Builtin rules compile ---

    #[test]
    fn builtin_rules_all_compile() {
        let (default, tagged) = crate::policy::config::load_merged_rules(None);
        if let Err(err) = compile(&tagged, default) {
            panic!("builtin rules failed to compile: {}", err);
        }
    }
}
