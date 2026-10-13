//! Approval store: bridges hook-based approval to command_run execution.
//!
//! When the hook calls `request_approval` and gets `Ask`, the daemon stores
//! a PendingApproval. When `command_run` arrives, it verifies the approval
//! is still valid (same command, same pane context, not expired), and
//! consumes it once the command is sent.

use std::collections::HashMap;
use std::time::Instant;

use super::PaneContext;

const APPROVAL_TTL_SECS: u64 = 1800;

struct PendingApproval {
    command: String,
    ctx: PaneContext,
    created_at: Instant,
}

#[derive(Default)]
pub struct ApprovalStore {
    /// (origin_pane, pane_id) → pending approval. Typically 0-3 entries.
    pending: HashMap<(String, String), PendingApproval>,
}

impl ApprovalStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store an approval. Replaces any existing for the same (origin, pane).
    pub fn store(&mut self, origin: &str, pane_id: &str, command: &str, ctx: &PaneContext) {
        self.cleanup();
        self.pending.insert(
            (origin.into(), pane_id.into()),
            PendingApproval {
                command: command.into(),
                ctx: ctx.clone(),
                created_at: Instant::now(),
            },
        );
    }

    /// Whether a pending approval covers this request: it exists, has not
    /// expired, and names this command in this pane context.
    pub fn verify(
        &self,
        origin: &str,
        pane_id: &str,
        command: &str,
        live_ctx: &PaneContext,
    ) -> Result<(), String> {
        let key = (origin.to_string(), pane_id.to_string());
        let approval = self.pending.get(&key).ok_or("No pending approval")?;

        if approval.created_at.elapsed().as_secs() >= APPROVAL_TTL_SECS {
            return Err("Approval expired".into());
        }
        if approval.command != command {
            return Err("Command changed since approval".into());
        }
        if approval.ctx != *live_ctx {
            return Err("Pane context changed since approval".into());
        }
        Ok(())
    }

    /// Removes the pending approval, once the command it covers is sent.
    pub fn consume(&mut self, origin: &str, pane_id: &str) {
        self.pending
            .remove(&(origin.to_string(), pane_id.to_string()));
    }

    fn cleanup(&mut self) {
        self.pending
            .retain(|_, a| a.created_at.elapsed().as_secs() < APPROVAL_TTL_SECS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> PaneContext {
        PaneContext {
            hostname: None,
            cwd: Some("/home/user/project".into()),
            user: Some("user".into()),
        }
    }

    #[test]
    fn store_and_verify_roundtrip() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        assert!(store.verify("%0", "%1", "rm foo", &c).is_ok());
    }

    #[test]
    fn consumed_cannot_be_reused() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        assert!(store.verify("%0", "%1", "rm foo", &c).is_ok());
        store.consume("%0", "%1");
        assert_eq!(
            store.verify("%0", "%1", "rm foo", &c).unwrap_err(),
            "No pending approval"
        );
    }

    #[test]
    fn command_mismatch() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        assert_eq!(
            store.verify("%0", "%1", "rm bar", &c).unwrap_err(),
            "Command changed since approval"
        );
    }

    #[test]
    fn different_origin_cannot_consume() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        assert_eq!(
            store.verify("%99", "%1", "rm foo", &c).unwrap_err(),
            "No pending approval"
        );
    }

    #[test]
    fn different_pane_cannot_consume() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        assert_eq!(
            store.verify("%0", "%2", "rm foo", &c).unwrap_err(),
            "No pending approval"
        );
    }

    #[test]
    fn context_drift_hostname() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        let mut drifted = c.clone();
        drifted.hostname = Some("prod-server".into());
        assert_eq!(
            store.verify("%0", "%1", "rm foo", &drifted).unwrap_err(),
            "Pane context changed since approval"
        );
    }

    #[test]
    fn context_drift_cwd() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        let mut drifted = c.clone();
        drifted.cwd = Some("/etc".into());
        assert_eq!(
            store.verify("%0", "%1", "rm foo", &drifted).unwrap_err(),
            "Pane context changed since approval"
        );
    }

    #[test]
    fn context_drift_user() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        let mut drifted = c.clone();
        drifted.user = Some("root".into());
        assert_eq!(
            store.verify("%0", "%1", "rm foo", &drifted).unwrap_err(),
            "Pane context changed since approval"
        );
    }

    #[test]
    fn store_replaces_existing() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        store.store("%0", "%1", "rm bar", &c);
        // Old command should be gone
        assert_eq!(
            store.verify("%0", "%1", "rm foo", &c).unwrap_err(),
            "Command changed since approval"
        );
        // New command should work
        assert!(store.verify("%0", "%1", "rm bar", &c).is_ok());
    }

    #[test]
    fn no_pending_approval() {
        let store = ApprovalStore::new();
        let c = ctx();
        assert_eq!(
            store.verify("%0", "%1", "rm foo", &c).unwrap_err(),
            "No pending approval"
        );
    }
}
