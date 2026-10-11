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

fn env() -> Env {
    Env { this_host: "here".into(), anchor: Some("/src/proj".into()), project_dirs: vec!["/src/proj".into()] }
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
        host: Host::Here,
        user: TriVal::String("u".into()),
        own_user: true,
    }
}

fn on(host: Host) -> Facts {
    Facts { host, ..facts_with_xs(strings(&[], true)) }
}

fn vars(pairs: &[(&str, TriVal)]) -> Ctx<'static> {
    Ctx { vars: pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect(), site: None, home: TriVal::Unknown }
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
fn a_rule_that_may_match_fires_unless_it_allows() {
    const UNSURE: &str = "rule (could not be ruled out)";
    // (action, when, beside, over a non-exhaustive xs, over an exhaustive xs):
    // each rule sits beside a rule of action `beside`, over xs = ["x"].
    let rows = [
        ("ask", r#"args.xs.exists(a, a == "-r")"#, "allow", (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
        ("ask", r#"(args.xs + ["y"]).exists(a, a == "-r")"#, "allow", (Decision::Ask, UNSURE), (Decision::Allow, "beside")),
        ("deny", r#""-r" in args.xs"#, "allow", (Decision::Deny, UNSURE), (Decision::Allow, "beside")),
        ("allow", r#"!args.xs.exists(a, a == "-r")"#, "ask", (Decision::Ask, "beside"), (Decision::Ask, "beside")),
    ];
    for (action, when, beside, incomplete, complete) in rows {
        let rules = ruleset(&format!(
            "[[rules]]\nid = \"rule\"\ncommand = \"t\"\nwhen = '{when}'\naction = \"{action}\"\n\
             [[rules]]\nid = \"beside\"\naction = \"{beside}\"\n"
        ));
        for (exhaustive, want) in [(false, incomplete), (true, complete)] {
            let got = decide(&rules, &facts_with_xs(strings(&["x"], exhaustive)), &env());
            assert_eq!((got.decision, got.rule.as_str()), want, "{when}, exhaustive={exhaustive}");
        }
    }
}

/// An *Exemption* removes the rule it names only where its own rule
/// surely matches: on the command, or on each path.
#[test]
fn an_exemption_removes_the_named_rule_only_where_its_rule_surely_matches() {
    let rules = ruleset(
        r#"
        [[rules]]
        id = "out"
        path = '!in_project(path)'
        action = "ask"
        [[rules]]
        id = "hosts"
        path = 'path == "/etc/hosts"'
        action = "allow"
        exempts = ["out"]
        [[rules]]
        id = "x-flag"
        command = "t"
        when = '"-x" in args.xs'
        action = "deny"
        [[rules]]
        id = "x-asks"
        command = "t"
        when = 'args.xs.exists(a, a == "-x")'
        action = "ask"
        exempts = ["x-flag"]
        [[rules]]
        id = "y-allows"
        command = "t"
        when = '"-y" in args.xs'
        action = "allow"
        exempts = ["x-asks"]
        "#,
    );
    let under_tmp = ruleset(
        r#"
        [[rules]]
        id = "x-flag"
        command = "t"
        when = '"-x" in args.xs'
        action = "deny"
        [[rules]]
        id = "under-tmp"
        command = "t"
        path = 'path.startsWith("/tmp/")'
        action = "allow"
        exempts = ["x-flag"]
        "#,
    );
    // (rules, xs, paths, decision, rule)
    let rows = [
        (&rules, strings(&[], true), strings(&["/etc/hosts"], true), Decision::Allow, "hosts"),
        (&rules, strings(&[], true), strings(&["/etc/hosts", "/etc/shadow"], true), Decision::Ask, "out"),
        (&rules, strings(&["-x"], true), strings(&[], true), Decision::Ask, "x-asks"),
        // An exempting rule that may match exempts nothing.
        (&rules, strings(&[], false), strings(&[], true), Decision::Deny, "x-flag (could not be ruled out)"),
        // An exempted rule's own exemptions do not apply.
        (&rules, strings(&["-x", "-y"], true), strings(&[], true), Decision::Deny, "x-flag"),
        // A path rule exempts a command rule only over every path of an exhaustive list.
        (&under_tmp, strings(&["-x"], true), strings(&["/tmp/a"], true), Decision::Allow, "under-tmp"),
        (&under_tmp, strings(&["-x"], true), strings(&["/tmp/a", "/etc/b"], true), Decision::Deny, "x-flag"),
        (&under_tmp, strings(&["-x"], true), strings(&["/tmp/a"], false), Decision::Deny, "x-flag"),
    ];
    for (rules, xs, paths, decision, rule) in rows {
        let facts = Facts { paths: paths.clone(), ..facts_with_xs(xs.clone()) };
        let got = decide(rules, &facts, &env());
        assert_eq!((got.decision, got.rule.as_str()), (decision, rule), "xs={xs:?}, paths={paths:?}");
    }
}

/// An ask or deny rule without `host` meets every host; an allow rule
/// without it only this machine, and `host` loosens it.
#[test]
fn a_rule_is_strict_by_default_and_its_host_loosens_an_allow() {
    let strict = ruleset("[[rules]]\nid = \"strict\"\naction = \"deny\"\n");
    let here = ruleset("[[rules]]\nid = \"here\"\naction = \"allow\"\n");
    let prod = ruleset("[[rules]]\nid = \"prod\"\nhost = 'host == \"prod\"'\naction = \"allow\"\n");
    // (host, strict, here, prod)
    let rows = [
        (Host::Here, Decision::Deny, Decision::Allow, Decision::Ask),
        (Host::There("prod".into()), Decision::Deny, Decision::Ask, Decision::Allow),
        (Host::Unknown, Decision::Deny, Decision::Ask, Decision::Ask),
    ];
    for (host, want_strict, want_here, want_prod) in rows {
        let facts = on(host);
        let got = [&strict, &here, &prod].map(|rules| decide(rules, &facts, &env()).decision);
        assert_eq!(got, [want_strict, want_here, want_prod]);
    }
}

/// An ask or deny rule without `user` meets every user; an allow rule
/// without it only the shell's own user, and `user` loosens it.
#[test]
fn a_rule_is_strict_by_default_and_its_user_loosens_an_allow() {
    let strict = ruleset("[[rules]]\nid = \"strict\"\naction = \"ask\"\n");
    let own = ruleset("[[rules]]\nid = \"own\"\naction = \"allow\"\n");
    let root = ruleset("[[rules]]\nid = \"root\"\nuser = 'user == \"root\"'\naction = \"allow\"\n");
    let u = |name: &str| TriVal::String(name.into());
    // (user, own_user, strict, own, root)
    let rows = [
        (u("u"), true, Decision::Ask, Decision::Allow, Decision::Ask),
        (TriVal::Unknown, true, Decision::Ask, Decision::Allow, Decision::Ask),
        (u("root"), false, Decision::Ask, Decision::Ask, Decision::Allow),
        (TriVal::Unknown, false, Decision::Ask, Decision::Ask, Decision::Ask),
    ];
    for (user, own_user, want_strict, want_own, want_root) in rows {
        let facts = Facts { user: user.clone(), own_user, ..facts_with_xs(strings(&[], true)) };
        let got = [&strict, &own, &root].map(|rules| decide(rules, &facts, &env()).decision);
        assert_eq!(got, [want_strict, want_own, want_root], "{user:?}, own_user={own_user}");
    }
}

#[test]
fn a_project_rule_applies_only_on_this_machine() {
    let config = parse_config("[[rules]]\nid = \"p\"\naction = \"deny\"").unwrap();
    let rules = compile(&[(config.rules[0].clone(), true)], Decision::Ask, &Grammars::default()).unwrap();
    assert_eq!(decide(&rules, &on(Host::There("prod".into())), &env()).decision, Decision::Ask);
    assert_eq!(decide(&rules, &on(Host::Here), &env()).decision, Decision::Deny);
}

#[test]
fn a_path_rule_holds_for_any_path_and_is_unsure_over_an_open_list() {
    let rules = ruleset("[[rules]]\nid = \"out\"\npath = '!in_project(path)'\naction = \"ask\"\n");
    let rows = [
        (strings(&["/src/proj/a"], true), "default"),
        (strings(&["/src/proj/a", "/etc/x"], true), "out"),
        (strings(&["/etc/x"], false), "out"),
        (strings(&["/src/proj/a"], false), "out (could not be ruled out)"),
        (TriVal::List { elements: vec![TriVal::Unknown], exhaustive: true }, "out (could not be ruled out)"),
    ];
    for (paths, want) in rows {
        let facts = Facts { paths: paths.clone(), ..facts_with_xs(strings(&[], true)) };
        assert_eq!(decide(&rules, &facts, &env()).rule, want, "{paths:?}");
    }
}

// --- Command identity ---

#[test]
fn a_command_is_named_by_its_path_from_the_anchor() {
    let sub = Some(Path::new("/src/proj/sub"));
    let rows = [
        ("rm", Host::Here, TriVal::String("rm".into())),
        ("/usr/bin/rm", Host::Here, TriVal::String("rm".into())),
        ("/src/proj/scripts/bench", Host::Here, TriVal::String("./scripts/bench".into())),
        ("../scripts/bench", Host::Here, TriVal::String("./scripts/bench".into())),
        ("./bin/cat", Host::Here, TriVal::String("./sub/bin/cat".into())),
        ("../../other/x", Host::Here, TriVal::String("../other/x".into())),
        ("~bob/bin/x", Host::Here, TriVal::String("x".into())),
        ("/usr/bin/rm", Host::There("prod".into()), TriVal::String("rm".into())),
        ("./run", Host::There("prod".into()), TriVal::Unknown),
    ];
    for (name, host, want) in rows {
        assert_eq!(identity(&TriVal::String(name.into()), &host, &env(), sub), want, "{name}");
    }
    let here = Host::Here;
    assert_eq!(identity(&TriVal::String("./run".into()), &here, &env(), None), TriVal::Unknown);
    assert_eq!(identity(&TriVal::String("rm".into()), &here, &env(), None), TriVal::String("rm".into()));
    assert_eq!(identity(&TriVal::Unknown, &here, &env(), None), TriVal::Unknown);
    assert_eq!(identity(&TriVal::Null, &here, &env(), None), TriVal::Null);
}

// --- Paths ---

#[test]
fn a_path_resolves_as_text() {
    let base = Some(Path::new("/src/proj"));
    let rows = [
        ("a/../b", TriVal::String("/src/proj/b".into())),
        ("../../..", TriVal::String("/".into())),
        ("/etc/./x", TriVal::String("/etc/x".into())),
        ("~/x", TriVal::Unknown),
        ("~bob/x", TriVal::Unknown),
    ];
    for (word, want) in rows {
        assert_eq!(resolve(word, base), want, "{word}");
    }
    assert_eq!(resolve("a", None), TriVal::Unknown);
    assert_eq!(resolve("/a", None), TriVal::String("/a".into()));
}

#[test]
fn path_functions_resolve_against_the_command_directory() {
    let env = env();
    let site = Site { env: &env, dir: Some(Path::new("/src/proj/sub")), host: &Host::Here };
    let ctx = Ctx { vars: HashMap::new(), site: Some(site), home: TriVal::Unknown };
    let rows = [
        ("in_project('..')", TriVal::Bool(true)),
        ("in_project('../../proj-evil/x')", TriVal::Bool(false)),
        ("in_project('/src/projx')", TriVal::Bool(false)),
        ("in_project('~bob/x')", TriVal::Unknown),
        ("is_device('/dev/null')", TriVal::Bool(true)),
        ("is_device('/devx')", TriVal::Bool(false)),
        ("path('a/../b')", TriVal::String("/src/proj/sub/b".into())),
        ("path(null)", TriVal::Null),
    ];
    for (expr, want) in rows {
        assert_eq!(eval_cel_with(expr, &ctx), want, "{expr}");
    }
    let site = Site { env: &env, dir: Some(Path::new("/src/proj")), host: &Host::Unknown };
    let ctx = Ctx { vars: HashMap::new(), site: Some(site), home: TriVal::Unknown };
    assert_eq!(eval_cel_with("in_project('/src/proj')", &ctx), TriVal::Unknown);
}

#[test]
fn with_functions_read_lists_destinations_and_the_home() {
    let ctx = Ctx {
        vars: HashMap::from([("user".to_string(), TriVal::String("u".into()))]),
        site: None,
        home: TriVal::String("/home/u".into()),
    };
    let rows = [
        ("last([])", TriVal::Null),
        (r#"last(["a", "b"])"#, TriVal::String("b".into())),
        ("host_of('prod')", TriVal::String("prod".into())),
        ("host_of('bob@prod')", TriVal::String("prod".into())),
        ("host_of('ssh://bob@prod:22')", TriVal::String("prod".into())),
        ("user_of('prod')", TriVal::Null),
        ("user_of('bob@prod')", TriVal::String("bob".into())),
        ("user_of('ssh://bob@prod:22')", TriVal::String("bob".into())),
        ("host_of(null)", TriVal::Null),
        ("home('u')", TriVal::String("/home/u".into())),
        ("home('root')", TriVal::Unknown),
        ("home(null)", TriVal::Unknown),
    ];
    for (expr, want) in rows {
        assert_eq!(eval_cel_with(expr, &ctx), want, "{expr}");
    }
    let open = vars(&[("xs", strings(&["a"], false))]);
    assert_eq!(eval_cel_with("last(xs)", &open), TriVal::Unknown);
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
    // `unknown` is a value, as `null` is; no variable binds it.
    let bound = vars(&[("unknown", TriVal::String("x".into()))]);
    assert_eq!(eval_cel_with("unknown", &bound), TriVal::Unknown);
    assert_eq!(eval_cel_with("true ? unknown : null", &bound), TriVal::Unknown);
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
