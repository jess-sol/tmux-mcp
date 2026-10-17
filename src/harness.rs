//! Harness detection.
//!
//! `tmux-mcp` is invoked by more than one agent host. The Claude Code
//! `PreToolUse` hook expects a Claude-specific JSON envelope, while the Pi
//! extension wants a neutral decision object. Rather than requiring callers to
//! pass a flag, the binary infers the host from the environment.
//!
//! Detection order (first match wins):
//! 1. Explicit override (a CLI flag).
//! 2. `TMUX_MCP_HARNESS=pi|claude` — deterministic override.
//! 3. Pi signals (`PI_SESSION_ID`, `PI_MODEL`, `PI_CODING_AGENT_DIR`, `PI_AGENT_DIR`).
//! 4. Claude signals (`CLAUDECODE`, `CLAUDE_CODE_ENTRYPOINT`).
//! 5. Default: [`Harness::Claude`], preserving the original hook contract.
//!
//! Pi is checked before Claude on purpose: when Pi runs inside a Claude Code
//! session, both sets of variables are present, and the immediate parent is Pi.

/// The agent host that invoked the binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Harness {
    /// Claude Code (or anything using the original `PreToolUse` hook contract).
    Claude,
    /// Pi, or any caller that prefers the neutral JSON decision object.
    Pi,
}

impl Harness {
    fn from_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pi" => Some(Harness::Pi),
            "claude" | "claude-code" | "claudecode" => Some(Harness::Claude),
            _ => None,
        }
    }
}

/// Detect the harness with no explicit override.
pub fn detect() -> Harness {
    detect_with(None)
}

/// Detect the harness, honoring an explicit override when provided.
pub fn detect_with(override_harness: Option<Harness>) -> Harness {
    if let Some(harness) = override_harness {
        return harness;
    }

    if let Some(harness) = std::env::var("TMUX_MCP_HARNESS")
        .ok()
        .and_then(|v| Harness::from_env_value(&v))
    {
        return harness;
    }

    // Pi signals take precedence over Claude signals (see module docs).
    if has_any(&[
        "PI_SESSION_ID",
        "PI_MODEL",
        "PI_CODING_AGENT_DIR",
        "PI_AGENT_DIR",
    ]) {
        return Harness::Pi;
    }

    if has_any(&["CLAUDECODE", "CLAUDE_CODE_ENTRYPOINT"]) {
        return Harness::Claude;
    }

    // Unknown caller: keep the historical Claude hook contract.
    Harness::Claude
}

fn has_any(vars: &[&str]) -> bool {
    vars.iter().any(|name| std::env::var_os(name).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_override_wins() {
        assert_eq!(detect_with(Some(Harness::Pi)), Harness::Pi);
        assert_eq!(detect_with(Some(Harness::Claude)), Harness::Claude);
    }

    #[test]
    fn parses_env_values() {
        assert_eq!(Harness::from_env_value("pi"), Some(Harness::Pi));
        assert_eq!(Harness::from_env_value("PI"), Some(Harness::Pi));
        assert_eq!(Harness::from_env_value(" claude "), Some(Harness::Claude));
        assert_eq!(
            Harness::from_env_value("claude-code"),
            Some(Harness::Claude)
        );
        assert_eq!(Harness::from_env_value("nope"), None);
    }
}
