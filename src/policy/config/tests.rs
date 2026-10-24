use super::format::{ArgumentConfig, ParserConfig, ParserName, ParserSettings};
use super::*;

fn compile(toml: &str) -> Result<Compiled, String> {
    let builtin = Source {
        config: parse_config(BUILTIN_TOML)?,
        is_project: false,
    };
    let project = Source {
        config: parse_config(toml)?,
        is_project: true,
    };
    compile_sources(&[builtin, project])
}

#[test]
fn the_builtin_policy_loads() {
    let config = parse_config(BUILTIN_TOML).unwrap();
    assert_eq!(config.default.as_deref(), Some("ask"));
    let builtin = Source {
        config,
        is_project: false,
    };
    if let Err(err) = compile_sources(&[builtin]) {
        panic!("the built-in policy did not load: {err}");
    }
}

#[test]
fn a_later_source_sets_the_default_and_the_project_dirs() {
    let (rules, _, _, dirs) =
        compile("default = \"deny\"\nproject_dirs = [\".\", \"../lib\"]").unwrap();
    assert_eq!(rules.default, Decision::Deny);
    assert_eq!(dirs, vec![".", "../lib"]);
}

#[test]
fn rules_keep_their_source_order() {
    let (rules, ..) = compile(
        r#"
        [[rules]]
        id = "first"
        action = "allow"
        [[rules]]
        id = "second"
        action = "deny"
        "#,
    )
    .unwrap();
    let names: Vec<_> = rules
        .rules
        .iter()
        .rev()
        .take(2)
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(names, ["second", "first"]);
}

/// One row per load error outside grammars, which `grammar` tests.
#[test]
fn a_source_that_breaks_the_format_does_not_load() {
    let rows = [
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nwhen = 'args.files == []'",
            "need a `command`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nwhen = 'subcommand == \"x\"'",
            "need a `command`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\ncommand = \"cat\"\nwhen = 'args.nope'",
            "`args.nope`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\ncommand = \"git frob\"",
            "`frob`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\ncommand = \"bench x\"",
            "no grammar",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\ncommand = \"scripts/bench\"",
            "`./`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nwhen = 'path == \"/\"'",
            "`path`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\npath = 'host == \"x\"'",
            "`host`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nwhen = 'user == \"root\"'",
            "`user`",
        ),
        ("[[rules]]\nid = \"x\"\naction = \"maybe\"", "maybe"),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nwhen = '@@'",
            "CEL",
        ),
        ("[[wrappers]]\nname = \"x\"", "wrappers"),
        ("[[rules]]\naction = \"allow\"", "`id`"),
        (
            "[[rules]]\nid = \"Bad Id\"\naction = \"allow\"",
            "lowercase",
        ),
        ("[[rules]]\nid = \"eval\"\naction = \"ask\"", "twice"),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nexempts = [\"nope\"]",
            "`nope`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nexempts = [\"file-tools\"]",
            "stricter",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"ask\"\nexempts = [\"rm-r\"]",
            "stricter",
        ),
        (
            "[[rules]]\nid = \"x\"\ncommand = \"cat\"\naction = \"allow\"\nexempts = [\"outside-project\"]",
            "per path",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\noverride = true",
            "override",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nvia = \"zsh\"",
            "expected `bash` or `tmux`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nvia = [\"tmux\", \"zsh\"]",
            "expected `bash` or `tmux`",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\nvia = []",
            "`via` names no channel",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\ncommand = []",
            "`command` is empty",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"allow\"\ncommand = [\"git stash\", \"git\"]",
            "one depth",
        ),
        (
            "[[rules]]\nid = \"x\"\naction = \"ask\"\ncommand = [\"kill\", \"pkill\"]\nwhen = 'args.signal_number != []'",
            "`args.signal_number` is not declared by `pkill`",
        ),
    ];
    for (toml, want) in rows {
        match compile(toml) {
            Ok(_) => panic!("loaded: {toml}"),
            Err(err) => assert!(
                err.contains(want),
                "{toml}\n  error: {err}\n  wanted: {want}"
            ),
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

    assert_eq!(
        project_root(root.join("a/b").to_str().unwrap()),
        Some(root.clone())
    );
    assert_eq!(project_root(dir.path().to_str().unwrap()), None);
}

#[test]
fn a_changed_policy_file_announces_its_reload() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join(".claude/tmux-mcp.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "").unwrap();
    let cwd = dir.path().to_str().unwrap();
    let engine = PolicyEngine::new(Some(cwd));
    let reloads = engine.reloads();

    engine.check_reload(Some(cwd));
    let unchanged = reloads.has_changed().unwrap();
    std::fs::write(&file, "default = \"ask\"\n").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&file)
        .unwrap()
        .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(1))
        .unwrap();
    engine.check_reload(Some(cwd));

    assert!(!unchanged);
    assert!(reloads.has_changed().unwrap());
}

/// A Nix switch relinks a policy file to a new store path, whose mtime is the
/// same as every store path's.
#[test]
fn a_policy_file_relinked_to_a_file_of_the_same_mtime_reloads() {
    let dir = tempfile::tempdir().unwrap();
    let store_file = |name: &str, toml: &str| {
        let path = dir.path().join(name);
        std::fs::write(&path, toml).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();
        path
    };
    let old = store_file("old.toml", "");
    let new = store_file("new.toml", "default = \"deny\"\n");
    let link = dir.path().join(".claude/tmux-mcp.toml");
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&old, &link).unwrap();
    let cwd = dir.path().to_str().unwrap();
    let engine = PolicyEngine::new(Some(cwd));
    let reloads = engine.reloads();

    // Relinked as a switch does: a new link renamed over the old one.
    let staged = dir.path().join(".claude/tmux-mcp.toml.new");
    std::os::unix::fs::symlink(&new, &staged).unwrap();
    std::fs::rename(&staged, &link).unwrap();
    engine.check_reload(Some(cwd));

    assert!(reloads.has_changed().unwrap());
}

#[test]
fn a_pi_policy_file_also_marks_a_project_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("proj");
    std::fs::create_dir_all(root.join(".pi")).unwrap();
    std::fs::create_dir_all(root.join("a/b")).unwrap();
    std::fs::write(root.join(".pi/tmux-mcp.toml"), "").unwrap();

    assert_eq!(
        project_root(root.join("a/b").to_str().unwrap()),
        Some(root.clone())
    );
}

#[test]
fn config_candidates_cover_pi_and_claude() {
    let [first, second] = home_config_candidates("/home/user");
    let all = format!("{} {}", first.display(), second.display());
    assert!(all.contains(".pi/agent/tmux-mcp.toml"));
    assert!(all.contains(".claude/tmux-mcp.toml"));
    assert_ne!(first, second);

    let [first, second] = project_config_candidates(Path::new("/home/user/project"));
    let all = format!("{} {}", first.display(), second.display());
    assert!(all.contains(".pi/tmux-mcp.toml"));
    assert!(all.contains(".claude/tmux-mcp.toml"));
    assert_ne!(first, second);
}

// --- Dependency contract: toml and serde ---

#[test]
fn a_policy_file_may_use_toml_1_1_inline_tables() {
    let toml = r#"
        rules = [
          {
            id = "my-tools",
            when = 'command.name == "cargo"',
            action = "allow",
          },
        ]
    "#;
    let config = parse_config(toml).unwrap();
    assert_eq!(config.rules[0].id, "my-tools");
}

#[test]
fn serde_reads_argument_settings_beside_flattened_levels() {
    let config = parse_config(
        r#"
        [commands.git]
        parser = { name = "gnu", short_equals = true }
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
    assert!(matches!(
        git.parser,
        Some(ParserConfig::Settings(ParserSettings {
            name: ParserName::Gnu,
            ..
        }))
    ));
    assert!(matches!(&git.arguments[1], ArgumentConfig::Group(g) if g.len() == 2));
    assert_eq!(
        git.argument["dir"].default.as_deref(),
        Some(&[".".to_string()][..])
    );
    assert_eq!(git.marks.cwd, ["dir"]);
    assert_eq!(git.subcommands.keys().collect::<Vec<_>>(), ["log"]);
    assert!(git.subcommands["log"].parser.is_none());
}
