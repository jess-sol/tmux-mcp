//! Approval store: bridges hook-based approval to command_run execution.
//!
//! When the hook calls `request_approval` and gets `Ask`, the daemon stores
//! a PendingApproval. When `command_run` arrives, it verifies the approval
//! was given in the same pane context, and consumes it once the command is
//! sent. An approval never expires: it runs one exact command once, in the
//! pane context it was given for, so a prompt answered the next morning
//! still runs.

use std::collections::HashMap;

use super::PaneContext;

/// Bounds the store, since a prompt the person declines leaves its approval
/// behind.
const MAX_PENDING: usize = 64;

/// Why no stored approval covers a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalError {
    /// The hook asked about nothing in this pane, or its approval was
    /// consumed.
    Missing,
    /// The hook asked about another command in this pane: the newest.
    OtherCommand { approved: String },
    /// The pane is not where the person approved the command.
    ContextChanged { approved: Box<PaneContext>, live: Box<PaneContext> },
}

struct PendingApproval {
    ctx: PaneContext,
    /// Order of storing: higher is newer.
    seq: u64,
}

#[derive(Default)]
pub struct ApprovalStore {
    /// (origin_pane, pane_id, command) → pending approval. Typically 0-3
    /// entries.
    pending: HashMap<(String, String, String), PendingApproval>,
    next_seq: u64,
}

impl ApprovalStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Store an approval. Replaces any existing for the same command in the
    /// same (origin, pane).
    pub fn store(&mut self, origin: &str, pane_id: &str, command: &str, ctx: &PaneContext) {
        let key = key(origin, pane_id, command);
        if !self.pending.contains_key(&key) && self.pending.len() >= MAX_PENDING {
            self.evict_oldest();
        }
        self.next_seq += 1;
        self.pending.insert(key, PendingApproval { ctx: ctx.clone(), seq: self.next_seq });
    }

    /// Whether a pending approval covers this request: it exists, and was
    /// given in this pane context.
    pub fn verify(
        &self, origin: &str, pane_id: &str, command: &str, live_ctx: &PaneContext,
    ) -> Result<(), ApprovalError> {
        let Some(approval) = self.pending.get(&key(origin, pane_id, command)) else {
            return Err(self
                .newest_other(origin, pane_id)
                .map_or(ApprovalError::Missing, |approved| ApprovalError::OtherCommand {
                    approved,
                }));
        };

        if approval.ctx != *live_ctx {
            return Err(ApprovalError::ContextChanged {
                approved: Box::new(approval.ctx.clone()),
                live: Box::new(live_ctx.clone()),
            });
        }
        Ok(())
    }

    /// Removes the pending approval, once the command it covers is sent.
    pub fn consume(&mut self, origin: &str, pane_id: &str, command: &str) {
        self.pending.remove(&key(origin, pane_id, command));
    }

    /// The command of the newest approval for (origin, pane).
    fn newest_other(&self, origin: &str, pane_id: &str) -> Option<String> {
        self.pending
            .iter()
            .filter(|((o, p, _), _)| o == origin && p == pane_id)
            .max_by_key(|(_, approval)| approval.seq)
            .map(|((_, _, command), _)| command.clone())
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .pending
            .iter()
            .min_by_key(|(_, approval)| approval.seq)
            .map(|(key, _)| key.clone());
        if let Some(oldest) = oldest {
            self.pending.remove(&oldest);
        }
    }
}

fn key(origin: &str, pane_id: &str, command: &str) -> (String, String, String) {
    (origin.to_string(), pane_id.to_string(), command.to_string())
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
        store.consume("%0", "%1", "rm foo");
        assert_eq!(store.verify("%0", "%1", "rm foo", &c), Err(ApprovalError::Missing));
    }

    /// An approval is for one command in one pane from one origin. Another
    /// command in that pane names the one the person approved.
    #[test]
    fn an_approval_covers_nothing_but_its_own_request() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        let other = Err(ApprovalError::OtherCommand { approved: "rm foo".into() });
        for (origin, pane, command, want) in [
            ("%99", "%1", "rm foo", Err(ApprovalError::Missing)),
            ("%0", "%2", "rm foo", Err(ApprovalError::Missing)),
            ("%0", "%1", "rm bar", other),
        ] {
            assert_eq!(
                store.verify(origin, pane, command, &c),
                want,
                "{origin} {pane} {command:?}"
            );
        }
    }

    /// Two commands asked for one pane each keep their approval.
    #[test]
    fn approvals_for_one_pane_stand_side_by_side() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        store.store("%0", "%1", "rm bar", &c);
        assert!(store.verify("%0", "%1", "rm foo", &c).is_ok());
        assert!(store.verify("%0", "%1", "rm bar", &c).is_ok());
    }

    /// A pane whose host, cwd or user moved since the approval refuses it,
    /// naming both contexts.
    #[test]
    fn a_context_that_drifted_refuses_with_both_contexts() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        let drifts: [fn(&mut PaneContext); 3] = [
            |d| d.hostname = Some("prod-server".into()),
            |d| d.cwd = Some("/etc".into()),
            |d| d.user = Some("root".into()),
        ];
        for drift in drifts {
            let mut live = c.clone();
            drift(&mut live);
            assert_eq!(
                store.verify("%0", "%1", "rm foo", &live),
                Err(ApprovalError::ContextChanged {
                    approved: Box::new(c.clone()),
                    live: Box::new(live.clone())
                })
            );
        }
    }
}
