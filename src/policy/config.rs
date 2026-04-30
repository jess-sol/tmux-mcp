//! Config loading: TOML parsing, 3-source merge, file watching for live reload.
//!
//! Three sources, one format: built-in (compiled in), home (~/.config/), project (.tmux-mcp/).
//! Rules are sorted by order, then source priority, then file order.

use serde::Deserialize;
use std::path::PathBuf;

use super::Decision;

// --- Types ---

/// Parsed config from a single TOML source.
#[derive(Debug, Deserialize)]
pub struct PolicyConfig {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub rules: Vec<RuleConfig>,
}

/// A single rule from TOML config.
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

/// A rule with its source and position tagged for stable sorting.
#[derive(Debug, Clone)]
pub struct TaggedRule {
    pub config: RuleConfig,
    pub source: RuleSource,
    pub source_index: usize, // position within its source file
}

/// Where a rule came from — determines priority within the same order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RuleSource {
    Builtin = 0,
    Home = 1,
    Project = 2,
}

const BUILTIN_TOML: &str = include_str!("builtin_rules.toml");

// --- Public API ---

/// Load and merge rules from all three sources. Returns (default_decision, sorted_rules).
pub fn load_merged_rules(project_cwd: Option<&str>) -> (Decision, Vec<TaggedRule>) {
    let builtin = parse_config(BUILTIN_TOML).expect("built-in rules TOML is invalid");
    let home = load_home_config();
    let project = project_cwd.and_then(load_project_config);

    merge(builtin, home, project)
}

/// Parse a TOML string into a PolicyConfig.
pub fn parse_config(toml_str: &str) -> Result<PolicyConfig, String> {
    toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {}", e))
}

/// Find the home config file path.
pub fn home_config_path() -> Option<PathBuf> {
    let xdg = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_default();
            PathBuf::from(home).join(".config")
        });
    let path = xdg.join("tmux-mcp").join("policy.toml");
    if path.exists() { Some(path) } else { None }
}

/// Find the project config file path relative to a working directory.
pub fn project_config_path(cwd: &str) -> Option<PathBuf> {
    let path = PathBuf::from(cwd).join(".tmux-mcp").join("policy.toml");
    if path.exists() { Some(path) } else { None }
}

// --- Internal ---

fn load_home_config() -> Option<PolicyConfig> {
    let path = home_config_path()?;
    load_config_file(&path)
}

fn load_project_config(cwd: &str) -> Option<PolicyConfig> {
    let path = project_config_path(cwd)?;
    load_config_file(&path)
}

fn load_config_file(path: &PathBuf) -> Option<PolicyConfig> {
    match std::fs::read_to_string(path) {
        Ok(content) => match parse_config(&content) {
            Ok(config) => Some(config),
            Err(e) => {
                tracing::warn!("Failed to parse {}: {}", path.display(), e);
                None
            }
        },
        Err(e) => {
            tracing::warn!("Failed to read {}: {}", path.display(), e);
            None
        }
    }
}

/// Merge all sources into a single sorted list.
///
/// Sort order: by `order` ascending, then by source (builtin < home < project),
/// then by file position. First-match-wins evaluation.
///
/// Default: project overrides home, home overrides builtin.
fn merge(
    builtin: PolicyConfig,
    home: Option<PolicyConfig>,
    project: Option<PolicyConfig>,
) -> (Decision, Vec<TaggedRule>) {
    // Determine default: most specific source wins
    let default = project
        .as_ref()
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

    // Stable sort: by order, then source priority, then file position
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

    // --- Loading ---

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
        assert!(eval_rule.when.contains("eval"));
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
        assert_eq!(config.rules[0].description, "my tools");
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

    // --- Default override ---

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

    // --- Rule ordering ---

    #[test]
    fn negative_order_sorted_first() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home_toml = r#"
            [[rules]]
            description = "early rule"
            when = 'command.name == "test"'
            action = "allow"
            order = -1
        "#;
        let home = parse_config(home_toml).unwrap();
        let (_, rules) = merge(builtin, Some(home), None);
        assert_eq!(rules[0].config.description, "early rule");
        assert_eq!(rules[0].config.order, -1);
    }

    #[test]
    fn positive_order_sorted_last() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let home_toml = r#"
            [[rules]]
            description = "late rule"
            when = 'command.name == "test"'
            action = "allow"
            order = 1
        "#;
        let home = parse_config(home_toml).unwrap();
        let (_, rules) = merge(builtin, Some(home), None);
        assert_eq!(rules.last().unwrap().config.description, "late rule");
    }

    #[test]
    fn builtin_before_home_at_same_order() {
        let builtin = parse_config(r#"
            [[rules]]
            description = "builtin rule"
            when = 'command.name == "ls"'
            action = "allow"
        "#).unwrap();
        let home = parse_config(r#"
            [[rules]]
            description = "home rule"
            when = 'command.name == "ls"'
            action = "deny"
        "#).unwrap();
        let (_, rules) = merge(builtin, Some(home), None);
        assert_eq!(rules[0].config.description, "builtin rule");
        assert_eq!(rules[0].source, RuleSource::Builtin);
        assert_eq!(rules[1].config.description, "home rule");
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
        assert!(!rules.is_empty());
    }

    #[test]
    fn empty_config_file_uses_builtin() {
        let builtin = parse_config(BUILTIN_TOML).unwrap();
        let empty = parse_config("").unwrap();
        let (_, rules) = merge(builtin, Some(empty), None);
        // All rules are still from builtin
        assert!(rules.iter().all(|r| r.source == RuleSource::Builtin));
    }
}
