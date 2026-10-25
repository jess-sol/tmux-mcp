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
use std::time::Instant;

use tokio::sync::{Mutex, watch};

use crate::pane::osc::OscEvent;
use crate::pane::osc133::Osc133Phase;
use crate::pane::processor::PaneProcessor;
use crate::pane::snapshot::{Fact, PaneSnapshot};
use crate::parse::layout::LayoutPane;

/// Mutable pane state — behind a per-pane Mutex.
struct PaneState {
    pid: Option<u32>,
    x: usize,
    y: usize,
    processor: PaneProcessor,
    mode: Fact<PaneMode>,
    phase: Fact<Osc133Phase>,
    integration: Fact<bool>,
}

/// A pane's screen as the emulator renders it, with the *Pane snapshot* of
/// the same moment.
#[derive(Debug)]
pub struct Screen {
    pub snapshot: Arc<PaneSnapshot>,
    /// One string per row, one char per column, trailing blanks trimmed.
    pub rows: Vec<String>,
    /// (row, column)
    pub cursor: (usize, usize),
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    /// When the pane last wrote output.
    pub last_output: Option<Instant>,
}

/// The pane is past the *Pane version* a write was decided at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stale;

/// The tmux mode a pane is in, such as copy mode. While one is on, tmux hands
/// typed keys to the mode instead of the program.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneMode {
    Off,
    /// The mode's name, as tmux's `pane_mode` gives it (`copy-mode`).
    On(String),
}

impl PaneMode {
    /// What tmux's `pane_mode` format gave: a mode's name, or nothing.
    pub fn from_format(value: &str) -> PaneMode {
        match value.trim() {
            "" => PaneMode::Off,
            name => PaneMode::On(name.to_string()),
        }
    }
}

/// A tracked pane: immutable identity + locked mutable state.
pub struct TrackedPane {
    pub pane_id: String,
    pub window_id: String,
    state: Mutex<PaneState>,
    /// Bumped after every change, output included — handlers
    /// `subscribe_state()` and `select!` on `changed()` instead of polling.
    state_tx: watch::Sender<u64>,
    /// The latest *Pane snapshot*.
    snapshot_tx: watch::Sender<Arc<PaneSnapshot>>,
}

impl TrackedPane {
    pub fn new(
        pane_id: &str,
        window_id: &str,
        lines: usize,
        columns: usize,
        x: usize,
        y: usize,
    ) -> TrackedPane {
        let state = PaneState {
            pid: None,
            x,
            y,
            processor: PaneProcessor::new(lines, columns),
            mode: Fact::Unknown,
            phase: Fact::Unknown,
            integration: Fact::Unknown,
        };
        let snapshot = state.snapshot(0);
        TrackedPane {
            pane_id: pane_id.to_string(),
            window_id: window_id.to_string(),
            state: Mutex::new(state),
            state_tx: watch::channel(0u64).0,
            snapshot_tx: watch::channel(Arc::new(snapshot)).0,
        }
    }

    /// The latest *Pane snapshot*.
    pub fn snapshot(&self) -> Arc<PaneSnapshot> {
        self.snapshot_tx.borrow().clone()
    }

    /// The screen, rendered under the pane lock with the snapshot it shows.
    pub async fn screen(&self) -> Screen {
        let state = self.state.lock().await;
        Screen {
            snapshot: self.snapshot(),
            rows: state.processor.screen_text(),
            cursor: state.processor.cursor_position(),
            x: state.x,
            y: state.y,
            width: state.processor.columns(),
            height: state.processor.screen_lines(),
            last_output: state.processor.state().last_data,
        }
    }

    /// The output of the command `command_id` so far, while its record is
    /// kept. Output stands outside versions: it grows without a new one.
    pub async fn output(&self, command_id: u64) -> Option<String> {
        let state = self.state.lock().await;
        state
            .processor
            .state()
            .output(command_id)
            .map(str::to_string)
    }

    /// Feeds the pane's output to its emulator. An A, B, C or D marker sets
    /// the phase, and shows that the shell sends markers.
    pub async fn apply_output(&self, bytes: &[u8]) {
        let mut state = self.state.lock().await;
        let events = state.processor.process_chunk(bytes);
        let marked = events.iter().any(|osc| {
            matches!(
                osc.event,
                OscEvent::Osc133 {
                    marker: b'A'..=b'D',
                    ..
                }
            )
        });
        if marked {
            state.phase = marked_phase(state.processor.osc133_phase());
            state.integration = Fact::Known(true);
        }
        self.publish(&state);
    }

    /// Errs when the pane is past `version`; else the phase is Unknown until
    /// a marker answers the command line about to be written. Both happen
    /// under the pane lock, so a write decided at the old version is `Stale`.
    pub async fn sending_command(&self, version: u64) -> Result<(), Stale> {
        let mut state = self.state.lock().await;
        if self.snapshot().version != version {
            return Err(Stale);
        }
        state.phase = Fact::Unknown;
        self.publish(&state);
        Ok(())
    }

    /// Whether the shell sends markers: false once a command line of the
    /// daemon's waited for one in vain. A marker makes it true again.
    pub async fn set_integration(&self, integration: Fact<bool>) {
        let mut state = self.state.lock().await;
        state.integration = integration;
        self.publish(&state);
    }

    /// The pane's mode: Unknown when tmux announces a change without saying
    /// which, known once tmux says.
    pub async fn set_mode(&self, mode: Fact<PaneMode>) {
        let mut state = self.state.lock().await;
        state.mode = mode;
        self.publish(&state);
    }

    /// The shell's process, as tmux gives it.
    pub async fn set_pid(&self, pid: u32) {
        let mut state = self.state.lock().await;
        state.pid = Some(pid);
        self.publish(&state);
    }

    /// Moves and resizes the pane as tmux's layout places it; true when its
    /// size changed.
    pub async fn place(&self, x: usize, y: usize, width: usize, height: usize) -> bool {
        let mut state = self.state.lock().await;
        state.x = x;
        state.y = y;
        let size = (state.processor.columns(), state.processor.screen_lines());
        let resized = size != (width, height);
        if resized {
            state.processor.resize(height, width);
        }
        self.publish(&state);
        resized
    }

    /// Subscribe to state changes. Use `receiver.changed().await` to wake
    /// when the pane changes.
    pub fn subscribe_state(&self) -> watch::Receiver<u64> {
        self.state_tx.subscribe()
    }

    /// Publishes the *Pane snapshot* of `state` at the next *Pane version*
    /// when it differs from the last one, and wakes whatever waits on the pane.
    fn publish(&self, state: &PaneState) {
        let last = self.snapshot();
        let mut next = state.snapshot(last.version);
        if next != *last {
            next.version += 1;
            self.snapshot_tx.send_replace(Arc::new(next));
        }
        self.notify_state_changed();
    }

    /// Signal that pane state has changed (new output, completion, etc.).
    fn notify_state_changed(&self) {
        self.state_tx.send_modify(|v| *v += 1);
    }
}

impl PaneState {
    /// The facts a decision reads, labelled `version`.
    fn snapshot(&self, version: u64) -> PaneSnapshot {
        let terminal = self.processor.state();
        let reported = terminal.osc7_since_command;
        PaneSnapshot {
            version,
            mode: self.mode.clone(),
            phase: self.phase.clone(),
            integration: self.integration.clone(),
            input: self.processor.read_input(),
            input_garbled: self.processor.input_looks_garbled(),
            cwd: terminal.cwd.clone().filter(|_| reported),
            host: terminal.hostname.clone().filter(|_| reported),
            user: terminal.user.clone().filter(|_| reported),
            pid: self.pid,
            commands: terminal.commands.iter().cloned().collect(),
        }
    }
}

impl Screen {
    /// The last `n` rows above the blank ones below them, joined.
    pub fn last_rows(&self, n: usize) -> String {
        let end = self
            .rows
            .iter()
            .rposition(|row| !row.is_empty())
            .map_or(0, |i| i + 1);
        self.rows[end.saturating_sub(n)..end].join("\n")
    }
}

/// The phase a marker leaves: Unknown after D and between A and B, where the
/// markers do not say whether the shell takes a command.
fn marked_phase(phase: &Osc133Phase) -> Fact<Osc133Phase> {
    match phase {
        Osc133Phase::Idle | Osc133Phase::Prompt => Fact::Unknown,
        phase => Fact::Known(phase.clone()),
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
    Resized {
        pane_id: String,
        width: usize,
        height: usize,
    },
}

/// Manages all tracked panes across the session.
#[derive(Default)]
pub struct PaneRegistry {
    panes: HashMap<String, PaneHandle>,
}

impl PaneRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    // --- Public lookup API (called under read lock) ---

    /// Get a handle to a tracked pane. The Arc clone is cheap; the caller
    /// reads its snapshot, a command's output or its screen.
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
        let dead: Vec<String> = self
            .panes
            .iter()
            .filter(|(_, h)| {
                h.window_id == window_id && !layout_ids.contains_key(h.pane_id.as_str())
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &dead {
            self.panes.remove(id);
            actions.push(SyncAction::PaneRemoved {
                pane_id: id.clone(),
            });
        }

        // Add new panes, update existing
        for (pane_id, lp) in &layout_ids {
            if let Some(handle) = self.panes.get(pane_id) {
                if handle.place(lp.x, lp.y, lp.width, lp.height).await {
                    actions.push(SyncAction::Resized {
                        pane_id: pane_id.clone(),
                        width: lp.width,
                        height: lp.height,
                    });
                }
            } else {
                let tracked = TrackedPane::new(pane_id, window_id, lp.height, lp.width, lp.x, lp.y);
                self.panes.insert(pane_id.clone(), Arc::new(tracked));
                actions.push(SyncAction::PaneAdded {
                    pane_id: pane_id.clone(),
                });
            }
        }

        actions
    }

    /// Remove all panes in a window. Returns `PaneRemoved` actions.
    pub fn remove_window(&mut self, window_id: &str) -> Vec<SyncAction> {
        let dead: Vec<String> = self
            .panes
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
    use crate::pane::osc133::Osc133Phase;

    fn lp(pane_number: u32, width: usize, height: usize, x: usize, y: usize) -> LayoutPane {
        LayoutPane {
            pane_number,
            width,
            height,
            x,
            y,
        }
    }

    fn has_action(actions: &[SyncAction], expected: &SyncAction) -> bool {
        actions.iter().any(|a| a == expected)
    }

    #[tokio::test]
    async fn adds_new_panes() {
        let mut reg = PaneRegistry::new();
        let actions = reg
            .apply_layout("@1", &[lp(0, 80, 24, 0, 0), lp(1, 80, 24, 0, 25)])
            .await;
        assert_eq!(actions.len(), 2);
        assert!(has_action(
            &actions,
            &SyncAction::PaneAdded {
                pane_id: "%0".into()
            }
        ));
        assert!(has_action(
            &actions,
            &SyncAction::PaneAdded {
                pane_id: "%1".into()
            }
        ));
        assert_eq!(reg.len(), 2);
    }

    #[tokio::test]
    async fn removes_dead_panes() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0), lp(2, 80, 24, 0, 25)])
            .await;
        assert_eq!(reg.len(), 2);

        let actions = reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;
        assert!(has_action(
            &actions,
            &SyncAction::PaneRemoved {
                pane_id: "%2".into()
            }
        ));
        assert_eq!(reg.len(), 1);
    }

    #[tokio::test]
    async fn detects_resize() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;

        let actions = reg.apply_layout("@1", &[lp(1, 120, 40, 0, 0)]).await;
        assert!(has_action(
            &actions,
            &SyncAction::Resized {
                pane_id: "%1".into(),
                width: 120,
                height: 40,
            }
        ));

        let screen = reg.get_handle("%1").unwrap().screen().await;
        assert_eq!((screen.width, screen.height), (120, 40));
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
        assert_eq!(reg.get_handle("%1").unwrap().screen().await.x, 0);

        reg.apply_layout("@1", &[lp(1, 80, 24, 50, 10)]).await;
        let screen = reg.get_handle("%1").unwrap().screen().await;
        assert_eq!((screen.x, screen.y), (50, 10));
    }

    #[tokio::test]
    async fn preserves_other_windows() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;
        reg.apply_layout("@2", &[lp(2, 80, 24, 0, 0)]).await;
        assert_eq!(reg.len(), 2);

        let actions = reg.apply_layout("@1", &[]).await;
        assert!(has_action(
            &actions,
            &SyncAction::PaneRemoved {
                pane_id: "%1".into()
            }
        ));
        assert_eq!(reg.len(), 1);
        assert!(reg.get_handle("%2").is_some());
    }

    #[tokio::test]
    async fn remove_window_clears_all() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0), lp(2, 80, 24, 0, 25)])
            .await;
        reg.apply_layout("@2", &[lp(3, 80, 24, 0, 0)]).await;
        assert_eq!(reg.len(), 3);

        let actions = reg.remove_window("@1");
        assert_eq!(actions.len(), 2);
        assert_eq!(reg.len(), 1);
        assert!(reg.get_handle("%3").is_some());
    }

    #[tokio::test]
    async fn processor_state_survives_layout_update() {
        let mut reg = PaneRegistry::new();
        reg.apply_layout("@1", &[lp(1, 80, 24, 0, 0)]).await;
        let pane = reg.get_handle("%1").unwrap();
        pane.apply_output(b"hello world").await;

        reg.apply_layout("@1", &[lp(1, 120, 24, 0, 0)]).await;

        assert_eq!(pane.screen().await.rows[0], "hello world");
    }

    #[tokio::test]
    async fn a_pane_version_rises_only_with_a_decision_fact() {
        let copy_mode = || Fact::Known(PaneMode::On("copy-mode".into()));
        let pane = TrackedPane::new("%1", "@1", 24, 80, 0, 0);
        pane.set_mode(Fact::Known(PaneMode::Off)).await; // as a new pane's query does
        let start = pane.snapshot().version;

        pane.apply_output(b"\r\n\r\n\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\")
            .await;
        let prompt = pane.snapshot();
        // A redraw on the top line, two lines above the prompt.
        pane.apply_output(b"\x1b7\x1b[1;1Hclock 12:01\x1b8").await;
        let unchanged = pane.snapshot();
        pane.set_mode(Fact::Unknown).await;
        let changing = pane.snapshot();
        pane.set_mode(copy_mode()).await;
        let copy = pane.snapshot();
        pane.set_mode(copy_mode()).await;
        let same = pane.snapshot();

        assert!(prompt.version > start, "{prompt:?}");
        assert_eq!(unchanged.version, prompt.version);
        assert!(changing.version > prompt.version);
        assert_eq!(changing.mode, Fact::Unknown);
        assert!(copy.version > changing.version, "an Unknown became known");
        assert_eq!(copy.mode, copy_mode());
        assert_eq!(same.version, copy.version, "nothing changed");
    }

    #[tokio::test]
    async fn a_command_line_holds_the_phase_unknown_until_a_marker_answers() {
        let pane = TrackedPane::new("%1", "@1", 24, 80, 0, 0);
        let fresh = pane.snapshot();
        pane.apply_output(b"\x1b]133;A\x1b\\$ ").await;
        let drawing = pane.snapshot();
        pane.apply_output(b"\x1b]133;B\x1b\\").await;
        let prompt = pane.snapshot();

        let stale = pane.sending_command(fresh.version).await;
        let sent = pane.sending_command(prompt.version).await;
        let sending = pane.snapshot();
        pane.apply_output(b"ls\r\n\x1b]133;E;ls\x1b\\").await;
        let after_e = pane.snapshot();
        pane.apply_output(b"\x1b]133;C\x1b\\").await;
        let running = pane.snapshot();

        assert_eq!(fresh.phase, Fact::Unknown, "before any marker");
        assert_eq!(drawing.phase, Fact::Unknown, "between A and B");
        assert_eq!(prompt.phase, Fact::Known(Osc133Phase::Input));
        assert_eq!(stale, Err(Stale));
        assert_eq!(sent, Ok(()));
        assert_eq!(sending.phase, Fact::Unknown);
        assert!(sending.version > prompt.version);
        assert_eq!(after_e.phase, Fact::Unknown, "E answers nothing");
        assert_eq!(
            running.phase,
            Fact::Known(Osc133Phase::Executing {
                command: "ls".into()
            })
        );
    }

    #[tokio::test]
    async fn a_marker_shows_the_shell_sends_markers_even_after_a_wait_in_vain() {
        let pane = TrackedPane::new("%1", "@1", 24, 80, 0, 0);
        let fresh = pane.snapshot();
        pane.set_integration(Fact::Known(false)).await;
        let in_vain = pane.snapshot();
        pane.apply_output(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\")
            .await;
        let marked = pane.snapshot();

        assert_eq!(fresh.integration, Fact::Unknown);
        assert_eq!(in_vain.integration, Fact::Known(false));
        assert_eq!(marked.integration, Fact::Known(true));
    }

    #[tokio::test]
    async fn a_screen_comes_with_the_snapshot_it_was_rendered_at() {
        let pane = TrackedPane::new("%1", "@1", 5, 20, 3, 4);
        pane.set_pid(42).await;
        pane.apply_output(
            b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\ls\r\n\x1b]133;C\x1b\\a\r\nb\r\n\x1b]133;D;0\x1b\\",
        )
        .await;

        let resized = pane.place(3, 4, 30, 5).await;
        let moved = pane.place(0, 0, 30, 5).await;
        let screen = pane.screen().await;
        let id = screen.snapshot.commands[0].id;

        assert_eq!(screen.snapshot, pane.snapshot());
        assert_eq!(screen.snapshot.pid, Some(42));
        assert_eq!(screen.last_rows(3), "$ ls\na\nb");
        assert_eq!(screen.cursor, (3, 0));
        assert_eq!(
            (screen.x, screen.y, screen.width, screen.height),
            (0, 0, 30, 5)
        );
        assert!(screen.last_output.is_some());
        assert!(resized && !moved, "only a new size counts as resized");
        assert_eq!(pane.output(id).await.as_deref(), Some("a\r\nb\r\n"));
        assert_eq!(pane.output(id + 1).await, None);
    }

    #[tokio::test]
    async fn a_snapshot_lists_commands_newest_first() {
        let pane = TrackedPane::new("%1", "@1", 24, 80, 0, 0);
        let cycle = |command: &str| {
            format!(
                "\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\{command}\r\n\x1b]133;C\x1b\\\x1b]133;D;0\x1b\\"
            )
        };

        pane.apply_output(cycle("ls").as_bytes()).await;
        pane.apply_output(cycle("pwd").as_bytes()).await;
        pane.apply_output(b"\x1b]133;A\x1b\\$ \x1b]133;B\x1b\\")
            .await;

        let snapshot = pane.snapshot();
        let commands: Vec<&str> = snapshot
            .commands
            .iter()
            .map(|c| c.command.as_str())
            .collect();
        assert_eq!(commands, ["pwd", "ls"]);
    }

    #[test]
    fn get_handle_unknown() {
        let reg = PaneRegistry::new();
        assert!(reg.get_handle("%99").is_none());
    }

    #[tokio::test]
    async fn multi_window_lifecycle() {
        let mut reg = PaneRegistry::new();

        reg.apply_layout("@1", &[lp(1, 100, 50, 0, 0), lp(2, 99, 50, 101, 0)])
            .await;
        reg.apply_layout("@2", &[lp(3, 80, 24, 0, 0)]).await;
        assert_eq!(reg.len(), 3);

        let actions = reg
            .apply_layout(
                "@1",
                &[
                    lp(1, 100, 50, 0, 0),
                    lp(2, 99, 25, 101, 0),
                    lp(4, 99, 24, 101, 26),
                ],
            )
            .await;
        assert!(has_action(
            &actions,
            &SyncAction::PaneAdded {
                pane_id: "%4".into()
            }
        ));
        assert!(has_action(
            &actions,
            &SyncAction::Resized {
                pane_id: "%2".into(),
                width: 99,
                height: 25
            }
        ));
        assert_eq!(reg.len(), 4);

        let actions = reg.remove_window("@2");
        assert_eq!(actions.len(), 1);
        assert_eq!(reg.len(), 3);
    }
}
