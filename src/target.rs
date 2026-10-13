/// Arrange a usable tmux pane for tmux-mcp commands.
///
/// The *origin* pane is where the agent (and the MCP server) runs. Sending a
/// shell command there types it into the agent's own terminal, so it is never a
/// valid target. When the origin pane is the only pane in its window there is
/// nowhere safe to run commands, and this module creates a scratch pane armed
/// with OSC 133. When other panes exist it returns the most suitable one
/// without modifying it.
use serde_json::{Value, json};

use crate::client::DaemonClient;

/// Result of [`ensure_target`].
#[derive(Debug, Clone)]
pub struct EnsuredTarget {
    /// Pane to target with `command_run`.
    pub pane_id: String,
    /// Whether a scratch pane was created.
    pub created: bool,
    /// Injection status for a created pane (`active`, `already_active`,
    /// `unsupported_shell`, `failed`, `error`, or `not_attempted`).
    pub injection: String,
    /// Detected shell of a created pane, when known.
    pub shell: Option<String>,
    /// Human-readable explanation of the choice.
    pub reason: String,
}

impl EnsuredTarget {
    pub fn to_json(&self) -> Value {
        json!({
            "pane_id": self.pane_id,
            "created": self.created,
            "injection": self.injection,
            "shell": self.shell,
            "reason": self.reason,
        })
    }
}

/// Pick the best non-origin pane: an idle shell first, then one with working
/// shell integration, then any pane.
fn pick_target(panes: &[Value], origin_pane: &str) -> Option<String> {
    let candidates: Vec<&Value> = panes
        .iter()
        .filter(|pane| pane["pane_id"].as_str().is_some_and(|id| id != origin_pane))
        .collect();

    let idle_shell = |pane: &&Value| {
        pane["foreground"].is_null() && !matches!(pane["activity"].as_str(), Some("Busy"))
    };
    let osc133_ready = |pane: &&Value| pane["osc133_status"].as_str() == Some("confirmed");

    candidates
        .iter()
        .find(|pane| idle_shell(pane))
        .or_else(|| candidates.iter().find(|pane| osc133_ready(pane)))
        .or_else(|| candidates.first())
        .and_then(|pane| pane["pane_id"].as_str())
        .map(str::to_string)
}

/// Ensure a target pane for `command_run`.
///
/// Returns an existing pane when one is available. Otherwise, when
/// `allow_create` is set, splits a new pane off the origin pane and arms it
/// with shell integration.
pub async fn ensure_target(
    client: &DaemonClient,
    origin_pane: &str,
    allow_create: bool,
) -> Result<EnsuredTarget, String> {
    let listed = client
        .request("list_panes", json!({ "origin_pane": origin_pane }))
        .await
        .map_err(|e| e.to_string())?;
    let panes = listed["panes"].as_array().cloned().unwrap_or_default();

    if let Some(pane_id) = pick_target(&panes, origin_pane) {
        return Ok(EnsuredTarget {
            pane_id,
            created: false,
            injection: "not_attempted".into(),
            shell: None,
            reason: "existing pane".into(),
        });
    }

    if !allow_create {
        return Err(format!(
            "No pane other than the origin pane {} is available. \
             Create one (e.g. `tmux split-window`) and retry.",
            origin_pane
        ));
    }

    // Split a scratch pane in the same window as the origin pane.
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let output = std::process::Command::new("tmux")
        .args([
            "split-window",
            "-h",
            "-t",
            origin_pane,
            "-P",
            "-F",
            "#{pane_id}",
            "-c",
        ])
        .arg(&cwd)
        .output()
        .map_err(|e| format!("Failed to run tmux split-window: {}", e))?;
    if !output.status.success() {
        return Err(format!(
            "tmux split-window failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let new_pane = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if new_pane.is_empty() {
        return Err("tmux split-window returned no pane id".into());
    }

    // Wait for the daemon to register the new pane (control-mode notification
    // is asynchronous). Proceed regardless after the deadline so a slow daemon
    // still produces an actionable result.
    let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(3);
    loop {
        let listed = client
            .request("list_panes", json!({ "origin_pane": origin_pane }))
            .await
            .map_err(|e| e.to_string())?;
        let known = listed["panes"].as_array().is_some_and(|panes| {
            panes
                .iter()
                .any(|pane| pane["pane_id"].as_str() == Some(new_pane.as_str()))
        });
        if known || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // Arm the new pane so `command_run` works immediately.
    let (injection, shell) = match client
        .request(
            "inject_osc133",
            json!({ "origin_pane": origin_pane, "pane_id": new_pane }),
        )
        .await
    {
        Ok(value) => (
            value["status"].as_str().unwrap_or("unknown").to_string(),
            value["shell"].as_str().map(str::to_string),
        ),
        Err(e) => (format!("error: {}", e), None),
    };

    Ok(EnsuredTarget {
        pane_id: new_pane,
        created: true,
        injection,
        shell,
        reason: "created scratch pane".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: &str, foreground: Value, activity: &str, osc133: &str) -> Value {
        json!({
            "pane_id": id,
            "foreground": foreground,
            "activity": activity,
            "osc133_status": osc133,
        })
    }

    #[test]
    fn returns_none_when_only_origin() {
        let panes = vec![pane("%0", Value::Null, "Idle", "unknown")];
        assert_eq!(pick_target(&panes, "%0"), None);
    }

    #[test]
    fn prefers_idle_shell_over_busy() {
        let panes = vec![
            pane("%0", Value::Null, "Idle", "unknown"),
            pane("%1", json!("vim"), "Busy", "unknown"),
            pane("%2", Value::Null, "Idle", "unknown"),
        ];
        assert_eq!(pick_target(&panes, "%0").as_deref(), Some("%2"));
    }

    #[test]
    fn falls_back_to_any_non_origin() {
        let panes = vec![
            pane("%0", Value::Null, "Idle", "unknown"),
            pane("%1", json!("vim"), "Busy", "unknown"),
        ];
        assert_eq!(pick_target(&panes, "%0").as_deref(), Some("%1"));
    }

    #[test]
    fn prefers_osc133_ready_when_all_busy() {
        let panes = vec![
            pane("%0", Value::Null, "Idle", "unknown"),
            pane("%1", json!("server"), "Busy", "unknown"),
            pane("%2", json!("server"), "Busy", "confirmed"),
        ];
        assert_eq!(pick_target(&panes, "%0").as_deref(), Some("%2"));
    }
}
