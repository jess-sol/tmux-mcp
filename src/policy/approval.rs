//! Approval store: bridges hook-based approval to command_run execution.
//!
//! When the hook calls `request_approval` and gets `Ask`, the daemon stores
//! a PendingApproval. When `command_run` arrives, it verifies the approval
//! is still valid (same command, same pane context), and consumes it once
//! the command is sent. An approval never expires: it runs one exact
//! command once, in the pane context it was given for, so a prompt
//! answered the next morning still runs.

use std::collections::HashMap;

use super::PaneContext;

/// Why no stored approval covers a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalError {
    /// The hook asked about nothing in this pane, or its approval was
    /// consumed.
    Missing,
    /// The hook asked about another command in this pane: the newest.
    OtherCommand { approved: String },
    /// The pane is not where the person approved the command.
    ContextChanged {
        approved: Box<PaneContext>,
        live: Box<PaneContext>,
    },
}

struct PendingApproval {
    command: String,
    ctx: PaneContext,
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
        self.pending.insert(
            (origin.into(), pane_id.into()),
            PendingApproval {
                command: command.into(),
                ctx: ctx.clone(),
            },
        );
    }

    /// Whether a pending approval covers this request: it exists, names this
    /// command, and was given in this pane context.
    pub fn verify(
        &self,
        origin: &str,
        pane_id: &str,
        command: &str,
        live_ctx: &PaneContext,
    ) -> Result<(), ApprovalError> {
        let key = (origin.to_string(), pane_id.to_string());
        let approval = self.pending.get(&key).ok_or(ApprovalError::Missing)?;

        if approval.command != command {
            return Err(ApprovalError::OtherCommand {
                approved: approval.command.clone(),
            });
        }
        if approval.ctx != *live_ctx {
            return Err(ApprovalError::ContextChanged {
                approved: Box::new(approval.ctx.clone()),
                live: Box::new(live_ctx.clone()),
            });
        }
        Ok(())
    }

    /// Removes the pending approval, once the command it covers is sent.
    pub fn consume(&mut self, origin: &str, pane_id: &str) {
        self.pending
            .remove(&(origin.to_string(), pane_id.to_string()));
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
            store.verify("%0", "%1", "rm foo", &c),
            Err(ApprovalError::Missing)
        );
    }

    /// An approval is for one command in one pane from one origin. Another
    /// command in that pane names the one the person approved.
    #[test]
    fn an_approval_covers_nothing_but_its_own_request() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        let other = Err(ApprovalError::OtherCommand {
            approved: "rm foo".into(),
        });
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

    /// A newer ask for a pane replaces the older one.
    #[test]
    fn a_newer_ask_replaces_the_older() {
        let mut store = ApprovalStore::new();
        let c = ctx();
        store.store("%0", "%1", "rm foo", &c);
        store.store("%0", "%1", "rm bar", &c);
        assert_eq!(
            store.verify("%0", "%1", "rm foo", &c),
            Err(ApprovalError::OtherCommand {
                approved: "rm bar".into()
            })
        );
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
