/// Pane registry: tracks all monitored panes, their VTE processors, and
/// minimal metadata. I/O-free — takes parsed data, returns actions for
/// the caller to execute against tmux.
///
/// Uses per-pane locking via `TrackedPane`: immutable identity fields
/// (pane_id, window_id) are accessible without locking, while mutable
/// state (processor, pid, position) is behind a per-pane Mutex.
/// The registry itself is wrapped in an RwLock by the daemon.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, MutexGuard};

use crate::pane::processor::PaneProcessor;
use crate::parse::layout::LayoutPane;

/// Mutable pane state — behind a per-pane Mutex.
pub struct PaneState {
    pub pid: Option<u32>,
    pub x: usize,
    pub y: usize,
    pub processor: PaneProcessor,
}

/// A tracked pane: immutable identity + locked mutable state.
pub struct TrackedPane {
    pub pane_id: String,
    pub window_id: String,
    state: Mutex<PaneState>,
}

impl TrackedPane {
    /// Lock the mutable pane state.
    pub async fn lock(&self) -> MutexGuard<'_, PaneState> {
        self.state.lock().await
    }
}

/// Handle to a tracked pane — can be held across await points without
/// blocking access to other panes.
pub type PaneHandle = Arc<TrackedPane>;

/// Action the caller must execute after a registry sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncAction {
    /// New pane discovered. Caller must enable `%output` and query `pane_pid`.
    PaneAdded { pane_id: String },
    /// Pane gone. Caller must disable `%output`.
    PaneRemoved { pane_id: String },
    /// Dimensions changed. VTE already resized. Informational.
    Resized { pane_id: String, width: usize, height: usize },
}

/// Manages all tracked panes across the session.
pub struct PaneRegistry {
    panes: HashMap<String, PaneHandle>,
}

impl PaneRegistry {
    pub fn new() -> Self {
        Self {
            panes: HashMap::new(),
        }
    }

    // --- Public lookup API (called under read lock) ---

    /// Get a handle to a tracked pane. The Arc clone is cheap; caller
    /// accesses identity fields directly and locks state via `handle.lock()`.
    pub fn get_handle(&self, pane_id: &str) -> Option<PaneHandle> {
        self.panes.get(pane_id).cloned()
    }

    /// Snapshot all pane handles for iteration (e.g., list_panes).
    /// Caller can drop the registry lock and lock panes individually.
    pub fn snapshot_handles(&self) -> Vec<PaneHandle> {
        self.panes.values().cloned().collect()
    }

    /// Get the window_id for a pane without locking the pane.
    pub fn window_for_pane(&self, pane_id: &str) -> Option<&str> {
        self.panes.get(pane_id).map(|h| h.window_id.as_str())
    }

    pub fn len(&self) -> usize {
        self.panes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
    }

    // --- Structural mutations (called under write lock) ---

    /// Apply a layout change for one window. Diffs tracked panes in that
    /// window against the layout. Returns actions for the caller to execute.
    pub async fn apply_layout(
        &mut self,
        window_id: &str,
        layout_panes: &[LayoutPane],
    ) -> Vec<SyncAction> {
        let mut actions = Vec::new();

        let layout_ids: HashMap<String, &LayoutPane> = layout_panes
            .iter()
            .map(|lp| (format!("%{}", lp.pane_number), lp))
            .collect();

        // Remove tracked panes in this window that aren't in the layout
        let dead: Vec<String> = self.panes
            .iter()
            .filter(|(_, h)| h.window_id == window_id && !layout_ids.contains_key(h.pane_id.as_str()))
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            self.panes.remove(id);
            actions.push(SyncAction::PaneRemoved { pane_id: id.clone() });
        }

        // Add new panes, update existing
        for (pane_id, lp) in &layout_ids {
            if let Some(handle) = self.panes.get(pane_id) {
                let mut state = handle.lock().await;
                state.x = lp.x;
                state.y = lp.y;

                let cur_w = state.processor.columns();
                let cur_h = state.processor.screen_lines();
                if lp.width != cur_w || lp.height != cur_h {
                    state.processor.resize(lp.height, lp.width);
                    actions.push(SyncAction::Resized {
                        pane_id: pane_id.clone(),
                        width: lp.width,
                        height: lp.height,
                    });
                }
            } else {
                let processor = PaneProcessor::new(lp.height, lp.width);
                let tracked = TrackedPane {
                    pane_id: pane_id.clone(),
                    window_id: window_id.to_string(),
                    state: Mutex::new(PaneState {
                        pid: None,
                        x: lp.x,
                        y: lp.y,
                        processor,
                    }),
                };
                self.panes.insert(pane_id.clone(), Arc::new(tracked));
                actions.push(SyncAction::PaneAdded { pane_id: pane_id.clone() });
            }
        }

        actions
    }

    /// Set the shell PID for a pane.
    pub async fn set_pid(&mut self, pane_id: &str, pid: u32) {
        if let Some(handle) = self.panes.get(pane_id) {
            handle.lock().await.pid = Some(pid);
        }
    }

    /// Remove all panes in a window. Returns `PaneRemoved` actions.
    pub fn remove_window(&mut self, window_id: &str) -> Vec<SyncAction> {
        let dead: Vec<String> = self.panes
            .iter()
            .filter(|(_, h)| h.window_id == window_id)
            .map(|(id, _)| id.clone())
            .collect();

        let mut actions = Vec::with_capacity(dead.len());
        for id in dead {
            self.panes.remove(&id);
            actions.push(SyncAction::PaneRemoved { pane_id: id });
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lp(pane_number: u32, width: usize, height: usize, x: usize, y: usize) -> LayoutPane {
        LayoutPane { pane_number, width, height, x, y }
    }

    fn has_action(actions: &[SyncAction], expected: &SyncAction) -> bool {
        actions.iter().any(|a| a == expected)
    }

    #[tokio::test]
    async fn adds_new_panes() {
        let mut reg = PaneRegistry::new();
        let actions = reg.apply_layout("@1", &[
            lp(0, 80, 24, 0, 0),
            lp(1, 80, 24, 0, 25),
        ]).await;
        assert_eq!(actions.len(), 2);
        assert!(has_action(&actions, &SyncAction::PaneAdded { pane_id: "%0".into() }));
        assert!(has_action(&actions, &SyncAction::PaneAdded { pane_id: "%1".into() }));
        assert_eq!(reg.len(), 2);
    }

    #[tokio::test]
    async fn removes_dead_panes() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0), lp(2, 80, 24, 0, 25)]).await;
        assert_eq!(reg.len(), 2);

        let actions = reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;
        assert!(has_action(&actions, &SyncAction::PaneRemoved { pane_id: "%2".into() }));
        assert_eq!(reg.len(), 1);
    }

    #[tokio::test]
    async fn detects_resize() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;

        let actions = reg.apply_layout("@1", &[lp(1, 120, 40, 0, 0)]).await;
        assert!(has_action(&actions, &SyncAction::Resized {
            pane_id: "%1".into(), width: 120, height: 40,
        }));

        let handle = reg.get_handle("%1").unwrap();
        let state = handle.lock().await;
        assert_eq!(state.processor.columns(), 120);
        assert_eq!(state.processor.screen_lines(), 40);
    }

    #[tokio::test]
    async fn no_op_unchanged() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;

        let actions = reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;
        assert!(actions.is_empty());
    }

    #[tokio::test]
    async fn updates_position() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;
        assert_eq!(reg.get_handle("%1").unwrap().lock().await.x, 0);

        reg.apply_layout("@1", &[lp(1, 80, 24, 50, 10)]).await;
        let handle = reg.get_handle("%1").unwrap();
        let state = handle.lock().await;
        assert_eq!(state.x, 50);
        assert_eq!(state.y, 10);
    }

    #[tokio::test]
    async fn preserves_other_windows() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;
        reg.apply_layout("@2", &[lp(2, 80, 24, 0, 0)]).await;
        assert_eq!(reg.len(), 2);

        let actions = reg.apply_layout("@1", &[]).await;
        assert!(has_action(&actions, &SyncAction::PaneRemoved { pane_id: "%1".into() }));
        assert_eq!(reg.len(), 1);
        assert!(reg.get_handle("%2").is_some());
    }

    #[tokio::test]
    async fn remove_window_clears_all() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0), lp(2, 80, 24, 0, 25)]).await;
        reg.apply_layout("@2", &[lp(3, 80, 24, 0, 0)]).await;
        assert_eq!(reg.len(), 3);

        let actions = reg.remove_window("@1");
        assert_eq!(actions.len(), 2);
        assert_eq!(reg.len(), 1);
        assert!(reg.get_handle("%3").is_some());
    }

    #[tokio::test]
    async fn set_pid_stores_value() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;

        assert!(reg.get_handle("%1").unwrap().lock().await.pid.is_none());
        reg.set_pid("%1", 12345).await;
        assert_eq!(reg.get_handle("%1").unwrap().lock().await.pid, Some(12345));
    }

    #[tokio::test]
    async fn set_pid_unknown_pane_is_noop() {
        let mut reg = PaneRegistry::new();
        reg.set_pid("%99", 12345).await;
    }

    #[tokio::test]
    async fn processor_state_survives_layout_update() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;

        reg.get_handle("%1").unwrap().lock().await.processor.process_chunk(b"hello world");
        assert_eq!(
            reg.get_handle("%1").unwrap().lock().await.processor.screen_line_text(0),
            "hello world"
        );

        reg.apply_layout("@1", &[lp(1, 120, 24, 0, 0)]).await;
        assert_eq!(
            reg.get_handle("%1").unwrap().lock().await.processor.screen_line_text(0),
            "hello world"
        );
    }

    #[test]
    fn get_handle_unknown() {
        let reg = PaneRegistry::new();
        assert!(reg.get_handle("%99").is_none());
    }

    #[tokio::test]
    async fn multi_window_lifecycle() {
        let mut reg = PaneRegistry::new();

        reg.apply_layout("@1", &[lp(1, 100, 50, 0, 0), lp(2, 99, 50, 101, 0)]).await;
        reg.apply_layout("@2", &[lp(3, 80, 24, 0, 0)]).await;
        assert_eq!(reg.len(), 3);

        let actions = reg.apply_layout("@1", &[
            lp(1, 100, 50, 0, 0),
            lp(2, 99, 25, 101, 0),
            lp(4, 99, 24, 101, 26),
        ]).await;
        assert!(has_action(&actions, &SyncAction::PaneAdded { pane_id: "%4".into() }));
        assert!(has_action(&actions, &SyncAction::Resized { pane_id: "%2".into(), width: 99, height: 25 }));
        assert_eq!(reg.len(), 4);

        let actions = reg.remove_window("@2");
        assert_eq!(actions.len(), 1);
        assert_eq!(reg.len(), 3);
    }
}
