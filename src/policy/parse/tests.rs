use brush_parser::word::{BraceExpressionOrText, WordPiece};

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
fn brush_parses_compound_commands() {
    let first = |line: &str| match &parse_program(line).complete_commands[0].0[0].0.first.seq[0] {
        ast::Command::Compound(c, _) => std::mem::discriminant(c),
        _ => panic!("{line}: expected a compound command"),
    };
    assert_ne!(first("(ls)"), first("{ ls; }"));
    assert_ne!(first("if true; then echo yes; fi"), first("for x in a b; do echo $x; done"));
}

#[test]
fn brush_keeps_a_quoted_pipe_in_one_word() {
    let pipeline = &parse_program("echo 'hello | world'").complete_commands[0].0[0].0.first;
    assert_eq!(pipeline.seq.len(), 1);
}

#[test]
fn brush_rejects_unclosed_quote() {
    assert!(parse_with_brush("echo 'hello").is_err());
}

fn simple(line: &str) -> ast::SimpleCommand {
    match &parse_program(line).complete_commands[0].0[0].0.first.seq[0] {
        ast::Command::Simple(s) => s.clone(),
        _ => panic!("{line}: expected a simple command"),
    }
}

#[test]
fn brush_reads_assignments_as_their_own_items() {
    let kinds = |line: &str| {
        let s = simple(line);
        let items = s.prefix.iter().flat_map(|p| &p.0).chain(s.suffix.iter().flat_map(|p| &p.0));
        items.map(|i| matches!(i, ast::CommandPrefixOrSuffixItem::AssignmentWord(..))).collect::<Vec<_>>()
    };
    assert_eq!(kinds("X=1 Y=2 ls a"), [true, true, false]);
    // After the name too, though only the command gives it a meaning.
    assert_eq!(kinds("export X=1 Y"), [true, false]);
    assert_eq!(kinds("env X=1 ls a=b"), [true, false, true]);
    assert!(simple("X=1").word_or_name.is_none());
}

#[test]
fn brush_gives_each_word_piece_its_source_span() {
    let options = brush_parser::ParserOptions::default();
    let raw = "a${x:-$(eval true)}\"$((1 + $(id)))\"";
    let pieces = brush_parser::word::parse(raw, &options).unwrap();
    assert_eq!(&raw[pieces[1].start_index..pieces[1].end_index], "${x:-$(eval true)}");
    let WordPiece::DoubleQuotedSequence(inner) = &pieces[2].piece else { panic!("{:?}", pieces[2]) };
    assert!(matches!(&inner[0].piece, WordPiece::ArithmeticExpression(e) if e.value.contains("$(id)")));
    let raw = "\"a${x:-$(id)}\"";
    let pieces = brush_parser::word::parse(raw, &options).unwrap();
    let WordPiece::DoubleQuotedSequence(inner) = &pieces[0].piece else { panic!("{:?}", pieces[0]) };
    assert_eq!(&raw[inner[1].start_index..inner[1].end_index], "${x:-$(id)}");
}

#[test]
fn brush_keeps_a_here_document_body_as_a_word() {
    let s = simple("cat <<EOF\n$(id)\nEOF\n");
    let docs: Vec<_> = s
        .suffix
        .iter()
        .flat_map(|p| &p.0)
        .filter_map(|i| match i {
            ast::CommandPrefixOrSuffixItem::IoRedirect(ast::IoRedirect::HereDocument(_, d)) => Some(d.clone()),
            _ => None,
        })
        .collect();
    assert!(docs[0].requires_expansion && docs[0].doc.value.contains("$(id)"));
    let quoted = simple("cat <<'EOF'\n$(id)\nEOF\n");
    assert!(quoted.suffix.iter().flat_map(|p| &p.0).any(|i| matches!(
        i,
        ast::CommandPrefixOrSuffixItem::IoRedirect(ast::IoRedirect::HereDocument(_, d)) if !d.requires_expansion
    )));
}

#[test]
fn brush_finds_brace_expressions_outside_quotes_only() {
    let options = brush_parser::ParserOptions::default();
    let has_expr = |w: &str| {
        brush_parser::word::parse_brace_expansions(w, &options)
            .unwrap()
            .is_some_and(|p| p.iter().any(|p| matches!(p, BraceExpressionOrText::Expr(_))))
    };
    assert!(has_expr("{a,b}"));
    assert!(has_expr("x{1..3}"));
    assert!(!has_expr("'{a,b}'"));
    assert!(!has_expr("{}"));
    assert!(!has_expr("@{u}"));
}

// --- Words ---

#[test]
fn a_word_loses_its_quotes_and_an_expansion_is_unknown() {
    let known = |w: &str| Word::Known(w.to_string());
    let home = s("/home/u");
    let rows = [
        ("plain", known("plain")),
        ("'eval'", known("eval")),
        ("\"/etc/shadow\"", known("/etc/shadow")),
        ("a\\ b", known("a b")),
        ("src/*.rs", known("src/*.rs")),
        ("~", known("/home/u")),
        ("~/x", known("/home/u/x")),
        ("'~/x'", known("~/x")),
        ("~bob/x", known("~bob/x")),
        ("{}", known("{}")),
        ("@{u}..", known("@{u}..")),
        ("$X", Word::Unknown { splits: true }),
        ("\"$X\"", Word::Unknown { splits: false }),
        ("\"$@\"", Word::Unknown { splits: true }),
        ("a$(date)", Word::Unknown { splits: true }),
        ("{notes,/etc/shadow}", Word::Unknown { splits: true }),
    ];
    for (raw, want) in rows {
        assert_eq!(word(raw, &home), want, "{raw}");
    }
    assert_eq!(word("~/x", &TriVal::Unknown), Word::Unknown { splits: false });
}

// --- Commands of a line ---

fn s(v: &str) -> TriVal {
    TriVal::String(v.into())
}

/// A shell in `/p/q` on this machine, as the user `u`.
fn start() -> Context {
    Context { cwd: s("/p/q"), host: s("here"), user: s("u"), home: s("/home/u"), own_user: true }
}

fn parse(line: &str) -> Vec<CommandInfo> {
    let grammars = crate::policy::config::builtin_grammars();
    parse_command(line, &grammars, &start()).unwrap_or_else(|e| panic!("{line}: {}", e.message))
}

/// The last command of a line named `name`.
fn command(cmds: &[CommandInfo], name: &str) -> CommandInfo {
    cmds.iter().rev().find(|c| c.name == s(name)).cloned().unwrap_or_else(|| panic!("no {name} in {cmds:?}"))
}

/// Each command's name, sorted: `-` for none, `?` for Unknown.
fn names(line: &str) -> Vec<String> {
    let commands = parse_command(line, &Grammars::default(), &start()).unwrap_or_else(|e| panic!("{line}: {}", e.message));
    let mut names: Vec<String> = commands
        .into_iter()
        .map(|c| match c.name {
            TriVal::String(n) => n,
            TriVal::Null => "-".into(),
            _ => "?".into(),
        })
        .collect();
    names.sort();
    names
}

#[test]
fn a_line_holds_every_command_it_runs() {
    let rows: &[(&str, &[&str])] = &[
        ("ls | grep foo | wc -l", &["grep", "ls", "wc"]),
        ("ls && echo done || echo failed", &["echo", "echo", "ls"]),
        ("echo 'hello | world'", &["echo"]),
        ("echo $(date)", &["date", "echo"]),
        ("(ls && pwd)", &["ls", "pwd"]),
        ("{ ls; pwd; }", &["ls", "pwd"]),
        ("time make -j4", &["make"]),
        ("X=1", &["-"]),
        ("> out", &["-"]),
        ("$X a", &["?"]),
        // Every word where a substitution can stand
        ("FOO=$(id) ls", &["id", "ls"]),
        ("echo ${x:-$(id)} \"${y#`date`}\"", &["date", "echo", "id"]),
        ("echo $((1 + $(id)))", &["echo", "id"]),
        ("echo hi > $(id)", &["echo", "id"]),
        ("{ echo; } > $(id)", &["echo", "id"]),
        ("cat <(id)", &["cat", "id"]),
        ("for i in $(id); do echo; done", &["echo", "id"]),
        ("case $(id) in $(date)) echo;; esac", &["date", "echo", "id"]),
        ("cat <<EOF\na \"$(id)\" \\$(date)\nEOF\n", &["cat", "date", "id"]),
        ("cat <<'EOF'\n$(id)\nEOF\n", &["cat"]),
        ("cat <<< $(id)", &["cat", "id"]),
    ];
    for (line, want) in rows {
        assert_eq!(names(line), *want, "{line}");
    }
    assert!(parse_command("", &Grammars::default(), &start()).unwrap().is_empty());
    assert!(parse_command("echo 'hello", &Grammars::default(), &start()).is_err());
}

#[test]
fn a_prefix_assignment_is_a_variable_of_its_command() {
    let cmds = parse_command("A=1 B[0]=2 env C=3 ls", &Grammars::default(), &start()).unwrap();
    let strings = |vals: &[&str]| TriVal::List { elements: vals.iter().map(|v| s(v)).collect(), exhaustive: true };
    assert_eq!(cmds[0].variables, strings(&["A", "B"]));
}

// --- Context and paths ---

#[test]
fn a_command_reads_and_writes_resolved_paths() {
    let paths = |vals: &[TriVal]| TriVal::List { elements: vals.to_vec(), exhaustive: true };
    let cat = command(&parse("cat a ../b /etc/./x ~/y '~/z' ~bob/w"), "cat");
    let want = [s("/p/q/a"), s("/p/b"), s("/etc/x"), s("/home/u/y"), TriVal::Unknown, TriVal::Unknown];
    assert_eq!((cat.cwd, cat.reads), (s("/p/q"), paths(&want)));
    let git = command(&parse("git -C sub log --output out f"), "git");
    assert_eq!((git.cwd, git.reads), (s("/p/q/sub"), paths(&[s("/p/q/sub/f"), s("/p/q/sub")])));
    let echo = command(&parse("echo hi > '/tmp/evil' 2>&1 < \"in\" >> $X"), "echo");
    let got: Vec<_> = echo.redirects.iter().map(|r| (r.target.clone(), r.is_write)).collect();
    assert_eq!(got, [(s("/tmp/evil"), true), (s("/p/q/in"), false), (TriVal::Unknown, true)]);
}

/// A cd holds for the commands that run only if it succeeded; a subshell,
/// a substitution and a pipeline stage are shells of their own.
#[test]
fn a_cd_moves_the_rest_of_its_and_chain() {
    let rows = [
        ("cat y", s("/p/q")),
        ("cd x && cat y", s("/p/q/x")),
        ("cd /a && cd ../b && cat y", s("/b")),
        ("cd && cat y", s("/home/u")),
        ("pushd x && cat y", s("/p/q/x")),
        ("cd x; cat y", TriVal::Unknown),
        ("cd x || cat y", TriVal::Unknown),
        ("cd x && true || cat y", TriVal::Unknown),
        ("cd - && cat y", TriVal::Unknown),
        ("cd $X && cat y", TriVal::Unknown),
        ("popd && cat y", TriVal::Unknown),
        ("pushd && cat y", TriVal::Unknown),
        ("pushd +1 && cat y", TriVal::Unknown),
        ("pushd -1 && cat y", TriVal::Unknown),
        ("pushd -n x && cat y", TriVal::Unknown),
        ("{ cd x; } && cat y", TriVal::Unknown),
        ("if true; then cd x; fi; cat y", TriVal::Unknown),
        ("for i in a; do cat y; cd x; done", TriVal::Unknown),
        ("f() { cd x; }; cat y", TriVal::Unknown),
        ("(cd x) && cat y", s("/p/q")),
        ("cd x | cat y", s("/p/q")),
        ("echo $(cd x) && cat y", s("/p/q")),
        ("sh -c 'cd x' && cat y", s("/p/q")),
        ("sh -c 'cd x && cat y'", s("/p/q/x")),
        ("cd x && sh -c 'cat y'", s("/p/q/x")),
    ];
    for (line, want) in rows {
        assert_eq!(command(&parse(line), "cat").cwd, want, "{line}");
    }
    let first_pass = parse("for i in a; do cat y; cd x; done");
    assert_eq!(first_pass.iter().find(|c| c.name == s("cat")).map(|c| c.cwd.clone()), Some(s("/p/q")));
}

/// The context of an *Inner command*: its outer command's, with the fields
/// that the outer command's `with` keys set.
#[test]
fn a_wrapper_gives_its_inner_command_the_context_its_with_sets() {
    let u = TriVal::Unknown;
    // (line, the inner command's cwd, host, user, home, own_user)
    let rows = [
        ("env X=1 ls", s("/p/q"), s("here"), s("u"), s("/home/u"), true),
        ("env -C sub ls", s("/p/q/sub"), s("here"), s("u"), s("/home/u"), true),
        ("env -C /etc ls", s("/etc"), s("here"), s("u"), s("/home/u"), true),
        ("env -C \"$X\" ls", u.clone(), s("here"), s("u"), s("/home/u"), true),
        ("sudo ls", s("/p/q"), s("here"), s("root"), u.clone(), false),
        ("sudo -u u ls", s("/p/q"), s("here"), s("u"), s("/home/u"), true),
        ("sudo -D sub ls", s("/p/q/sub"), s("here"), s("root"), u.clone(), false),
        ("sudo -i ls", u.clone(), s("here"), s("root"), u.clone(), false),
        ("sudo -u u -i ls", s("/home/u"), s("here"), s("u"), s("/home/u"), true),
        ("sudo -u \"$X\" ls", s("/p/q"), s("here"), u.clone(), u.clone(), false),
        ("ssh prod ls", u.clone(), s("prod"), s("u"), u.clone(), true),
        ("ssh bob@prod ls", u.clone(), s("prod"), s("bob"), u.clone(), false),
        ("ssh -l bob ssh://ann@prod:22 ls", u.clone(), s("prod"), s("bob"), u.clone(), false),
    ];
    for (line, cwd, host, user, home, own_user) in rows {
        let ls = command(&parse(line), "ls");
        let want = Context { cwd: cwd.clone(), host, user, home, own_user };
        assert_eq!((&ls.cwd, &ls.context), (&cwd, &want), "{line}");
    }
    let cat = command(&parse("ssh prod 'cat -- ~/x'"), "cat");
    assert_eq!(cat.reads, TriVal::List { elements: vec![u.clone()], exhaustive: true });
    let cat = command(&parse("sudo -u root sh -c 'cd /tmp && cat x'"), "cat");
    assert_eq!((cat.cwd, cat.context.user), (s("/tmp"), s("root")));
    // -execdir runs its command in each file's directory.
    assert_eq!(command(&parse("find . -execdir cat x ;"), "cat").cwd, u);
    assert_eq!(command(&parse("find . -exec cat x ;"), "cat").cwd, s("/p/q"));
}
