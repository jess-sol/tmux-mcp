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
use super::rules::{self, RuleSet};

// --- Types ---

#[derive(Debug, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
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

#[derive(Debug, Clone)]
pub struct TaggedRule {
    pub config: RuleConfig,
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

/// Thread-safe policy engine that holds compiled rules and watches config files.
/// The daemon holds one of these in its shared state.
pub struct PolicyEngine {
    compiled: RwLock<RuleSet>,
    home_path: PathBuf,
    /// Last known mtime of home config (None if file didn't exist)
    home_mtime: RwLock<Option<SystemTime>>,
    /// Last known mtime of project config, keyed by path
    project_mtime: RwLock<Option<(PathBuf, SystemTime)>>,
}

impl PolicyEngine {
    /// Create a new engine, loading rules from all sources.
    pub fn new(project_cwd: Option<&str>) -> Self {
        let (default, tagged) = load_merged_rules(project_cwd);
        let compiled = match rules::compile(&tagged, default) {
            Ok(rs) => rs,
            Err(e) => {
                tracing::error!("Failed to compile policy rules: {}", e);
                RuleSet { rules: Vec::new(), default: Decision::Ask }
            }
        };

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

    /// Get a read reference to the compiled rules.
    pub fn rules(&self) -> std::sync::RwLockReadGuard<'_, RuleSet> {
        self.compiled.read().unwrap()
    }

    /// Check if config files have changed and reload if needed.
    /// Called periodically or before evaluate.
    pub fn check_reload(&self, project_cwd: Option<&str>) {
        let mut changed = false;

        // Check home config mtime
        let current_home_mtime = file_mtime(&self.home_path);
        {
            let prev = self.home_mtime.read().unwrap();
            if current_home_mtime != *prev {
                changed = true;
            }
        }

        // Check project config mtime
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
        let (default, tagged) = load_merged_rules(project_cwd);
        match rules::compile(&tagged, default) {
            Ok(new_rules) => {
                *self.compiled.write().unwrap() = new_rules;
                *self.home_mtime.write().unwrap() = file_mtime(&self.home_path);
                if let Some(cwd) = project_cwd {
                    let p = project_config_path_always(cwd);
                    *self.project_mtime.write().unwrap() = file_mtime(&p).map(|m| (p, m));
                }
                tracing::info!("Policy rules reloaded successfully");
            }
            Err(e) => {
                tracing::warn!("Failed to compile reloaded rules, keeping previous: {}", e);
            }
        }
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

/// Load and merge rules from all three sources. Returns (default_decision, sorted_rules).
pub fn load_merged_rules(project_cwd: Option<&str>) -> (Decision, Vec<TaggedRule>) {
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

/// Home config path (only if file exists).
pub fn home_config_path() -> Option<PathBuf> {
    let p = home_config_path_always();
    if p.exists() { Some(p) } else { None }
}

/// Project config path (only if file exists).
pub fn project_config_path(cwd: &str) -> Option<PathBuf> {
    let p = project_config_path_always(cwd);
    if p.exists() { Some(p) } else { None }
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
) -> (Decision, Vec<TaggedRule>) {
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
    if let Some(home) = home {
        for (i, rule) in home.rules.into_iter().enumerate() {
            tagged.push(TaggedRule { config: rule, source: RuleSource::Home, source_index: i });
        }
    }
    if let Some(project) = project {
        for (i, rule) in project.rules.into_iter().enumerate() {
            tagged.push(TaggedRule { config: rule, source: RuleSource::Project, source_index: i });
        }
    }

    tagged.sort_by(|a, b| {
        a.config.order.cmp(&b.config.order)
            .then(a.source.cmp(&b.source))
            .then(a.source_index.cmp(&b.source_index))
    });

    (default, tagged)
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
        let safe = config.rules.iter().find(|r| r.description == "read-only tools").unwrap();
        assert_eq!(safe.action, "allow");
        assert!(safe.when.contains("ls"));
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
        let (default, _) = load_merged_rules(None);
        assert_eq!(default, Decision::Ask);
    }

    #[test]
    fn home_overrides_default() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home = parse_config("default = \"deny\"").unwrap();
        let (default, _) = merge(builtin, Some(home), None);
        assert_eq!(default, Decision::Deny);
    }

    #[test]
    fn project_overrides_home_default() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home = parse_config("default = \"deny\"").unwrap();
        let project = parse_config("default = \"allow\"").unwrap();
        let (default, _) = merge(builtin, Some(home), Some(project));
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
        let (_, rules) = merge(builtin, Some(home), None);
        assert_eq!(rules[0].config.description, "early rule");
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
        let (_, rules) = merge(builtin, Some(home), None);
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
        let (_, rules) = merge(builtin, Some(home), None);
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
        let (_, rules) = merge(builtin, Some(home), Some(project));
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
        let (_, rules) = merge(builtin, None, None);
        assert_eq!(rules[0].config.description, "first");
        assert_eq!(rules[1].config.description, "second");
    }

    #[test]
    fn no_config_files_uses_builtin_only() {
        let (default, rules) = load_merged_rules(None);
        assert_eq!(default, Decision::Ask);
        assert!(rules.iter().all(|r| r.source == RuleSource::Builtin));
    }

    #[test]
    fn empty_config_file_uses_builtin() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let empty = parse_config("").unwrap();
        let (_, rules) = merge(builtin, Some(empty), None);
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
