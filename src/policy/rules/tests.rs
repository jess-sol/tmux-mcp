use std::path::Path;

use super::*;
use crate::policy::config::parse_config;

const T: &str = "[commands.t]\nparser = \"gnu\"\narguments = [\"xs...\"]\n";

fn ruleset(rules_toml: &str) -> RuleSet {
    let config = parse_config(&format!("{T}{rules_toml}")).unwrap();
    let grammars = Grammars::compile([&config.commands]).unwrap();
    let rules: Vec<_> = config.rules.into_iter().map(|r| (r, false)).collect();
    compile(&rules, Decision::Ask, &grammars).unwrap_or_else(|e| panic!("{e}"))
}

fn env(host: Host) -> Env {
    Env {
        host,
        this_host: "here".into(),
        pane_cwd: Some("/src/proj/sub".into()),
        home: Some("/home/u".into()),
        anchor: Some("/src/proj".into()),
        project_dirs: vec!["/src/proj".into()],
    }
}

fn facts_with_xs(xs: TriVal) -> Facts {
    Facts {
        identity: TriVal::String("t".into()),
        relative: false,
        subcommands: vec![],
        args: TriVal::Map(HashMap::from([("xs".to_string(), xs)])),
        paths: TriVal::List { elements: vec![], exhaustive: true },
        writes: TriVal::List { elements: vec![], exhaustive: true },
        dir: Some("/src/proj".into()),
        variables: TriVal::List { elements: vec![], exhaustive: true },
        user: TriVal::Null,
    }
}

fn vars(pairs: &[(&str, TriVal)]) -> Ctx<'static> {
    Ctx { vars: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), site: None }
}

fn strings(vals: &[&str], exhaustive: bool) -> TriVal {
    TriVal::List { elements: vals.iter().map(|v| TriVal::String(v.to_string())).collect(), exhaustive }
}

fn eval_cel(expr: &str) -> TriVal {
    eval_cel_with(expr, &vars(&[]))
}

fn eval_cel_with(expr: &str, ctx: &Ctx) -> TriVal {
    let ast = cel_parser::Parser::new().parse(expr).unwrap_or_else(|e| panic!("CEL parse error: {:?}", e));
    eval_expr(&ast.expr, ctx)
}

// --- Dependency contract: cel-parser ---

#[test]
fn cel_parser_names_operators_and_macros() {
    let func = |text: &str| match cel_parser::Parser::new().parse(text).unwrap().expr {
        cel::Expr::Call(c) => c.func_name,
        other => format!("{other:?}"),
    };
    assert_eq!(func(r#"x == "a""#), "_==_");
    assert_eq!(func("a && b"), "_&&_");
    assert_eq!(func(r#"x in ["a"]"#), "@in");
    assert_eq!(func("xs[0]"), "_[_]");
    assert!(matches!(cel_parser::Parser::new().parse("xs.exists(x, x)").unwrap().expr, cel::Expr::Comprehension(_)));
    assert!(matches!(cel_parser::Parser::new().parse("a.b.c").unwrap().expr, cel::Expr::Select(_)));
    assert!(cel_parser::Parser::new().parse("invalid @@@ syntax").is_err());
}

// --- Matching and combining ---

#[test]
fn a_rule_that_may_match_fires_unless_it_allows_or_overrides() {
    const UNSURE: &str = "rule (could not be ruled out)";
    // (override, action, when, beside, over a non-exhaustive xs, over an exhaustive xs):
    // each rule sits beside a rule of action `beside`, over xs = ["x"].
    let rows = [
        ("false", "ask", r#"args.xs.exists(a, a == "-r")"#, "allow", (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
        ("false", "ask", r#"(args.xs + ["y"]).exists(a, a == "-r")"#, "allow", (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
        ("false", "deny", r#""-r" in args.xs"#, "allow", (Decision::Deny, UNSURE), (Decision::Allow, "beside")),
        ("false", "allow", r#"!args.xs.exists(a, a == "-r")"#, "ask", (Decision::Ask, "beside"), (Decision::Ask, "beside")),
        ("true", "allow", r#"args.xs.exists(a, a == "-r")"#, "ask", (Decision::Ask, "beside"), (Decision::Ask, "beside")),
    ];
    for (is_override, action, when, beside, incomplete, complete) in rows {
        let rules = ruleset(&format!(
            "[[rules]]\ndescription = \"rule\"\ncommand = \"t\"\nwhen = '{when}'\naction = \"{action}\"\noverride = {is_override}\n\
             [[rules]]\ndescription = \"beside\"\naction = \"{beside}\"\n"
        ));
        for (exhaustive, want) in [(false, incomplete), (true, complete)] {
            let got = decide(&rules, &facts_with_xs(strings(&["x"], exhaustive)), &env(Host::Here));
            assert_eq!((got.decision, got.rule.as_str()), want, "{when}, exhaustive={exhaustive}");
        }
    }
}

#[test]
fn a_rule_meets_only_this_machine_unless_its_host_says_otherwise() {
    let rules = ruleset(
        r#"
        [[rules]]
        description = "anywhere"
        host = 'true'
        action = "deny"
        [[rules]]
        description = "here"
        action = "ask"
        "#,
    );
    let facts = facts_with_xs(strings(&[], true));
    let rows = [
        (Host::Here, Decision::Deny),
        (Host::There("prod".into()), Decision::Deny),
        (Host::Unknown, Decision::Deny),
    ];
    for (host, want) in rows {
        assert_eq!(decide(&rules, &facts, &env(host)).decision, want);
    }
    let here_only = ruleset("[[rules]]\ndescription = \"here\"\naction = \"allow\"\n");
    assert_eq!(decide(&here_only, &facts, &env(Host::There("prod".into()))).decision, Decision::Ask);
    assert_eq!(decide(&here_only, &facts, &env(Host::Unknown)).decision, Decision::Ask);
    assert_eq!(decide(&here_only, &facts, &env(Host::Here)).decision, Decision::Allow);
}

#[test]
fn a_project_rule_applies_only_on_this_machine() {
    let config = parse_config("[[rules]]\ndescription = \"p\"\nhost = 'true'\naction = \"deny\"").unwrap();
    let rules = compile(&[(config.rules[0].clone(), true)], Decision::Ask, &Grammars::default()).unwrap();
    let facts = facts_with_xs(strings(&[], true));
    assert_eq!(decide(&rules, &facts, &env(Host::There("prod".into()))).decision, Decision::Ask);
    assert_eq!(decide(&rules, &facts, &env(Host::Here)).decision, Decision::Deny);
}

#[test]
fn a_path_rule_holds_for_any_path_and_is_unsure_over_an_open_list() {
    let rules = ruleset("[[rules]]\ndescription = \"out\"\npath = '!in_project(path)'\naction = \"ask\"\n");
    let rows = [
        (strings(&["/src/proj/a"], true), "default"),
        (strings(&["/src/proj/a", "/etc/x"], true), "out"),
        (strings(&["/etc/x"], false), "out"),
        (strings(&["/src/proj/a"], false), "out (could not be ruled out)"),
        (TriVal::List { elements: vec![TriVal::Unknown], exhaustive: true }, "out (could not be ruled out)"),
    ];
    for (paths, want) in rows {
        let facts = Facts { paths: paths.clone(), ..facts_with_xs(strings(&[], true)) };
        assert_eq!(decide(&rules, &facts, &env(Host::Here)).rule, want, "{paths:?}");
    }
}

// --- Command identity ---

#[test]
fn a_command_is_named_by_its_path_from_the_anchor() {
    let rows = [
        ("rm", Host::Here, TriVal::String("rm".into())),
        ("/usr/bin/rm", Host::Here, TriVal::String("rm".into())),
        ("/src/proj/scripts/bench", Host::Here, TriVal::String("./scripts/bench".into())),
        ("../scripts/bench", Host::Here, TriVal::String("./scripts/bench".into())),
        ("./bin/cat", Host::Here, TriVal::String("./sub/bin/cat".into())),
        ("../../other/x", Host::Here, TriVal::String("../other/x".into())),
        ("~/bin/x", Host::Here, TriVal::String("x".into())),
        ("/usr/bin/rm", Host::There("prod".into()), TriVal::String("rm".into())),
        ("./run", Host::There("prod".into()), TriVal::Unknown),
    ];
    for (name, host, want) in rows {
        let env = env(host);
        assert_eq!(identity(&TriVal::String(name.into()), &env, env.pane_cwd.as_deref()), want, "{name}");
    }
    let here = env(Host::Here);
    assert_eq!(identity(&TriVal::String("./run".into()), &here, None), TriVal::Unknown);
    assert_eq!(identity(&TriVal::String("rm".into()), &here, None), TriVal::String("rm".into()));
    assert_eq!(identity(&TriVal::Unknown, &here, None), TriVal::Unknown);
    assert_eq!(identity(&TriVal::Null, &here, None), TriVal::Null);
}

// --- Paths ---

#[test]
fn a_path_resolves_as_text() {
    let base = Some(Path::new("/src/proj"));
    let home = Some(Path::new("/home/u"));
    let rows = [
        ("a/../b", TriVal::String("/src/proj/b".into())),
        ("../../..", TriVal::String("/".into())),
        ("/etc/./x", TriVal::String("/etc/x".into())),
        ("~/x", TriVal::String("/home/u/x".into())),
        ("~bob/x", TriVal::String("~bob/x".into())),
    ];
    for (word, want) in rows {
        assert_eq!(resolve(word, base, home), want, "{word}");
    }
    assert_eq!(resolve("a", None, home), TriVal::Unknown);
}

#[test]
fn path_functions_resolve_against_the_command_directory() {
    let env = env(Host::Here);
    let site = Site { env: &env, dir: Some(Path::new("/src/proj/sub")) };
    let ctx = Ctx { vars: HashMap::new(), site: Some(site) };
    let rows = [
        ("in_project('..')", TriVal::Bool(true)),
        ("in_project('../../proj-evil/x')", TriVal::Bool(false)),
        ("in_project('/src/projx')", TriVal::Bool(false)),
        ("in_project('~bob/x')", TriVal::Bool(false)),
        ("is_device('/dev/null')", TriVal::Bool(true)),
        ("is_device('/devx')", TriVal::Bool(false)),
        ("path('a/../b')", TriVal::String("/src/proj/sub/b".into())),
        ("path(null)", TriVal::Null),
    ];
    for (expr, want) in rows {
        assert_eq!(eval_cel_with(expr, &ctx), want, "{expr}");
    }
    let elsewhere = Env { host: Host::Unknown, ..env };
    let ctx = Ctx { vars: HashMap::new(), site: Some(Site { env: &elsewhere, dir: Some(Path::new("/src/proj")) }) };
    assert_eq!(eval_cel_with("in_project('/src/proj')", &ctx), TriVal::Unknown);
}

// --- Unknown, Null and lists ---

#[test]
fn unknown_propagates_through_kleene_logic() {
    let ctx = vars(&[("x", TriVal::Unknown)]);
    assert_eq!(eval_cel_with(r#"x == "root""#, &ctx), TriVal::Unknown);
    assert_eq!(eval_cel_with(r#"x != "root""#, &ctx), TriVal::Unknown);
    assert_eq!(eval_cel_with(r#"true || x == "root""#, &ctx), TriVal::Bool(true));
    assert_eq!(eval_cel_with(r#"true && x == "root""#, &ctx), TriVal::Unknown);
    assert_eq!(eval_cel_with(r#"false && x == "root""#, &ctx), TriVal::Bool(false));
}

#[test]
fn null_compares_definitely() {
    let ctx = vars(&[("x", TriVal::Null)]);
    assert_eq!(eval_cel_with(r#"x.name == "sudo""#, &ctx), TriVal::Bool(false));
    assert_eq!(eval_cel_with(r#"x.name != "sudo""#, &ctx), TriVal::Bool(true));
    assert_eq!(eval_cel_with("x == null", &ctx), TriVal::Bool(true));
    assert_eq!(eval_cel_with(r#"x in [null, "show"]"#, &ctx), TriVal::Bool(true));
}

#[test]
fn a_non_exhaustive_list_answers_only_what_it_can() {
    let open = vars(&[("xs", strings(&["-v"], false))]);
    let closed = vars(&[("xs", strings(&["-v"], true))]);
    let rows = [
        (r#"xs.exists(a, a == "-v")"#, TriVal::Bool(true), TriVal::Bool(true)),
        (r#"xs.exists(a, a == "-rf")"#, TriVal::Unknown, TriVal::Bool(false)),
        (r#""-rf" in xs"#, TriVal::Unknown, TriVal::Bool(false)),
        (r#"xs.all(a, a == "-v")"#, TriVal::Unknown, TriVal::Bool(true)),
        ("xs.size()", TriVal::Unknown, TriVal::Int(1)),
        ("xs[0]", TriVal::String("-v".into()), TriVal::String("-v".into())),
        ("xs[1]", TriVal::Unknown, TriVal::Null),
        (r#"xs == ["-v"]"#, TriVal::Unknown, TriVal::Bool(true)),
        ("xs == []", TriVal::Bool(false), TriVal::Bool(false)),
        (r#"xs == ["-x"]"#, TriVal::Bool(false), TriVal::Bool(false)),
        (r#"xs == ["-v", "-x"]"#, TriVal::Unknown, TriVal::Bool(false)),
        (r#"xs == "-v""#, TriVal::Unknown, TriVal::Unknown),
    ];
    for (expr, want_open, want_closed) in rows {
        assert_eq!(eval_cel_with(expr, &open), want_open, "{expr} over an open list");
        assert_eq!(eval_cel_with(expr, &closed), want_closed, "{expr} over a closed list");
    }
}

// --- Functions and operators ---

#[test]
fn string_functions_test_their_operands() {
    let rows = [
        (r#"glob("*.prod.*", "web.prod.example.com")"#, TriVal::Bool(true)),
        (r#"glob("*.prod.*", "web.staging.example.com")"#, TriVal::Bool(false)),
        (r#"matches("abc123", "^[a-z]+[0-9]+$")"#, TriVal::Bool(true)),
        (r#""abc".matches("[")"#, TriVal::Unknown),
        (r#"contains("abc", "b")"#, TriVal::Bool(true)),
        (r#""abc".startsWith("ab")"#, TriVal::Bool(true)),
        (r#"or(null, "x")"#, TriVal::String("x".into())),
        (r#"awk_is_plain("{print $1}")"#, TriVal::Bool(true)),
        (r#"sed_is_plain("s/a/b/w x")"#, TriVal::Bool(false)),
    ];
    for (expr, want) in rows {
        assert_eq!(eval_cel(expr), want, "{expr}");
    }
    let ctx = vars(&[("h", TriVal::Unknown)]);
    assert_eq!(eval_cel_with(r#"glob("*", h)"#, &ctx), TriVal::Unknown);
}

#[test]
fn operators_compute_over_known_values() {
    let ctx = vars(&[("xs", strings(&["ls", "-la", "/"], true))]);
    let rows = [
        ("2 > 1 && 1 >= 1 && !(1 < 1)", TriVal::Bool(true)),
        ("1 + 2", TriVal::Int(3)),
        (r#"["a"] + ["b"]"#, strings(&["a", "b"], true)),
        (r#""a" + 1"#, TriVal::Unknown),
        (r#"size("ls")"#, TriVal::Int(2)),
        (r#"true ? (false ? "a" : "b") : "c""#, TriVal::String("b".into())),
        ("x ? 1 : 2", TriVal::Unknown),
        (r#"xs.filter(a, a != "-la")"#, strings(&["ls", "/"], true)),
        (r#"xs.map(a, a == "-la" ? "long" : a)"#, strings(&["ls", "long", "/"], true)),
    ];
    for (expr, want) in rows {
        assert_eq!(eval_cel_with(expr, &ctx), want, "{expr}");
    }
}
