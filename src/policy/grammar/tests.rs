use super::*;
use crate::policy::config::parse_config;
use crate::policy::parse::word;

fn grammars(toml: &str) -> Result<Grammars, String> {
    let config = parse_config(toml)?;
    Grammars::compile([&config.commands])
}

fn parse(toml: &str, line: &str) -> Parse {
    let g = grammars(toml).unwrap_or_else(|e| panic!("{e}"));
    let home = s("/home/u");
    let mut words = line.split_whitespace().map(|w| word(w, &home));
    let name = match words.next() {
        Some(Word::Known(n)) => n,
        _ => panic!("no name in {line}"),
    };
    g.parse(&name, &words.collect::<Vec<_>>(), &home)
}

fn s(v: &str) -> TriVal {
    TriVal::String(v.into())
}

fn strings(vals: &[&str]) -> TriVal {
    list(vals.iter().map(|v| s(v)).collect(), true)
}

fn open(vals: &[&str]) -> TriVal {
    list(vals.iter().map(|v| s(v)).collect(), false)
}

/// `(line, part, value)` rows over one grammar.
fn check(toml: &str, rows: &[(&str, &str, TriVal)]) {
    for (line, part, want) in rows {
        let got = parse(toml, line)
            .args
            .get(*part)
            .cloned()
            .unwrap_or(TriVal::Unknown);
        assert_eq!(&got, want, "{line}: {part}");
    }
}

const SORT: &str = r#"
    [commands.sort]
    parser = "gnu"
    arguments = ["files...", "--output|-o:", "--reverse|-r", "--numeric-sort|-n"]
"#;

#[test]
fn the_gnu_parser_reads_options_anywhere_until_double_dash() {
    check(
        SORT,
        &[
            ("sort -o out a", "output", strings(&["out"])),
            ("sort -oout a", "output", strings(&["out"])),
            ("sort --output=out a", "output", strings(&["out"])),
            ("sort a -o out b", "files", strings(&["a", "b"])),
            ("sort -rno out a", "output", strings(&["out"])),
            ("sort -rno out a", "numeric_sort", TriVal::Bool(true)),
            ("sort --reverse=x a", "reverse", TriVal::Bool(true)),
            ("sort --reverse=x a", "files", strings(&["a"])),
            ("sort -- -o a", "files", strings(&["-o", "a"])),
            ("sort -- -o a", "output", strings(&[])),
            ("sort - a", "files", strings(&["-", "a"])),
        ],
    );
}

#[test]
fn an_option_matches_only_its_whole_spelling() {
    check(
        SORT,
        &[
            ("sort --out x a", "output", strings(&[])),
            ("sort --out x a", "files", TriVal::Unknown),
            ("sort --rev", "reverse", TriVal::Bool(false)),
        ],
    );
}

#[test]
fn the_posix_parser_stops_options_at_the_first_operand() {
    let toml = r#"
        [commands.p]
        parser = "posix"
        arguments = ["files...", "--force|-f"]
    "#;
    check(
        toml,
        &[
            ("p -f a -f", "files", strings(&["a", "-f"])),
            ("p a -f", "force", TriVal::Bool(false)),
        ],
    );
}

#[test]
fn parser_settings_copy_one_tool() {
    let head = r#"
        [commands.head]
        parser = { name = "gnu", leading_number = "lines" }
        arguments = ["files...", "--lines|-n:"]
    "#;
    let log = r#"
        [commands.log]
        parser = { name = "gnu", number = "max_count" }
        arguments = ["files...", "--max-count|-n:"]
    "#;
    let pflag = r#"
        [commands.k]
        parser = { name = "gnu", short_equals = true }
        arguments = ["files...", "--expr|-e:"]
    "#;
    check(
        head,
        &[
            ("head -5 a", "lines", strings(&["5"])),
            ("head +5 a", "lines", strings(&["+5"])),
        ],
    );
    check(
        head,
        &[
            ("head a -5", "files", strings(&["a"])),
            ("head a -5", "lines", strings(&[])),
        ],
    );
    check(log, &[("log a -5", "max_count", strings(&["5"]))]);
    check(pflag, &[("k -e=x", "expr", strings(&["x"]))]);
    check(SORT, &[("sort -o=x", "output", strings(&["=x"]))]);
}

#[test]
fn the_find_parser_reads_single_dash_words_and_operators() {
    let toml = r#"
        [commands.find]
        parser = "find"
        arguments = ["files...", "-delete", "-name:", "-newer:", "follow=-L"]
        argument.files.default = ["."]
        marks.read = ["files", "newer"]
    "#;
    check(
        toml,
        &[
            ("find a b -name x", "files", strings(&["a", "b"])),
            (
                "find ( -name x -o ! -newer y ) -delete",
                "delete",
                TriVal::Bool(true),
            ),
            ("find ( -name x -o -newer y )", "newer", strings(&["y"])),
            ("find -name x", "files", strings(&["."])),
            ("find -L a -name x", "files", strings(&["a"])),
            ("find . -exec rm {} ;", "files", TriVal::Unknown),
            ("find . -exec rm {} ;", "delete", TriVal::Bool(false)),
            ("find . -exec rm -delete ;", "delete", TriVal::Unknown),
        ],
    );
    assert_eq!(parse(toml, "find a -newer b").reads, strings(&["a", "b"]));
}

const CP: &str = r#"
    [commands.cp]
    parser = "gnu"
    arguments = ["files...", ["dest", "--target-directory|-t:"]]
    marks.read = ["files"]
    marks.write = ["dest", "target_directory"]
"#;

#[test]
fn a_group_option_takes_the_place_of_its_operand() {
    check(
        CP,
        &[
            ("cp a b c", "files", strings(&["a", "b"])),
            ("cp a b c", "dest", s("c")),
            ("cp -t d a b", "files", strings(&["a", "b"])),
            ("cp -t d a b", "dest", TriVal::Null),
            ("cp a b -t d", "files", strings(&["a", "b"])),
        ],
    );
    let p = parse(CP, "cp -t/tmp/x a");
    assert_eq!((p.reads, p.writes), (strings(&["a"]), strings(&["/tmp/x"])));
}

const UNIQ: &str = r#"
    [commands.u]
    parser = "gnu"
    arguments = ["input", "output", "--force|-f", "--skip|-s:"]
    marks.read = ["input"]
    marks.write = ["output"]
"#;

/// A word the grammar cannot place leaves the operands Unknown; options
/// stay known unless a word could hide one.
#[test]
fn an_extra_operand_leaves_options_known() {
    check(
        UNIQ,
        &[
            ("u a b", "input", s("a")),
            ("u a b c", "input", TriVal::Unknown),
            ("u a b c", "force", TriVal::Bool(false)),
            ("u -f a b c", "force", TriVal::Bool(true)),
            ("u a b c", "skip", strings(&[])),
        ],
    );
    assert_eq!(parse(UNIQ, "u a b").writes, strings(&["b"]));
    assert_eq!(
        parse(UNIQ, "u a b c").writes,
        list(vec![TriVal::Unknown], false)
    );
}

/// After an undeclared option, a later word may be its value: what is
/// given from there reads Unknown, and what no word gives stays known.
#[test]
fn an_undeclared_option_leaves_what_follows_it_unknown() {
    check(
        UNIQ,
        &[
            ("u -x a b", "input", TriVal::Unknown),
            ("u -x a b", "force", TriVal::Bool(false)),
            ("u -x a -f b", "force", TriVal::Unknown),
            ("u -f -x a", "force", TriVal::Bool(true)),
            ("u -x a -s v", "skip", open(&[])),
            ("u -xf a", "force", TriVal::Unknown),
            ("u a b -x", "input", s("a")),
            ("u a b --x=y", "input", TriVal::Unknown),
        ],
    );
    assert_eq!(parse(UNIQ, "u a b -x").reads, strings(&["a"]));
    assert_eq!(
        parse(UNIQ, "u -x a b").reads,
        list(vec![TriVal::Unknown], false)
    );
}

/// A word that cannot be known may be any option, `--` or several words.
#[test]
fn an_unknown_word_leaves_everything_it_can_reach_unknown() {
    check(
        UNIQ,
        &[
            ("u a $X", "force", TriVal::Unknown),
            ("u a $X", "output", TriVal::Unknown),
            ("u a -s \"$X\"", "skip", list(vec![TriVal::Unknown], true)),
            ("u a -s \"$X\"", "force", TriVal::Bool(false)),
            ("u a -s $X", "skip", open(&[])),
        ],
    );
}

#[test]
fn an_operand_with_a_match_takes_only_a_matching_word() {
    let toml = r#"
        [commands.cargo]
        parser = "gnu"
        arguments = ["toolchain", "subcommand", "rest..."]
        argument.toolchain.match = "+*"
        argument.subcommand.words = ["build"]
    "#;
    check(
        toml,
        &[
            ("cargo +nightly build x", "toolchain", s("+nightly")),
            ("cargo build x", "toolchain", TriVal::Null),
            ("cargo build x", "rest", strings(&["x"])),
        ],
    );
}

const GIT: &str = r#"
    [commands.git]
    parser = "gnu"
    arguments = ["dir=-C:", "config=-c:", "subcommand"]
    argument.dir.default = ["."]
    argument.subcommand.words = ["add"]
    marks.cwd = ["dir"]

    [commands.git.log]
    parser = { name = "gnu", number = "max_count" }
    arguments = ["files...", "--output:", "find_copies=-C", "--max-count|-n:"]
    marks.read = ["files"]
    marks.write = ["output"]

    [commands.git.stash]
    arguments = ["subcommand", "names..."]
    argument.subcommand.words = ["list", "drop"]

    [commands.cargo]
    parser = "gnu"
    arguments = ["subcommand", "rest..."]
    argument.subcommand.words = ["test"]
    global = ["--manifest-path:"]
    marks.read = ["manifest_path"]
"#;

#[test]
fn subcommand_words_choose_the_level_that_parses_the_rest() {
    let rows = [
        ("git log", strings(&["log"])),
        ("git -C x stash drop y", strings(&["stash", "drop"])),
        ("git add x", strings(&["add"])),
        ("git", strings(&[])),
        ("git frob x", list(vec![TriVal::Unknown], true)),
        ("git $X log", list(vec![TriVal::Unknown], true)),
        (
            "git stash $X",
            list(vec![s("stash"), TriVal::Unknown], true),
        ),
    ];
    for (line, want) in rows {
        assert_eq!(parse(GIT, line).subcommands, want, "{line}");
    }
    check(
        GIT,
        &[
            ("git -C x log -C", "find_copies", TriVal::Bool(true)),
            ("git -C x log -C", "dir", strings(&["x"])),
            ("git log -C x", "dir", strings(&["."])),
            ("git log -C x", "files", strings(&["x"])),
            ("git stash drop a b", "names", strings(&["a", "b"])),
            ("git frob x", "dir", strings(&["."])),
            ("git frob x", "config", strings(&[])),
            ("git $X log", "config", open(&[])),
            ("git --frob log", "config", strings(&[])),
            ("git --frob -c x log", "config", open(&[])),
            (
                "cargo test --manifest-path m",
                "manifest_path",
                strings(&["m"]),
            ),
        ],
    );
}

#[test]
fn marks_turn_parts_into_paths_relative_to_the_marked_directory() {
    let p = parse(GIT, "git -C a -C b log --output o f");
    assert_eq!(
        (p.cwd, p.reads, p.writes),
        (s("a/b"), strings(&["f", "."]), strings(&["o"]))
    );
    let p = parse(GIT, "git -C / log");
    assert_eq!((p.cwd, p.reads), (s("/"), strings(&["."])));
    let p = parse(GIT, "git log $X");
    assert_eq!((p.cwd, p.reads), (s("."), open(&["."])));
    let p = parse(GIT, "git frob");
    assert_eq!((p.cwd, p.reads), (s("."), open(&["."])));
    assert_eq!(parse(GIT, "git $X log").cwd, TriVal::Unknown);
}

const WRAPPERS: &str = r#"
    [commands.timeout]
    parser = "posix"
    arguments = ["duration", "command...", "--signal|-s:"]
    marks.command = ["command"]

    [commands.xargs]
    parser = "posix"
    arguments = ["command...", "--max-args|-n:"]
    argument.command.appends = true
    marks.command = ["command"]

    [commands.sh]
    parser = "posix"
    arguments = [["file", "command=-c:"], "positional..."]
    marks.shell = ["command"]

    [commands.find]
    parser = "find"
    arguments = ["files...", "-name:", "-exec...", "-delete"]
    marks.command = ["exec"]

    [commands.env]
    parser = "posix"
    arguments = ["assigns...", "command...", "--chdir|-C:"]
    argument.assigns.match = "*=*"
    marks.variables = ["assigns"]
    marks.command = ["command"]
    with.cwd = 'last(args.chdir)'
"#;

#[test]
fn marks_give_the_inner_commands_that_a_command_runs() {
    let k = |w: &str| Word::Known(w.into());
    let u = || Word::Unknown { splits: true };
    let words = |ws: &[Word]| Inner::Words(ws.to_vec());
    let rows = [
        ("timeout 5 ls -l", vec![words(&[k("ls"), k("-l")])]),
        ("timeout 5", vec![]),
        ("timeout --frob 5 ls", vec![words(&[u()])]),
        ("timeout 5 $X", vec![words(&[u()])]),
        ("xargs rm -f", vec![words(&[k("rm"), k("-f"), u()])]),
        ("sh -c date", vec![Inner::Line(Some("date".into()))]),
        ("sh -c $X", vec![Inner::Line(None)]),
        ("sh script", vec![]),
        ("find . -exec rm {} ; -name x", vec![words(&[k("rm"), u()])]),
        (
            "find . -exec ls {} + -exec cp {}.bak d ;",
            vec![words(&[k("ls"), u()]), words(&[k("cp"), u(), k("d")])],
        ),
        ("find . -exec rm", vec![words(&[k("rm")])]),
        // `$X` may hold the `;`, and more `-exec` uses after it.
        (
            "find . -exec rm $X ; -delete",
            vec![words(&[k("rm"), u()]), words(&[u()])],
        ),
        ("find . -name $X", vec![words(&[u()])]),
        ("env A=1 ls", vec![words(&[k("ls")])]),
    ];
    for (line, want) in rows {
        assert_eq!(parse(WRAPPERS, line).inner, want, "{line}");
    }
    check(
        WRAPPERS,
        &[("find . -exec rm {} ; -name x", "name", strings(&["x"]))],
    );
}

#[test]
fn marks_name_the_variables_that_a_command_sets() {
    let p = parse(WRAPPERS, "env A=1 B ls");
    assert_eq!(p.variables, strings(&["A"]));
    assert_eq!(
        parse(WRAPPERS, "env --frob A=1 ls").variables,
        list(vec![], false)
    );
}

#[test]
fn a_default_is_read_as_a_word() {
    let toml = "[commands.cd]\nparser = \"posix\"\narguments = [\"dir\"]\nargument.dir.default = [\"~\"]\nmarks.cwd = [\"dir\"]";
    assert_eq!(parse(toml, "cd").cwd, s("/home/u"));
    assert_eq!(parse(toml, "cd x").cwd, s("x"));
}

#[test]
fn a_command_finds_its_grammar_by_its_last_path_component() {
    assert_eq!(
        parse(GIT, "/usr/bin/git log").subcommands,
        strings(&["log"])
    );
    assert_eq!(parse(GIT, "./bin/git log").subcommands, strings(&["log"]));
    assert_eq!(parse(GIT, "gti log").subcommands, strings(&[]));
}

#[test]
fn a_rule_command_declares_the_names_of_its_levels() {
    let g = grammars(GIT).unwrap();
    let path = |p: &str| p.split(' ').map(String::from).collect::<Vec<_>>();
    let names = g.declared(&path("git stash")).unwrap();
    assert!(names.contains("dir") && names.contains("names") && !names.contains("output"));
    assert!(g.declared(&path("git add")).is_ok());
    assert!(g.declared(&path("git add x")).is_err());
    assert!(g.declared(&path("git frob")).is_err());
    assert!(g.declared(&path("bench x")).is_err());
    assert!(g.declared(&path("bench")).unwrap().is_empty());
}

#[test]
fn a_grammar_that_breaks_the_format_does_not_load() {
    let rows = [
        ("arguments = [\"x\"]", "needs `parser`"),
        (
            "parser = \"gnu\"\narguments = [\"--a|b\"]",
            "not an argument string",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x:\"]",
            "not an argument string",
        ),
        (
            "parser = \"gnu\"\narguments = [\"force=--force\"]",
            "repeats the name",
        ),
        ("parser = \"gnu\"\narguments = [\"-C:\"]", "not snake_case"),
        (
            "parser = \"gnu\"\narguments = [\"-newer:\"]",
            "single-dash word",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x\", \"x...\"]",
            "declared twice",
        ),
        (
            "parser = \"gnu\"\nglobal = [\"x\"]",
            "`global` holds options",
        ),
        (
            "parser = \"gnu\"\narguments = [[\"x\", \"--y\"]]",
            "a group is",
        ),
        (
            "parser = \"gnu\"\narguments = [[\"x...\", \"--y:\"]]",
            "a group is",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand...\"]",
            "takes no `...`",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand\", \"subcommand\"]",
            "twice",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x...\", \"subcommand\"]\nargument.subcommand.words = [\"a\"]",
            "list operand",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand\"]",
            "no words and no tables",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x\"]\n[commands.t.sub]\narguments = []",
            "without `subcommand`",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand\"]\nargument.subcommand.words = [\"a\"]\n[commands.t.a]\narguments = []",
            "both in `words` and a table",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand\"]\n[commands.t.with]\narguments = []",
            "unknown field",
        ),
        (
            "parser = \"gnu\"\narguments = [\"--run...\"]",
            "only for the `find` parser",
        ),
        (
            "parser = \"gnu\"\narguments = [\"xs...\"]\nargument.xs.appends = true",
            "`marks.command` does not name",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x\"]\nmarks.command = [\"x\"]",
            "a one-word operand",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x\"]\nmarks.shell = [\"y\"]",
            "does not declare",
        ),
        ("parser = \"gnu\"\nwith.home = 'x'", "unknown field"),
        ("parser = \"gnu\"\nwith.cwd = '@@'", "CEL"),
        ("parser = \"gnu\"\nwith.cwd = 'path'", "`path`"),
        (
            "parser = \"gnu\"\narguments = [\"x\"]\nwith.user = 'args.y'",
            "`args.y`",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand\"]\n[commands.t.a]\narguments = [\"x\"]\n[commands.t.b]\nwith.cwd = 'args.x'",
            "`args.x`",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x\"]\nargument.x.words = [\"a\"]",
            "only for `subcommand`",
        ),
        (
            "parser = \"gnu\"\narguments = [\"--x:\"]\nargument.x.match = \"*\"",
            "only for an operand",
        ),
        (
            "parser = \"gnu\"\narguments = [\"--x\"]\nargument.x.default = [\"a\"]",
            "only for an operand or an option",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x\"]\nargument.y.default = [\"a\"]",
            "names no argument",
        ),
        (
            "parser = \"gnu\"\narguments = [\"x\"]\nmarks.read = [\"y\"]",
            "does not declare",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand\"]\nglobal = [\"--g\"]\n[commands.t.a]\narguments = [\"other=--g\"]",
            "already a global option",
        ),
        (
            "parser = \"gnu\"\narguments = [\"subcommand\", \"x\"]\n[commands.t.a]\narguments = [\"x\"]",
            "declared twice along",
        ),
    ];
    for (body, want) in rows {
        let toml = format!("[commands.t]\n{body}");
        match grammars(&toml) {
            Ok(_) => panic!("loaded:\n{toml}"),
            Err(err) => assert!(
                err.contains(want),
                "{toml}\n  error: {err}\n  wanted: {want}"
            ),
        }
    }
    assert!(grammars("[commands.t]\nparser = \"gnu\"\nwith.cwd = 'unknown'").is_ok());
}
