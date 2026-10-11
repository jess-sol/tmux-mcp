//! Paths as text: resolving a word to an absolute path, and a command's
//! identity. No file is read, so both work on every host.

use std::path::{Component, Path, PathBuf};

use super::{Env, Host};
use crate::policy::value::TriVal;

/// `word` as an absolute path, from `base` when relative. A word that starts
/// with `~` kept it through tilde expansion (`~user/…`, or quoted), so its
/// directory cannot be known.
pub(crate) fn resolve(word: &str, base: Option<&Path>) -> TriVal {
    let resolved = if word.starts_with('~') {
        None
    } else if word.starts_with('/') {
        Some(PathBuf::from(word))
    } else {
        base.map(|b| b.join(word))
    };
    match resolved {
        Some(p) => TriVal::String(normalize(&p).to_string_lossy().into_owned()),
        None => TriVal::Unknown,
    }
}

/// A command's identity, from its first word as text: a bare name is itself;
/// a path inside the anchor is `./` and its path from the anchor; another
/// absolute path is its last component, as is a `~user/…` one; a relative
/// path outside the anchor keeps its `../`. Off this machine, or from a
/// shell directory that cannot be known, a relative path cannot be known. A
/// command without a name has none.
pub(super) fn identity(name: &TriVal, host: &Host, env: &Env, shell_dir: Option<&Path>) -> TriVal {
    let TriVal::String(name) = name else { return name.clone() };
    if !name.contains('/') {
        return TriVal::String(name.clone());
    }
    let last = || TriVal::String(name.rsplit('/').next().unwrap_or_default().to_string());
    let written_absolute = name.starts_with('/') || name.starts_with('~');
    if !matches!(host, Host::Here) || name.starts_with('~') {
        return if written_absolute { last() } else { TriVal::Unknown };
    }
    let TriVal::String(path) = resolve(name, shell_dir) else {
        return TriVal::Unknown;
    };
    let path = PathBuf::from(path);
    match &env.anchor {
        Some(anchor) if path.starts_with(anchor) => {
            TriVal::String(format!("./{}", path.strip_prefix(anchor).unwrap_or(&path).display()))
        }
        _ if written_absolute => last(),
        Some(anchor) => TriVal::String(relative(anchor, &path)),
        None => TriVal::Unknown,
    }
}

/// `.` and `..` resolved as text; `..` above the root stays at the root.
pub(super) fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => _ = out.pop(),
            other => out.push(other),
        }
    }
    out
}

/// The path from `from` to `to`, both absolute and normalized.
fn relative(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let ups = std::iter::repeat_n("..".to_string(), from.len() - common);
    let downs = to[common..].iter().map(|c| c.as_os_str().to_string_lossy().into_owned());
    ups.chain(downs).collect::<Vec<_>>().join("/")
}
