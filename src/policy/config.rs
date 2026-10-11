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
    /// Directories considered "in-project" for path containment checks.
    /// Relative paths resolved against pane.cwd. Default: `["."]`.
    #[serde(default)]
    pub project_dirs: Option<Vec<String>>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RuleConfig {
    pub description: String,
    pub when: String,
    pub action: String,
    /// An *Override rule*: when any matches, the strictest matching override
    /// decides and the other rules do not count.
    #[serde(default, rename = "override")]
    pub is_override: bool,
    #[serde(default)]
    pub message: Option<String>,
}

const BUILTIN_TOML: &str = include_str!("builtin_rules.toml");

// --- PolicyEngine: manages compiled rules with live reload ---

/// Rules compiled from one config snapshot.
pub struct CompiledPolicy {
    pub rules: RuleSet,
    /// Unresolved project dir patterns from config (resolved against pane.cwd at eval time).
    pub project_dirs: Vec<String>,
}

/// Thread-safe policy engine that holds compiled policy and watches config files.
pub struct PolicyEngine {
    compiled: RwLock<CompiledPolicy>,
    home_path: PathBuf,
    home_mtime: RwLock<Option<SystemTime>>,
    project_mtime: RwLock<Option<(PathBuf, SystemTime)>>,
}

fn compile_config(project_cwd: Option<&str>) -> CompiledPolicy {
    let (default, project_dirs, rule_configs) = load_merged_config(project_cwd);
    let rules = match rules::compile(&rule_configs, default) {
        Ok(rs) => rs,
        Err(e) => {
            tracing::error!("Failed to compile policy rules: {}", e);
            RuleSet { rules: Vec::new(), default: Decision::Ask }
        }
    };
    CompiledPolicy { rules, project_dirs }
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
) -> (Decision, Vec<String>, Vec<RuleConfig>) {
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
) -> (Decision, Vec<String>, Vec<RuleConfig>) {
    let default = project.as_ref()
        .and_then(|c| c.default.as_ref())
        .or_else(|| home.as_ref().and_then(|c| c.default.as_ref()))
        .or(builtin.default.as_ref())
        .map(|s| parse_decision(s))
        .unwrap_or(Decision::Ask);

    let project_dirs = project.as_ref()
        .and_then(|c| c.project_dirs.as_ref())
        .or_else(|| home.as_ref().and_then(|c| c.project_dirs.as_ref()))
        .or(builtin.project_dirs.as_ref())
        .cloned()
        .unwrap_or_else(|| vec![".".to_string()]);

    // Order decides no result; when two matching rules share an action, the
    // reply names the first, built-in before home before project.
    let mut rules = builtin.rules;
    for config in [&home, &project].into_iter().flatten() {
        rules.extend(config.rules.iter().cloned());
    }

    (default, project_dirs, rules)
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
        assert_eq!(config.default.as_deref(), Some("ask"));
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
        let (_, _, rules) = merge(builtin, Some(home), None);
        assert_eq!(rules[0].description, "builtin");
        assert_eq!(rules[1].description, "home");
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
        let (_, _, rules) = merge(builtin, Some(home), Some(project));
        assert_eq!(rules[0].description, "home");
        assert_eq!(rules[1].description, "project");
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
        let (_, _, rules) = merge(builtin, None, None);
        assert_eq!(rules[0].description, "first");
        assert_eq!(rules[1].description, "second");
    }

    #[test]
    fn no_config_files_uses_builtin_only() {
        let (default, _, rules) = load_merged_config(None);
        assert_eq!(default, Decision::Ask);
        assert_eq!(rules.len(), parse_config(BUILTIN_TOML).unwrap().rules.len());
    }

    #[test]
    fn empty_config_file_uses_builtin() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let builtin_count = builtin.rules.len();
        let empty = parse_config("").unwrap();
        let (_, _, rules) = merge(builtin, Some(empty), None);
        assert_eq!(rules.len(), builtin_count);
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

    // --- Dependency contract: toml ---

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
}
