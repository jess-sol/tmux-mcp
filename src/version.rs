//! The build a tmux-mcp process runs as. When builds meet at one daemon
//! socket, the newest daemon serves: a newer client retires an older daemon,
//! and an older client refuses a newer one.

use std::fmt;

/// A semver `major.minor.patch`; anything it can't read is the oldest.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(u64, u64, u64);

impl Version {
    /// This process's version: `$TMUX_MCP_VERSION`, which tests set to stand
    /// in for another build, else the crate's.
    pub fn current() -> Version {
        match std::env::var("TMUX_MCP_VERSION") {
            Ok(claimed) => Version::parse(&claimed),
            Err(_) => Version::parse(env!("CARGO_PKG_VERSION")),
        }
    }

    pub fn parse(text: &str) -> Version {
        let mut parts = text.split('.').map(|part| part.parse::<u64>().ok());
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) => {
                Version(major, minor, patch)
            }
            _ => Version::default(),
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_order_by_number_and_unreadable_ones_are_oldest() {
        let ordered = ["", "0.1.x", "0.1.0", "0.1.1", "0.9.0", "0.10.0", "1.0.0"];
        let parsed: Vec<Version> = ordered.iter().map(|v| Version::parse(v)).collect();
        assert!(parsed.windows(2).all(|w| w[0] <= w[1]), "{parsed:?}");
        assert_eq!(Version::parse("0.10.0").to_string(), "0.10.0");
    }
}
