//! Config loading: TOML parsing, 3-source merge, file watching for live reload.
//!
//! Three sources, one format:
//! - Built-in: compiled into binary via include_str!
//! - Home: ~/.claude/tmux-mcp.toml
//! - Project: .claude/tmux-mcp.toml (relative to pane CWD)

use std::path::PathBuf;
use std::sync::RwLock;
use std::time::SystemTime;

use serde::Deserialize;

use super::Decision;
use super::parse::{self, WrapperRegistry};
use super::rules::{self, RuleSet};

// --- Types ---

#[derive(Debug, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
    #[serde(default)]
    pub wrappers: Vec<WrapperConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuleConfig {
    pub description: String,
    pub when: String,
    pub action: String,
    #[serde(default)]
    pub order: i32,
    #[serde(default)]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WrapperConfig {
    pub name: String,
    pub when: String,
    /// POSIX getopt: stop at first non-option. `getopt = "u:C:"` or `getopt = { short = "s:k:", long = ["signal:"] }`
    #[serde(default)]
    pub getopt: Option<GetoptConfig>,
    /// GNU getopt: flags anywhere before `--`. Same format as `getopt`.
    #[serde(default)]
    pub getopt_gnu: Option<GetoptConfig>,
    pub inner: String,
    #[serde(default)]
    pub capture_user: Option<String>,
    #[serde(default)]
    pub capture_host: Option<String>,
    #[serde(default = "default_true")]
    pub skip_wrapper: bool,
    #[serde(default = "default_true")]
    pub args_complete: bool,
    #[serde(default)]
    pub order: i32,
}

fn default_true() -> bool { true }

/// Getopt configuration from TOML. Two forms:
/// - String: just short options optstring `"isC:D:u:"`
/// - Table: `{ short = "s:k:", long = ["signal:", "kill-after:"] }`
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum GetoptConfig {
    /// Optstring: `getopt = "isC:D:u:"`
    Short(String),
    /// With long options: `getopt = { short = "s:k:", long = ["signal:"] }`
    Full {
        #[serde(default)]
        short: String,
        #[serde(default)]
        long: Vec<String>,
    },
}

impl GetoptConfig {
    /// Convert to ArgSpec with the given style.
    pub fn to_arg_spec(&self, style: super::args::ArgStyle) -> super::args::ArgSpec {
        match self {
            GetoptConfig::Short(optstring) => {
                super::args::ArgSpec::from_optstring(style, optstring)
            }
            GetoptConfig::Full { short, long } => {
                let long_refs: Vec<&str> = long.iter().map(|s| s.as_str()).collect();
                super::args::ArgSpec::from_optstring_long(style, short, &long_refs)
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaggedRule {
    pub config: RuleConfig,
    pub source: RuleSource,
    pub source_index: usize,
}

#[derive(Debug, Clone)]
pub struct TaggedWrapper {
    pub config: WrapperConfig,
    pub source: RuleSource,
    pub source_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleSource {
    Builtin = 0,
    Home = 1,
    Project = 2,
}

const BUILTIN_TOML: &str = include_str!("builtin_rules.toml");

// --- PolicyEngine: manages compiled rules with live reload ---

/// Rules + wrappers compiled from the same config snapshot.
pub struct CompiledPolicy {
    pub rules: RuleSet,
    pub wrappers: WrapperRegistry,
}

/// Thread-safe policy engine that holds compiled policy and watches config files.
pub struct PolicyEngine {
    compiled: RwLock<CompiledPolicy>,
    home_path: PathBuf,
    home_mtime: RwLock<Option<SystemTime>>,
    project_mtime: RwLock<Option<(PathBuf, SystemTime)>>,
}

fn compile_config(project_cwd: Option<&str>) -> CompiledPolicy {
    let (default, tagged, tagged_wrappers) = load_merged_config(project_cwd);
    let rules = match rules::compile(&tagged, default) {
        Ok(rs) => rs,
        Err(e) => {
            tracing::error!("Failed to compile policy rules: {}", e);
            RuleSet { rules: Vec::new(), default: Decision::Ask }
        }
    };
    let wrappers = parse::compile_wrappers(&tagged_wrappers);
    CompiledPolicy { rules, wrappers }
}

impl PolicyEngine {
    pub fn new(project_cwd: Option<&str>) -> Self {
        let compiled = compile_config(project_cwd);

        let home_path = home_config_path_always();
        let home_mtime = file_mtime(&home_path);
        let project_mtime = project_cwd
            .and_then(|cwd| {
                let p = project_config_path_always(cwd);
                file_mtime(&p).map(|m| (p, m))
            });

        Self {
            compiled: RwLock::new(compiled),
            home_path,
            home_mtime: RwLock::new(home_mtime),
            project_mtime: RwLock::new(project_mtime),
        }
    }

    /// Get a read reference to the compiled policy (rules + wrappers).
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

        if let Some(cwd) = project_cwd {
            let project_path = project_config_path_always(cwd);
            let current_mtime = file_mtime(&project_path);
            let prev = self.project_mtime.read().unwrap();
            match (&*prev, current_mtime) {
                (Some((prev_path, prev_mtime)), Some(cur_mtime))
                    if prev_path == &project_path && *prev_mtime == cur_mtime => {}
                (None, None) => {}
                _ => { changed = true; }
            }
        }

        if !changed {
            return;
        }

        tracing::info!("Policy config changed, reloading...");
        let new = compile_config(project_cwd);
        *self.compiled.write().unwrap() = new;
        *self.home_mtime.write().unwrap() = file_mtime(&self.home_path);
        if let Some(cwd) = project_cwd {
            let p = project_config_path_always(cwd);
            *self.project_mtime.write().unwrap() = file_mtime(&p).map(|m| (p, m));
        }
        tracing::info!("Policy config reloaded successfully");
    }

    /// Start a background file watcher that calls check_reload on changes.
    /// Returns the watcher handle (must be kept alive).
    pub fn start_watcher(
        engine: std::sync::Arc<PolicyEngine>,
        project_cwd: Option<String>,
    ) -> Option<notify::RecommendedWatcher> {
        use notify::{Watcher, RecursiveMode};

        let engine_clone = engine.clone();
        let cwd_clone = project_cwd.clone();

        let mut watcher = notify::recommended_watcher(move |res: Result<notify::Event, notify::Error>| {
            match res {
                Ok(_event) => {
                    engine_clone.check_reload(cwd_clone.as_deref());
                }
                Err(e) => tracing::warn!("File watch error: {}", e),
            }
        }).ok()?;

        // Watch home config directory
        let home_dir = engine.home_path.parent()?;
        if home_dir.exists() {
            if let Err(e) = watcher.watch(home_dir, RecursiveMode::NonRecursive) {
                tracing::warn!("Failed to watch {}: {}", home_dir.display(), e);
            }
        }

        // Watch project config directory
        if let Some(cwd) = &project_cwd {
            let project_path = project_config_path_always(cwd);
            if let Some(project_dir) = project_path.parent() {
                if project_dir.exists() {
                    if let Err(e) = watcher.watch(project_dir, RecursiveMode::NonRecursive) {
                        tracing::warn!("Failed to watch {}: {}", project_dir.display(), e);
                    }
                }
            }
        }

        Some(watcher)
    }
}

// --- Public helpers ---

/// Load and merge config from all three sources.
pub fn load_merged_config(
    project_cwd: Option<&str>,
) -> (Decision, Vec<TaggedRule>, Vec<TaggedWrapper>) {
    let builtin = parse_config(BUILTIN_TOML).expect("built-in rules TOML is invalid");
    let home = load_config_file(&home_config_path_always());
    let project = project_cwd.and_then(|cwd| load_config_file(&project_config_path_always(cwd)));
    merge(builtin, home, project)
}

pub fn parse_config(toml_str: &str) -> Result<PolicyConfig, String> {
    toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {}", e))
}

// --- Paths ---

/// Home config path (always returns the path, whether or not file exists).
fn home_config_path_always() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    PathBuf::from(home).join(".claude").join("tmux-mcp.toml")
}

/// Project config path (always returns the path, whether or not file exists).
fn project_config_path_always(cwd: &str) -> PathBuf {
    PathBuf::from(cwd).join(".claude").join("tmux-mcp.toml")
}

// --- Internal ---

fn file_mtime(path: &PathBuf) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

fn load_config_file(path: &PathBuf) -> Option<PolicyConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match parse_config(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            tracing::warn!("Failed to parse {}: {}", path.display(), e);
            None
        }
    }
}

fn merge(
    builtin: PolicyConfig,
    home: Option<PolicyConfig>,
    project: Option<PolicyConfig>,
) -> (Decision, Vec<TaggedRule>, Vec<TaggedWrapper>) {
    let default = project.as_ref()
        .and_then(|c| c.default.as_ref())
        .or_else(|| home.as_ref().and_then(|c| c.default.as_ref()))
        .or(builtin.default.as_ref())
        .map(|s| parse_decision(s))
        .unwrap_or(Decision::Ask);

    let mut tagged = Vec::new();

    for (i, rule) in builtin.rules.into_iter().enumerate() {
        tagged.push(TaggedRule { config: rule, source: RuleSource::Builtin, source_index: i });
    }
    if let Some(ref home) = home {
        for (i, rule) in home.rules.iter().cloned().enumerate() {
            tagged.push(TaggedRule { config: rule, source: RuleSource::Home, source_index: i });
        }
    }
    if let Some(ref project) = project {
        for (i, rule) in project.rules.iter().cloned().enumerate() {
            tagged.push(TaggedRule { config: rule, source: RuleSource::Project, source_index: i });
        }
    }

    tagged.sort_by(|a, b| {
        a.config.order.cmp(&b.config.order)
            .then(a.source.cmp(&b.source))
            .then(a.source_index.cmp(&b.source_index))
    });

    // Merge wrappers — same ordering as rules (order → source → file index)
    let mut tagged_wrappers = Vec::new();

    for (i, w) in builtin.wrappers.into_iter().enumerate() {
        tagged_wrappers.push(TaggedWrapper { config: w, source: RuleSource::Builtin, source_index: i });
    }
    if let Some(home) = home {
        for (i, w) in home.wrappers.into_iter().enumerate() {
            tagged_wrappers.push(TaggedWrapper { config: w, source: RuleSource::Home, source_index: i });
        }
    }
    if let Some(project) = project {
        for (i, w) in project.wrappers.into_iter().enumerate() {
            tagged_wrappers.push(TaggedWrapper { config: w, source: RuleSource::Project, source_index: i });
        }
    }

    tagged_wrappers.sort_by(|a, b| {
        a.config.order.cmp(&b.config.order)
            .then(a.source.cmp(&b.source))
            .then(a.source_index.cmp(&b.source_index))
    });

    (default, tagged, tagged_wrappers)
}

fn parse_decision(s: &str) -> Decision {
    match s {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        _ => Decision::Ask,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_toml_parses_successfully() {
        let config = parse_config(BUILTIN_TOML).unwrap();
        assert!(!config.rules.is_empty());
        assert_eq!(config.default.as_deref(), Some("ask"));
    }

    #[test]
    fn builtin_has_eval_deny_rule() {
        let config = parse_config(BUILTIN_TOML).unwrap();
        let eval_rule = config.rules.iter().find(|r| r.description == "eval").unwrap();
        assert_eq!(eval_rule.action, "deny");
    }

    #[test]
    fn builtin_has_safe_commands() {
        let config = parse_config(BUILTIN_TOML).unwrap();
        let safe = config.rules.iter().find(|r| r.description == "file readers (in-project)").unwrap();
        assert_eq!(safe.action, "allow");
        assert!(safe.when.contains("cat"));
    }

    #[test]
    fn empty_config_parses() {
        let config = parse_config("").unwrap();
        assert!(config.rules.is_empty());
        assert!(config.default.is_none());
    }

    #[test]
    fn invalid_toml_returns_error() {
        assert!(parse_config("[[invalid").is_err());
    }

    #[test]
    fn user_config_parses() {
        let toml = r#"
            default = "deny"
            [[rules]]
            description = "my tools"
            when = 'command.name == "cargo"'
            action = "allow"
        "#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.default.as_deref(), Some("deny"));
        assert_eq!(config.rules.len(), 1);
    }

    #[test]
    fn user_config_with_order() {
        let toml = r#"
            [[rules]]
            description = "override eval"
            when = 'command.name == "eval"'
            action = "ask"
            order = -1
        "#;
        let config = parse_config(toml).unwrap();
        assert_eq!(config.rules[0].order, -1);
    }

    #[test]
    fn builtin_default_is_ask() {
        let (default, _, _) = load_merged_config(None);
        assert_eq!(default, Decision::Ask);
    }

    #[test]
    fn home_overrides_default() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home = parse_config("default = \"deny\"").unwrap();
        let (default, _, _) = merge(builtin, Some(home), None);
        assert_eq!(default, Decision::Deny);
    }

    #[test]
    fn project_overrides_home_default() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home = parse_config("default = \"deny\"").unwrap();
        let project = parse_config("default = \"allow\"").unwrap();
        let (default, _, _) = merge(builtin, Some(home), Some(project));
        assert_eq!(default, Decision::Allow);
    }

    #[test]
    fn negative_order_sorted_first() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home = parse_config(r#"
            [[rules]]
            description = "early rule"
            when = 'command.name == "test"'
            action = "allow"
            order = -1
        "#).unwrap();
        let (_, rules, _) = merge(builtin, Some(home), None);
        // The early rule should be before any order-0 rules
        let early_pos = rules.iter().position(|r| r.config.description == "early rule").unwrap();
        let first_order0 = rules.iter().position(|r| r.config.order == 0).unwrap();
        assert!(early_pos < first_order0, "order -1 rule should come before order 0 rules");
    }

    #[test]
    fn positive_order_sorted_last() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home = parse_config(r#"
            [[rules]]
            description = "late rule"
            when = 'command.name == "test"'
            action = "allow"
            order = 1
        "#).unwrap();
        let (_, rules, _) = merge(builtin, Some(home), None);
        assert_eq!(rules.last().unwrap().config.description, "late rule");
    }

    #[test]
    fn builtin_before_home_at_same_order() {
        let builtin = parse_config(r#"
            [[rules]]
            description = "builtin"
            when = 'command.name == "ls"'
            action = "allow"
        "#).unwrap();
        let home = parse_config(r#"
            [[rules]]
            description = "home"
            when = 'command.name == "ls"'
            action = "deny"
        "#).unwrap();
        let (_, rules, _) = merge(builtin, Some(home), None);
        assert_eq!(rules[0].source, RuleSource::Builtin);
        assert_eq!(rules[1].source, RuleSource::Home);
    }

    #[test]
    fn home_before_project_at_same_order() {
        let builtin = parse_config("").unwrap();
        let home = parse_config(r#"
            [[rules]]
            description = "home"
            when = 'true'
            action = "allow"
        "#).unwrap();
        let project = parse_config(r#"
            [[rules]]
            description = "project"
            when = 'true'
            action = "deny"
        "#).unwrap();
        let (_, rules, _) = merge(builtin, Some(home), Some(project));
        assert_eq!(rules[0].config.description, "home");
        assert_eq!(rules[1].config.description, "project");
    }

    #[test]
    fn file_order_preserved_within_source() {
        let builtin = parse_config(r#"
            [[rules]]
            description = "first"
            when = 'true'
            action = "allow"
            [[rules]]
            description = "second"
            when = 'true'
            action = "deny"
        "#).unwrap();
        let (_, rules, _) = merge(builtin, None, None);
        assert_eq!(rules[0].config.description, "first");
        assert_eq!(rules[1].config.description, "second");
    }

    #[test]
    fn no_config_files_uses_builtin_only() {
        let (default, rules, _) = load_merged_config(None);
        assert_eq!(default, Decision::Ask);
        assert!(rules.iter().all(|r| r.source == RuleSource::Builtin));
    }

    #[test]
    fn empty_config_file_uses_builtin() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let empty = parse_config("").unwrap();
        let (_, rules, _) = merge(builtin, Some(empty), None);
        assert!(rules.iter().all(|r| r.source == RuleSource::Builtin));
    }

    #[test]
    fn home_config_path_is_claude_dir() {
        let path = home_config_path_always();
        assert!(path.to_str().unwrap().contains(".claude/tmux-mcp.toml"));
    }

    #[test]
    fn project_config_path_is_claude_dir() {
        let path = project_config_path_always("/home/user/project");
        assert_eq!(path, PathBuf::from("/home/user/project/.claude/tmux-mcp.toml"));
    }
}
