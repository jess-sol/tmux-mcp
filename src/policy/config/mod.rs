//! Config loading: TOML parsing, 3-source merge, reloaded on the first
//! decision after a source changes.
//!
//! Three sources, one format (`format`):
//! - Built-in: compiled into binary via include_str!
//! - Home: ~/.claude/tmux-mcp.toml
//! - Project: `<project root>/.claude/tmux-mcp.toml`, found from the pane's directory

mod format;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::RwLock;
use std::time::SystemTime;

use super::Decision;
use super::grammar::Grammars;
use super::rules::{self, RuleSet};
pub use format::{
    ArgumentConfig, CommandConfig, ContextConfig, GrammarConfig, ParserConfig, ParserName,
    PolicyConfig, RuleConfig, parse_config,
};

// --- Types ---

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

/// Thread-safe policy engine that holds the compiled policy and reloads it
/// when a source file changes.
pub struct PolicyEngine {
    compiled: RwLock<CompiledPolicy>,
    home_path: PathBuf,
    home_stamp: RwLock<Option<Stamp>>,
    project_stamp: RwLock<Option<Stamp>>,
}

/// One loaded source; a project source applies only on this machine.
struct Source {
    config: PolicyConfig,
    is_project: bool,
}

type Compiled = (RuleSet, Grammars, Grammars, Vec<String>);

/// A policy file as last loaded: the file its path resolves to, and that
/// file's mtime. A relinked symlink resolves elsewhere even when the new file
/// has the same mtime, as every Nix store path does.
type Stamp = (PathBuf, SystemTime);

const BUILTIN_TOML: &str = include_str!("../builtin_rules.toml");

// --- PolicyEngine: manages compiled rules with live reload ---

impl PolicyEngine {
    pub fn new(project_cwd: Option<&str>) -> Self {
        let compiled = compile_config(project_cwd);

        let home_path = home_config_path();
        let home_stamp = file_stamp(&home_path);
        let project_stamp = project_cwd
            .and_then(project_config_path)
            .and_then(|p| file_stamp(&p));

        Self {
            compiled: RwLock::new(compiled),
            home_path,
            home_stamp: RwLock::new(home_stamp),
            project_stamp: RwLock::new(project_stamp),
        }
    }

    /// Get a read reference to the compiled policy.
    pub fn compiled(&self) -> std::sync::RwLockReadGuard<'_, CompiledPolicy> {
        self.compiled.read().unwrap()
    }

    /// Check if config files have changed and reload if needed.
    pub fn check_reload(&self, project_cwd: Option<&str>) {
        let mut changed = false;

        let current_home_stamp = file_stamp(&self.home_path);
        if current_home_stamp != *self.home_stamp.read().unwrap() {
            changed = true;
        }

        let project = project_cwd.map(|cwd| project_config_path(cwd).and_then(|p| file_stamp(&p)));
        if let Some(current) = &project
            && *self.project_stamp.read().unwrap() != *current
        {
            changed = true;
        }

        if !changed {
            return;
        }

        tracing::info!("Policy config changed, reloading...");
        let new = compile_config(project_cwd);
        *self.compiled.write().unwrap() = new;
        *self.home_stamp.write().unwrap() = current_home_stamp;
        if let Some(current) = project {
            *self.project_stamp.write().unwrap() = current;
        }
        tracing::info!("Policy config reloaded");
    }
}

// --- Public helpers ---

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

/// The built-in grammars, for tests that walk real lines.
#[cfg(test)]
pub(crate) fn builtin_grammars() -> Grammars {
    let config = parse_config(BUILTIN_TOML).expect("the built-in policy parses");
    Grammars::compile([&config.commands]).expect("the built-in grammars compile")
}

// --- Compiling ---

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
                rules: RuleSet {
                    rules: Vec::new(),
                    default: Decision::Ask,
                },
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
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        let config = parse_config(&content).map_err(|e| format!("{}: {e}", path.display()))?;
        sources.push(Source { config, is_project });
    }
    Ok(sources)
}

fn compile_sources(sources: &[Source]) -> Result<Compiled, String> {
    let grammars = Grammars::compile(sources.iter().map(|s| &s.config.commands))?;
    let remote_grammars = Grammars::compile(
        sources
            .iter()
            .filter(|s| !s.is_project)
            .map(|s| &s.config.commands),
    )?;

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
        .flat_map(|s| {
            s.config
                .rules
                .iter()
                .map(move |r| (r.clone(), s.is_project))
        })
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

fn file_stamp(path: &Path) -> Option<Stamp> {
    let resolved = std::fs::canonicalize(path).ok()?;
    let mtime = std::fs::metadata(&resolved).ok()?.modified().ok()?;
    Some((resolved, mtime))
}
