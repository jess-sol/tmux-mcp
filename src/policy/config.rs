//! Config loading: TOML parsing, 3-source merge, file watching for live reload.
//!
//! Three sources, one format:
//! - Built-in: compiled into binary via include_str!
//! - Home: ~/.claude/tmux-mcp.toml
//! - Project: <project root>/.claude/tmux-mcp.toml, found from the pane's directory

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use serde::Deserialize;

use super::Decision;
use super::grammar::Grammars;
use super::rules::{self, RuleSet};

// --- Types ---

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfig {
    #[serde(default)]
    pub default: Option<String>,
    /// The project directories, resolved against the project root.
    #[serde(default)]
    pub project_dirs: Option<Vec<String>>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub commands: BTreeMap<String, GrammarConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleConfig {
    pub description: String,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub write: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    pub action: String,
    /// An *Override rule*: when any matches, the strictest matching override
    /// decides and the other rules do not count.
    #[serde(default, rename = "override")]
    pub is_override: bool,
    #[serde(default)]
    pub message: Option<String>,
}

/// One level of a grammar: `[commands.<name>]` or `[commands.<name>.<word>]`.
#[derive(Debug, Default, Deserialize)]
pub struct GrammarConfig {
    #[serde(default)]
    pub parser: Option<ParserConfig>,
    #[serde(default)]
    pub arguments: Vec<ArgumentConfig>,
    #[serde(default)]
    pub global: Vec<ArgumentConfig>,
    #[serde(default)]
    pub argument: BTreeMap<String, ArgumentSettings>,
    #[serde(default)]
    pub marks: MarksConfig,
    #[serde(default)]
    pub with: ContextConfig,
    #[serde(flatten)]
    pub subcommands: BTreeMap<String, GrammarConfig>,
}

/// CEL for the context of a level's *Inner command*s: one per field it changes.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextConfig {
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ParserConfig {
    Name(ParserName),
    Settings(ParserSettings),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParserSettings {
    pub name: ParserName,
    #[serde(default)]
    pub leading_number: Option<String>,
    #[serde(default)]
    pub number: Option<String>,
    #[serde(default)]
    pub short_equals: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParserName {
    Posix,
    Gnu,
    Find,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ArgumentConfig {
    Spec(String),
    Group(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArgumentSettings {
    #[serde(default)]
    pub words: Option<Vec<String>>,
    #[serde(default)]
    pub default: Option<Vec<String>>,
    #[serde(default, rename = "match")]
    pub matches: Option<String>,
    /// The tool adds words at the end of the command that this argument holds.
    #[serde(default)]
    pub appends: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarksConfig {
    #[serde(default)]
    pub read: Vec<String>,
    #[serde(default)]
    pub write: Vec<String>,
    #[serde(default)]
    pub cwd: Vec<String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub shell: Vec<String>,
    #[serde(default)]
    pub variables: Vec<String>,
}

const BUILTIN_TOML: &str = include_str!("builtin_rules.toml");

// --- PolicyEngine: manages compiled rules with live reload ---

/// The policy compiled from one snapshot of its sources.
pub struct CompiledPolicy {
    pub rules: RuleSet,
    /// The grammars of every source, for commands on this machine.
    pub grammars: Grammars,
    /// The grammars without the project's, for commands on another host.
    pub remote_grammars: Grammars,
    /// Unresolved: they resolve against the project root, else the pane's directory.
    pub project_dirs: Vec<String>,
    /// The directory that holds the project's `.claude/tmux-mcp.toml`.
    pub project_root: Option<PathBuf>,
    /// Why a source did not load. While it is set, every command is denied.
    pub broken: Option<String>,
}

/// Thread-safe policy engine that holds compiled policy and watches config files.
pub struct PolicyEngine {
    compiled: RwLock<CompiledPolicy>,
    home_path: PathBuf,
    home_mtime: RwLock<Option<SystemTime>>,
    project_mtime: RwLock<Option<(PathBuf, SystemTime)>>,
}

impl PolicyEngine {
    pub fn new(project_cwd: Option<&str>) -> Self {
        let compiled = compile_config(project_cwd);

        let home_path = home_config_path();
        let home_mtime = file_mtime(&home_path);
        let project_mtime = project_cwd.and_then(project_config_path).and_then(|p| {
            let m = file_mtime(&p)?;
            Some((p, m))
        });

        Self {
            compiled: RwLock::new(compiled),
            home_path,
            home_mtime: RwLock::new(home_mtime),
            project_mtime: RwLock::new(project_mtime),
        }
    }

    /// Get a read reference to the compiled policy.
    pub fn compiled(&self) -> std::sync::RwLockReadGuard<'_, CompiledPolicy> {
        self.compiled.read().unwrap()
    }

    /// Check if config files have changed and reload if needed.
    pub fn check_reload(&self, project_cwd: Option<&str>) {
        let mut changed = false;

        let current_home_mtime = file_mtime(&self.home_path);
        {
            let prev = self.home_mtime.read().unwrap();
            if current_home_mtime != *prev {
                changed = true;
            }
        }

        let project = project_cwd.map(|cwd| {
            project_config_path(cwd).and_then(|p| {
                let m = file_mtime(&p)?;
                Some((p, m))
            })
        });
        if let Some(current) = &project
            && *self.project_mtime.read().unwrap() != *current
        {
            changed = true;
        }

        if !changed {
            return;
        }

        tracing::info!("Policy config changed, reloading...");
        let new = compile_config(project_cwd);
        *self.compiled.write().unwrap() = new;
        *self.home_mtime.write().unwrap() = current_home_mtime;
        if let Some(current) = project {
            *self.project_mtime.write().unwrap() = current;
        }
        tracing::info!("Policy config reloaded");
    }

    /// Start a background file watcher that calls check_reload on changes.
    /// Returns the watcher handle (must be kept alive).
    pub fn start_watcher(
        engine: std::sync::Arc<PolicyEngine>,
        project_cwd: Option<String>,
    ) -> Option<notify::RecommendedWatcher> {
        use notify::{RecursiveMode, Watcher};

        let engine_clone = engine.clone();
        let cwd_clone = project_cwd.clone();

        let mut watcher =
            notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| match res {
                Ok(_event) => engine_clone.check_reload(cwd_clone.as_deref()),
                Err(e) => tracing::warn!("File watch error: {}", e),
            })
            .ok()?;

        let home_dir = engine.home_path.parent()?;
        if home_dir.exists()
            && let Err(e) = watcher.watch(home_dir, RecursiveMode::NonRecursive)
        {
            tracing::warn!("Failed to watch {}: {}", home_dir.display(), e);
        }

        if let Some(project_dir) =
            project_cwd.as_deref().and_then(project_config_path).and_then(|p| p.parent().map(Path::to_path_buf))
            && let Err(e) = watcher.watch(&project_dir, RecursiveMode::NonRecursive)
        {
            tracing::warn!("Failed to watch {}: {}", project_dir.display(), e);
        }

        Some(watcher)
    }
}

// --- Public helpers ---

pub fn parse_config(toml_str: &str) -> Result<PolicyConfig, String> {
    toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {}", e))
}

/// The nearest directory, from `cwd` up to but not including `$HOME`, that
/// holds `.claude/tmux-mcp.toml`. `$HOME`'s own is the user source.
pub fn project_root(cwd: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dir = Path::new(cwd);
    loop {
        if home.as_deref() == Some(dir) {
            return None;
        }
        if dir.join(".claude").join("tmux-mcp.toml").is_file() {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

// --- Compiling ---

/// One loaded source; a project source applies only on this machine.
struct Source {
    config: PolicyConfig,
    is_project: bool,
}

fn compile_config(project_cwd: Option<&str>) -> CompiledPolicy {
    let project_root = project_cwd.and_then(project_root);
    match load_sources(project_root.as_deref()).and_then(|sources| compile_sources(&sources)) {
        Ok((rules, grammars, remote_grammars, project_dirs)) => CompiledPolicy {
            rules,
            grammars,
            remote_grammars,
            project_dirs,
            project_root,
            broken: None,
        },
        Err(err) => {
            tracing::error!("Policy did not load: {}", err);
            CompiledPolicy {
                rules: RuleSet { rules: Vec::new(), default: Decision::Ask },
                grammars: Grammars::default(),
                remote_grammars: Grammars::default(),
                project_dirs: vec![".".into()],
                project_root,
                broken: Some(err),
            }
        }
    }
}

fn load_sources(project_root: Option<&Path>) -> Result<Vec<Source>, String> {
    let mut sources = vec![Source {
        config: parse_config(BUILTIN_TOML).map_err(|e| format!("built-in policy: {e}"))?,
        is_project: false,
    }];
    let project = project_root.map(|root| root.join(".claude").join("tmux-mcp.toml"));
    for (path, is_project) in [(Some(home_config_path()), false), (project, true)] {
        let Some(path) = path else { continue };
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let config = parse_config(&content).map_err(|e| format!("{}: {e}", path.display()))?;
        sources.push(Source { config, is_project });
    }
    Ok(sources)
}

type Compiled = (RuleSet, Grammars, Grammars, Vec<String>);

fn compile_sources(sources: &[Source]) -> Result<Compiled, String> {
    let grammars = Grammars::compile(sources.iter().map(|s| &s.config.commands))?;
    let remote_grammars =
        Grammars::compile(sources.iter().filter(|s| !s.is_project).map(|s| &s.config.commands))?;

    let default = sources
        .iter()
        .rev()
        .find_map(|s| s.config.default.as_deref())
        .map(parse_decision)
        .unwrap_or(Decision::Ask);
    let project_dirs = sources
        .iter()
        .rev()
        .find_map(|s| s.config.project_dirs.clone())
        .unwrap_or_else(|| vec![".".to_string()]);

    // Order decides no result; when two matching rules share an action, the
    // reply names the first, built-in before home before project.
    let rules: Vec<(RuleConfig, bool)> = sources
        .iter()
        .flat_map(|s| s.config.rules.iter().map(move |r| (r.clone(), s.is_project)))
        .collect();
    let rules = rules::compile(&rules, default, &grammars)?;

    Ok((rules, grammars, remote_grammars, project_dirs))
}

fn parse_decision(s: &str) -> Decision {
    match s {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        _ => Decision::Ask,
    }
}

// --- Paths ---

fn home_config_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude").join("tmux-mcp.toml")
}

fn project_config_path(cwd: &str) -> Option<PathBuf> {
    project_root(cwd).map(|root| root.join(".claude").join("tmux-mcp.toml"))
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(test)]
mod tests {
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
}
