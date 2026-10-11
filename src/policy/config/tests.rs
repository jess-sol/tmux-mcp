use super::format::{ArgumentConfig, ParserConfig, ParserName, ParserSettings};
use super::*;

fn compile(toml: &str) -> Result<Compiled, String> {
    let builtin = Source { config: parse_config(BUILTIN_TOML)?, is_project: false };
    let project = Source { config: parse_config(toml)?, is_project: true };
    compile_sources(&[builtin, project])
}

#[test]
fn the_builtin_policy_loads() {
    let config = parse_config(BUILTIN_TOML).unwrap();
    assert_eq!(config.default.as_deref(), Some("ask"));
    let builtin = Source { config, is_project: false };
    if let Err(err) = compile_sources(&[builtin]) {
        panic!("the built-in policy did not load: {err}");
    }
}

#[test]
fn a_later_source_sets_the_default_and_the_project_dirs() {
    let (rules, _, _, dirs) = compile("default = \"deny\"\nproject_dirs = [\".\", \"../lib\"]").unwrap();
    assert_eq!(rules.default, Decision::Deny);
    assert_eq!(dirs, vec![".", "../lib"]);
}

#[test]
fn rules_keep_their_source_order() {
    let (rules, ..) = compile(
        r#"
        [[rules]]
        description = "first"
        action = "allow"
        [[rules]]
        description = "second"
        action = "deny"
        "#,
    )
    .unwrap();
    let names: Vec<_> = rules.rules.iter().rev().take(2).map(|r| r.description.as_str()).collect();
    assert_eq!(names, ["second", "first"]);
}

/// One row per load error outside grammars, which `grammar` tests.
#[test]
fn a_source_that_breaks_the_format_does_not_load() {
    let rows = [
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\nwhen = 'args.files == []'", "need a `command`"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\nwhen = 'subcommand == \"x\"'", "need a `command`"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\ncommand = \"cat\"\nwhen = 'args.nope'", "`args.nope`"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\ncommand = \"git frob\"", "`frob`"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\ncommand = \"bench x\"", "no grammar"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\ncommand = \"scripts/bench\"", "`./`"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\nwhen = 'path == \"/\"'", "`path`"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\npath = 'host == \"x\"'", "`host`"),
        ("[[rules]]\ndescription = \"x\"\naction = \"maybe\"", "maybe"),
        ("[[rules]]\ndescription = \"x\"\naction = \"allow\"\nwhen = '@@'", "CEL"),
        ("[[wrappers]]\nname = \"x\"", "wrappers"),
    ];
    for (toml, want) in rows {
        match compile(toml) {
            Ok(_) => panic!("loaded: {toml}"),
            Err(err) => assert!(err.contains(want), "{toml}\n  error: {err}\n  wanted: {want}"),
        }
    }
}

#[test]
fn the_project_root_is_the_nearest_directory_with_a_policy() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(root.join(".claude")).unwrap();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join(".claude/tmux-mcp.toml"), "").unwrap();

    assert_eq!(project_root(root.join("a/b").to_str().unwrap()), Some(root.clone()));
    assert_eq!(project_root(dir.path().to_str().unwrap()), None);
}

// --- Dependency contract: toml and serde ---

#[test]
fn a_policy_file_may_use_toml_1_1_inline_tables() {
    let toml = r#"
        rules = [
          {
            description = "my tools",
            when = 'command.name == "cargo"',
            action = "allow",
          },
        ]
    "#;
    let config = parse_config(toml).unwrap();
    assert_eq!(config.rules[0].description, "my tools");
}

#[test]
fn serde_reads_argument_settings_beside_flattened_levels() {
    let config = parse_config(
        r#"
        [commands.git]
        parser = { name = "gnu", number = "max_count" }
        arguments = ["dir=-C:", ["pattern", "-e:"], "subcommand"]
        argument.dir.default = ["."]
        argument.subcommand.words = ["add"]
        marks.cwd = ["dir"]

        [commands.git.log]
        arguments = ["files..."]
        "#,
    )
    .unwrap();
    let git = &config.commands["git"];
    assert!(matches!(git.parser, Some(ParserConfig::Settings(ParserSettings { name: ParserName::Gnu, .. }))));
    assert!(matches!(&git.arguments[1], ArgumentConfig::Group(g) if g.len() == 2));
    assert_eq!(git.argument["dir"].default.as_deref(), Some(&[".".to_string()][..]));
    assert_eq!(git.marks.cwd, ["dir"]);
    assert_eq!(git.subcommands.keys().collect::<Vec<_>>(), ["log"]);
    assert!(git.subcommands["log"].parser.is_none());
}
