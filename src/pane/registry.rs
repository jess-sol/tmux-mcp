/// Pane registry: tracks all monitored panes, their VTE processors, and
/// minimal metadata. I/O-free — takes parsed data, returns actions for
/// the caller to execute against tmux.

use std::collections::HashMap;

use crate::pane::processor::PaneProcessor;
use crate::parse::layout::LayoutPane;

/// A tracked pane with its VTE processor and minimal metadata.
pub struct TrackedPane {
    pub pane_id: String,
    pub window_id: String,
    pub pid: Option<u32>,
    pub x: usize,
    pub y: usize,
    pub processor: PaneProcessor,
}

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
    panes: HashMap<String, TrackedPane>,
}

impl PaneRegistry {
    pub fn new() -> Self {
        Self {
            panes: HashMap::new(),
        }
    }

    /// Apply a layout change for one window. Diffs tracked panes in that
    /// window against the layout. Returns actions for the caller to execute.
    ///
    /// New panes get a PaneProcessor at the right dimensions.
    /// Resized panes get their VTE updated immediately.
    /// Removed panes (in this window only) are dropped.
    pub fn apply_layout(
        &mut self,
        window_id: &str,
        layout_panes: &[LayoutPane],
    ) -> Vec<SyncAction> {
        let mut actions = Vec::new();

        // Build set of pane IDs from the layout (as %N strings)
        let layout_ids: HashMap<String, &LayoutPane> = layout_panes
            .iter()
            .map(|lp| (format!("%{}", lp.pane_number), lp))
            .collect();

        // Remove tracked panes in this window that aren't in the layout
        let dead: Vec<String> = self.panes
            .iter()
            .filter(|(_, tp)| tp.window_id == window_id && !layout_ids.contains_key(&tp.pane_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in dead {
            self.panes.remove(&id);
            actions.push(SyncAction::PaneRemoved { pane_id: id });
        }

        // Add new panes, update existing
        for (pane_id, lp) in &layout_ids {
            match self.panes.get_mut(pane_id) {
                Some(tracked) => {
                    // Update position
                    tracked.x = lp.x;
                    tracked.y = lp.y;

                    // Check for resize
                    let cur_w = tracked.processor.columns();
                    let cur_h = tracked.processor.screen_lines();
                    if lp.width != cur_w || lp.height != cur_h {
                        tracked.processor.resize(lp.height, lp.width);
                        actions.push(SyncAction::Resized {
                            pane_id: pane_id.clone(),
                            width: lp.width,
                            height: lp.height,
                        });
                    }
                }
                None => {
                    let processor = PaneProcessor::new(lp.height, lp.width);
                    self.panes.insert(pane_id.clone(), TrackedPane {
                        pane_id: pane_id.clone(),
                        window_id: window_id.to_string(),
                        pid: None,
                        x: lp.x,
                        y: lp.y,
                        processor,
                    });
                    actions.push(SyncAction::PaneAdded { pane_id: pane_id.clone() });
                }
            }
        }

        actions
    }

    /// Set the shell PID for a pane (called after querying tmux).
    pub fn set_pid(&mut self, pane_id: &str, pid: u32) {
        if let Some(tracked) = self.panes.get_mut(pane_id) {
            tracked.pid = Some(pid);
        }
    }

    /// Remove all panes in a window. Returns `PaneRemoved` actions.
    pub fn remove_window(&mut self, window_id: &str) -> Vec<SyncAction> {
        let dead: Vec<String> = self.panes
            .iter()
            .filter(|(_, tp)| tp.window_id == window_id)
            .map(|(id, _)| id.clone())
            .collect();

        let mut actions = Vec::with_capacity(dead.len());
        for id in dead {
            self.panes.remove(&id);
            actions.push(SyncAction::PaneRemoved { pane_id: id });
        }
        actions
    }

    /// Get a mutable reference to a pane's processor (for `%output` dispatch).
    pub fn get_processor_mut(&mut self, pane_id: &str) -> Option<&mut PaneProcessor> {
        self.panes.get_mut(pane_id).map(|tp| &mut tp.processor)
    }

    /// Get a tracked pane by ID.
    pub fn get(&self, pane_id: &str) -> Option<&TrackedPane> {
        self.panes.get(pane_id)
    }

    /// Iterate all tracked panes.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &TrackedPane)> {
        self.panes.iter()
    }

    pub fn len(&self) -> usize {
        self.panes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.panes.is_empty()
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

    #[test]
    fn adds_new_panes() {
        let mut reg = PaneRegistry::new();
        let actions = reg.apply_layout("@1", &[
            lp(0, 80, 24, 0, 0),
            lp(1, 80, 24, 0, 25),
        ]);
        assert_eq!(actions.len(), 2);
        assert!(has_action(&actions, &SyncAction::PaneAdded { pane_id: "%0".into() }));
        assert!(has_action(&actions, &SyncAction::PaneAdded { pane_id: "%1".into() }));
        assert_eq!(reg.len(), 2);
    }

    #[test]
    fn removes_dead_panes() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0), lp(2, 80, 24, 0, 25)]);
        assert_eq!(reg.len(), 2);

        // %2 disappears
        let actions = reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);
        assert!(has_action(&actions, &SyncAction::PaneRemoved { pane_id: "%2".into() }));
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn detects_resize() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);

        let actions = reg.apply_layout("@1", &[lp(1, 120, 40, 0, 0)]);
        assert!(has_action(&actions, &SyncAction::Resized {
            pane_id: "%1".into(), width: 120, height: 40,
        }));

        let tp = reg.get("%1").unwrap();
        assert_eq!(tp.processor.columns(), 120);
        assert_eq!(tp.processor.screen_lines(), 40);
    }

    #[test]
    fn no_op_unchanged() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);

        let actions = reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);
        assert!(actions.is_empty());
    }

    #[test]
    fn updates_position() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);
        assert_eq!(reg.get("%1").unwrap().x, 0);

        // Position changes but dimensions don't — no Resized action, but position updated
        reg.apply_layout("@1", &[lp(1, 80, 24, 50, 10)]);
        let tp = reg.get("%1").unwrap();
        assert_eq!(tp.x, 50);
        assert_eq!(tp.y, 10);
    }

    #[test]
    fn preserves_other_windows() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);
        reg.apply_layout("@2", &[lp(2, 80, 24, 0, 0)]);
        assert_eq!(reg.len(), 2);

        // Remove all panes from @1 — @2 unaffected
        let actions = reg.apply_layout("@1", &[]);
        assert!(has_action(&actions, &SyncAction::PaneRemoved { pane_id: "%1".into() }));
        assert_eq!(reg.len(), 1);
        assert!(reg.get("%2").is_some());
    }

    #[test]
    fn remove_window_clears_all() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0), lp(2, 80, 24, 0, 25)]);
        reg.apply_layout("@2", &[lp(3, 80, 24, 0, 0)]);
        assert_eq!(reg.len(), 3);

        let actions = reg.remove_window("@1");
        assert_eq!(actions.len(), 2);
        assert_eq!(reg.len(), 1);
        assert!(reg.get("%3").is_some());
    }

    #[test]
    fn set_pid_stores_value() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);

        assert!(reg.get("%1").unwrap().pid.is_none());
        reg.set_pid("%1", 12345);
        assert_eq!(reg.get("%1").unwrap().pid, Some(12345));
    }

    #[test]
    fn set_pid_unknown_pane_is_noop() {
        let mut reg = PaneRegistry::new();
        reg.set_pid("%99", 12345); // should not panic
    }

    #[test]
    fn processor_state_survives_layout_update() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]);

        // Feed data to the processor
        reg.get_processor_mut("%1").unwrap().process_chunk(b"hello world");
        assert_eq!(reg.get("%1").unwrap().processor.screen_line_text(0), "hello world");

        // Layout update (resize) should not lose processor state lines
        // (alacritty preserves content on resize)
        reg.apply_layout("@1", &[lp(1, 120, 24, 0, 0)]);
        assert_eq!(reg.get("%1").unwrap().processor.screen_line_text(0), "hello world");
    }

    #[test]
    fn get_processor_mut_unknown() {
        let mut reg = PaneRegistry::new();
        assert!(reg.get_processor_mut("%99").is_none());
    }

    #[test]
    fn multi_window_lifecycle() {
        let mut reg = PaneRegistry::new();

        // Window @1 with two panes
        reg.apply_layout("@1", &[lp(1, 100, 50, 0, 0), lp(2, 99, 50, 101, 0)]);
        // Window @2 with one pane
        reg.apply_layout("@2", &[lp(3, 80, 24, 0, 0)]);
        assert_eq!(reg.len(), 3);

        // Pane %2 splits into %2 and %4 in window @1
        let actions = reg.apply_layout("@1", &[
            lp(1, 100, 50, 0, 0),
            lp(2, 99, 25, 101, 0),
            lp(4, 99, 24, 101, 26),
        ]);
        assert!(has_action(&actions, &SyncAction::PaneAdded { pane_id: "%4".into() }));
        assert!(has_action(&actions, &SyncAction::Resized { pane_id: "%2".into(), width: 99, height: 25 }));
        assert_eq!(reg.len(), 4);

        // Close window @2
        let actions = reg.remove_window("@2");
        assert_eq!(actions.len(), 1);
        assert_eq!(reg.len(), 3);
    }
}
