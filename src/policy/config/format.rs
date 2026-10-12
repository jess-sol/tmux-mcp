//! The policy file: TOML 1.1, read by serde into these types. Every source
//! (built-in, user, project) has this one format.

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::policy::Via;

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
    /// Lowercase words joined by `-`, unique across all sources.
    pub id: String,
    #[serde(default)]
    pub command: Option<CommandConfig>,
    #[serde(default)]
    pub when: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub write: Option<String>,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    pub action: String,
    #[serde(default)]
    pub message: Option<String>,
    /// The ids of the stricter rules it exempts: its *Exemption*s.
    #[serde(default)]
    pub exempts: Vec<String>,
    /// The *Channel*s it counts on: one, or a list; without it, tmux.
    #[serde(default, deserialize_with = "channels")]
    pub via: Option<Vec<Via>>,
}

/// A rule's `command`: one command path, or a list of them.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CommandConfig {
    One(String),
    Many(Vec<String>),
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
    /// The option's value may also be written `-VALUE` (`head -5`, `kill -KILL`).
    #[serde(default)]
    pub dash: bool,
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

pub fn parse_config(toml_str: &str) -> Result<PolicyConfig, String> {
    toml::from_str(toml_str).map_err(|e| format!("TOML parse error: {}", e))
}

/// `via`: one channel, or a list of them. Each is read as a `Via`, so a
/// misspelling names the channels there are.
fn channels<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Vec<Via>>, D::Error> {
    use serde::de::{self, IntoDeserializer};

    struct Channels;
    impl<'de> de::Visitor<'de> for Channels {
        type Value = Vec<Via>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("`bash`, `tmux`, or a list of them")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Vec<Via>, E> {
            Ok(vec![Via::deserialize(v.into_deserializer())?])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> Result<Vec<Via>, A::Error> {
            Vec::deserialize(de::value::SeqAccessDeserializer::new(seq))
        }
    }
    d.deserialize_any(Channels).map(Some)
}
