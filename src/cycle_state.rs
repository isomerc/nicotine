use crate::window_manager::{EveWindow, WindowId, WindowManager};
use anyhow::Result;
use std::time::{Duration, Instant};

/// After Nicotine itself drives a focus change via `activate_window`, the
/// compositor commits that focus asynchronously and only then updates
/// `_NET_ACTIVE_WINDOW`. For this long we treat our own `current_index` as
/// the source of truth and skip `sync_with_active`, so a fast burst of
/// cycles isn't rewound to a stale read of the compositor's lagging focus.
/// Once it elapses (the user has paused), the next sync is honored so a
/// manual alt-tab / click switch is still picked up.
const ACTIVATION_GRACE: Duration = Duration::from_millis(300);

pub struct CycleState {
    current_index: usize,
    windows: Vec<EveWindow>,
    /// Optional ordered list of character names from characters.txt. When
    /// set, forward/backward cycling traverses this order, skipping any
    /// listed names that aren't currently logged in. When None, cycles
    /// through windows in whatever order the window manager reports them.
    character_order: Option<Vec<String>>,
    /// When we last drove an activation ourselves. Gates `sync_with_active`
    /// against the compositor's asynchronous focus commit — see
    /// `ACTIVATION_GRACE`.
    last_activated: Option<Instant>,
}

impl CycleState {
    pub fn new() -> Self {
        Self {
            current_index: 0,
            windows: Vec::new(),
            character_order: None,
            last_activated: None,
        }
    }

    pub fn set_character_order(&mut self, order: Option<Vec<String>>) {
        self.character_order = order;
    }

    /// Live view of the configured cycle order. The Daemon used to
    /// cache its own copy here, but the hot-reload thread only updated
    /// the copy on CycleState, not the Daemon's — `nicotine N` ended
    /// up using a snapshot from daemon startup instead of the latest
    /// panel-edited list. Callers should read this each time they
    /// need the order, never cache it for longer than a single
    /// operation.
    pub fn character_order(&self) -> Option<&[String]> {
        self.character_order.as_deref()
    }

    /// Indices into `self.windows` in the order forward-cycling should
    /// traverse them. If `character_order` is set, only listed characters
    /// who are currently logged in are included, in list order. Otherwise
    /// every window is included in detection order.
    fn cycle_indices(&self) -> Vec<usize> {
        if let Some(order) = &self.character_order {
            order
                .iter()
                .filter_map(|name| self.windows.iter().position(|w| &w.title == name))
                .collect()
        } else {
            (0..self.windows.len()).collect()
        }
    }

    pub fn update_windows(&mut self, windows: Vec<EveWindow>) {
        // Preserve which client we're on across a wholesale list rebuild.
        // `current_index` is positional, but the periodic rescan can return
        // windows in a different order or with an entry added/removed (a
        // client opened or closed). Without remapping by id, the index would
        // silently start pointing at a *different* client and the next cycle
        // would compute from the wrong base. Fall back to a clamp only when
        // the window we were on is genuinely gone.
        let current_id = self.windows.get(self.current_index).map(|w| w.id);
        self.windows = windows;
        if let Some(id) = current_id {
            if let Some(pos) = self.windows.iter().position(|w| w.id == id) {
                self.current_index = pos;
                return;
            }
        }
        if self.current_index >= self.windows.len() && !self.windows.is_empty() {
            self.current_index = 0;
        }
    }

    /// Windows in the user-configured cycle order. When `character_order`
    /// is set, returns only logged-in configured characters in that order;
    /// otherwise falls back to whatever order the window manager reports.
    /// Used by the list-view renderer so rows stay put as you cycle.
    /// Windows-only consumer (preview manager); kept cross-platform so
    /// future Linux UI can reuse it.
    #[cfg_attr(unix, allow(dead_code))]
    pub fn get_ordered_windows(&self) -> Vec<EveWindow> {
        self.cycle_indices()
            .into_iter()
            .map(|i| self.windows[i].clone())
            .collect()
    }

    /// Activate the EVE client whose title exactly matches `name`.
    /// No-op if that character isn't currently logged in. Used by
    /// per-character global hotkeys (Windows only).
    #[cfg_attr(unix, allow(dead_code))]
    pub fn switch_to_character(
        &mut self,
        name: &str,
        wm: &dyn WindowManager,
        minimize_inactive: bool,
    ) -> Result<()> {
        let target_idx = match self.windows.iter().position(|w| w.title == name) {
            Some(i) => i,
            None => return Ok(()),
        };
        if target_idx == self.current_index {
            // Already focused — ensure it's actually brought to
            // foreground (in case another app stole focus) and return.
            let id = self.windows[target_idx].id;
            wm.activate_window(id)?;
            self.last_activated = Some(Instant::now());
            return Ok(());
        }

        let previous_index = self.current_index;
        self.current_index = target_idx;

        let new_id = self.windows[target_idx].id;
        if minimize_inactive {
            let _ = wm.restore_window(new_id);
        }
        wm.activate_window(new_id)?;
        self.last_activated = Some(Instant::now());
        if minimize_inactive {
            let prev_id = self.windows[previous_index].id;
            let _ = wm.minimize_window(prev_id);
        }
        Ok(())
    }

    pub fn cycle_forward(&mut self, wm: &dyn WindowManager, minimize_inactive: bool) -> Result<()> {
        self.cycle_step(wm, minimize_inactive, 1)
    }

    pub fn cycle_backward(
        &mut self,
        wm: &dyn WindowManager,
        minimize_inactive: bool,
    ) -> Result<()> {
        self.cycle_step(wm, minimize_inactive, -1)
    }

    /// Advance through the cycle by `step` positions (1 = forward,
    /// -1 = backward). Wraps at both ends. Honors `character_order` if set.
    fn cycle_step(
        &mut self,
        wm: &dyn WindowManager,
        minimize_inactive: bool,
        step: isize,
    ) -> Result<()> {
        if self.windows.is_empty() {
            return Ok(());
        }

        let cycle = self.cycle_indices();
        if cycle.is_empty() {
            // character_order is set but none of the listed characters are
            // currently logged in — nothing to cycle to.
            return Ok(());
        }

        // Find where the currently-active window sits in the cycle list.
        // If the active window isn't in the cycle (e.g., user is on an
        // unlisted character), jump to the first or last entry depending
        // on direction.
        let position_in_cycle = cycle.iter().position(|&i| i == self.current_index);
        let next_position = match position_in_cycle {
            Some(p) => {
                let len = cycle.len() as isize;
                (((p as isize + step) % len) + len) as usize % cycle.len()
            }
            None => {
                if step > 0 {
                    0
                } else {
                    cycle.len() - 1
                }
            }
        };

        let previous_index = self.current_index;
        self.current_index = cycle[next_position];

        let new_window_id = self.windows[self.current_index].id;

        if minimize_inactive {
            let _ = wm.restore_window(new_window_id);
        }

        wm.activate_window(new_window_id)?;
        self.last_activated = Some(Instant::now());

        if minimize_inactive && previous_index != self.current_index {
            let previous_window_id = self.windows[previous_index].id;
            let _ = wm.minimize_window(previous_window_id);
        }

        Ok(())
    }

    // The accessors below are used by the unit tests and the preview
    // managers but not by every release code path; `#[allow(dead_code)]`
    // keeps them defined cross-platform without tripping the Windows
    // `cargo clippy -- -D warnings` job.

    /// Windows preview manager reads this for paint; on Linux the
    /// XComposite preview manager enumerates X11 windows directly so
    /// nothing in the Linux build calls it. Tests use it on both.
    #[cfg_attr(unix, allow(dead_code))]
    pub fn get_windows(&self) -> &[EveWindow] {
        &self.windows
    }

    #[allow(dead_code)]
    pub fn get_current_index(&self) -> usize {
        self.current_index
    }

    #[allow(dead_code)]
    pub fn set_current_index(&mut self, index: usize) {
        if index < self.windows.len() || self.windows.is_empty() {
            self.current_index = index;
        }
    }

    /// True if we drove an activation within the last `ACTIVATION_GRACE`.
    /// Callers can use this to skip the `get_active_window` round-trip +
    /// `sync_with_active` during a fast cycle burst: the sync would be a
    /// no-op anyway (we trust our own index inside the grace window), so the
    /// round-trip is pure latency on the hot path. Only the Linux evdev
    /// listeners call this; the Windows cycle path relies on the grace
    /// early-return inside `sync_with_active` instead.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn in_activation_grace(&self) -> bool {
        self.last_activated
            .is_some_and(|t| t.elapsed() < ACTIVATION_GRACE)
    }

    pub fn sync_with_active(&mut self, active_window: WindowId) {
        // Within the grace window after our own activation, the compositor's
        // reported active window may still be the *previous* one — its focus
        // commit is asynchronous. Trust `current_index` rather than rewinding
        // to that stale read, which is what made rapid cycling "jump back" or
        // skip. Once grace elapses (the user paused), honor the report so a
        // manual alt-tab / click switch is still picked up. See
        // `ACTIVATION_GRACE`.
        if self
            .last_activated
            .is_some_and(|at| at.elapsed() < ACTIVATION_GRACE)
        {
            return;
        }
        // Find which window is active and update current_index
        for (i, window) in self.windows.iter().enumerate() {
            if window.id == active_window {
                self.current_index = i;
                break;
            }
        }
    }

    /// Switch to a specific target number (1-indexed)
    /// If character_order is provided, uses that to map target -> character name
    /// Otherwise falls back to window list order
    pub fn switch_to(
        &mut self,
        target: usize,
        wm: &dyn WindowManager,
        minimize_inactive: bool,
        character_order: Option<&[String]>,
    ) -> Result<()> {
        if self.windows.is_empty() || target == 0 {
            return Ok(());
        }

        let target_index = if let Some(characters) = character_order {
            // Use character order from characters.txt
            let target_idx = target - 1; // Convert to 0-indexed
            if target_idx >= characters.len() {
                anyhow::bail!(
                    "Target {} is out of range (only {} characters configured)",
                    target,
                    characters.len()
                );
            }

            let target_name = &characters[target_idx];

            // Find window matching this character name
            self.windows
                .iter()
                .position(|w| w.title == *target_name)
                .ok_or_else(|| {
                    anyhow::anyhow!("Character '{}' not found in active windows", target_name)
                })?
        } else {
            // Fall back to window list order
            let target_idx = target - 1; // Convert to 0-indexed
            if target_idx >= self.windows.len() {
                anyhow::bail!(
                    "Target {} is out of range (only {} windows)",
                    target,
                    self.windows.len()
                );
            }
            target_idx
        };

        // Don't do anything if already on target
        if target_index == self.current_index {
            return Ok(());
        }

        let previous_index = self.current_index;
        self.current_index = target_index;

        let new_window_id = self.windows[self.current_index].id;

        if minimize_inactive {
            let _ = wm.restore_window(new_window_id);
        }

        wm.activate_window(new_window_id)?;
        self.last_activated = Some(Instant::now());

        if minimize_inactive {
            let previous_window_id = self.windows[previous_index].id;
            let _ = wm.minimize_window(previous_window_id);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_window(id: WindowId, title: &str) -> EveWindow {
        EveWindow {
            id,
            title: title.to_string(),
        }
    }

    #[test]
    fn test_new_cycle_state_is_empty() {
        let state = CycleState::new();
        assert_eq!(state.get_current_index(), 0);
        assert_eq!(state.get_windows().len(), 0);
    }

    #[test]
    fn test_update_windows() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(1, "EVE - Character 1"),
            create_test_window(2, "EVE - Character 2"),
            create_test_window(3, "EVE - Character 3"),
        ];

        state.update_windows(windows);
        assert_eq!(state.get_windows().len(), 3);
        assert_eq!(state.get_current_index(), 0);
    }

    #[test]
    fn test_update_windows_clamps_index() {
        let mut state = CycleState::new();

        // Set up with 5 windows and move to index 4
        let windows = vec![
            create_test_window(1, "EVE - Character 1"),
            create_test_window(2, "EVE - Character 2"),
            create_test_window(3, "EVE - Character 3"),
            create_test_window(4, "EVE - Character 4"),
            create_test_window(5, "EVE - Character 5"),
        ];
        state.update_windows(windows);
        state.current_index = 4; // Manually set to last index

        // Now update with only 2 windows
        let windows = vec![
            create_test_window(1, "EVE - Character 1"),
            create_test_window(2, "EVE - Character 2"),
        ];
        state.update_windows(windows);

        // Index should be clamped back to 0
        assert_eq!(state.get_current_index(), 0);
    }

    #[test]
    fn test_sync_with_active_updates_index() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(100, "EVE - Character 1"),
            create_test_window(200, "EVE - Character 2"),
            create_test_window(300, "EVE - Character 3"),
        ];
        state.update_windows(windows);

        // Sync with window id 300
        state.sync_with_active(300);
        assert_eq!(state.get_current_index(), 2);

        // Sync with window id 100
        state.sync_with_active(100);
        assert_eq!(state.get_current_index(), 0);
    }

    #[test]
    fn test_sync_with_active_nonexistent_window() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(100, "EVE - Character 1"),
            create_test_window(200, "EVE - Character 2"),
        ];
        state.update_windows(windows);
        state.current_index = 1;

        // Sync with non-existent window - index shouldn't change
        state.sync_with_active(999);
        assert_eq!(state.get_current_index(), 1);
    }

    #[test]
    fn test_get_windows_returns_slice() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(1, "EVE - Character 1"),
            create_test_window(2, "EVE - Character 2"),
        ];
        state.update_windows(windows);

        let returned_windows = state.get_windows();
        assert_eq!(returned_windows.len(), 2);
        assert_eq!(returned_windows[0].id, 1);
        assert_eq!(returned_windows[1].id, 2);
    }

    #[test]
    fn test_empty_windows_stays_at_zero() {
        let mut state = CycleState::new();

        // Update with empty list
        state.update_windows(vec![]);

        assert_eq!(state.get_current_index(), 0);
        assert_eq!(state.get_windows().len(), 0);
    }

    #[test]
    fn test_single_window_behavior() {
        let mut state = CycleState::new();
        let windows = vec![create_test_window(1, "EVE - Single Client")];
        state.update_windows(windows);

        // With a single window, we should stay at index 0
        assert_eq!(state.get_current_index(), 0);

        // Syncing with the only window should work
        state.sync_with_active(1);
        assert_eq!(state.get_current_index(), 0);
    }

    #[test]
    fn test_update_windows_preserves_valid_index() {
        let mut state = CycleState::new();

        // Start with 5 windows, move to index 2
        let windows = vec![
            create_test_window(1, "EVE - Character 1"),
            create_test_window(2, "EVE - Character 2"),
            create_test_window(3, "EVE - Character 3"),
            create_test_window(4, "EVE - Character 4"),
            create_test_window(5, "EVE - Character 5"),
        ];
        state.update_windows(windows);
        state.current_index = 2;

        // Update with 4 windows - index 2 is still valid
        let windows = vec![
            create_test_window(1, "EVE - Character 1"),
            create_test_window(2, "EVE - Character 2"),
            create_test_window(3, "EVE - Character 3"),
            create_test_window(4, "EVE - Character 4"),
        ];
        state.update_windows(windows);

        // Index should stay at 2 since it's still valid
        assert_eq!(state.get_current_index(), 2);
    }

    // Mock WindowManager for testing switch_to
    struct MockWindowManager {
        activated_windows: std::sync::Mutex<Vec<WindowId>>,
    }

    impl MockWindowManager {
        fn new() -> Self {
            Self {
                activated_windows: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn get_activated(&self) -> Vec<WindowId> {
            self.activated_windows.lock().unwrap().clone()
        }
    }

    impl WindowManager for MockWindowManager {
        fn get_eve_windows(&self) -> anyhow::Result<Vec<EveWindow>> {
            Ok(vec![])
        }

        fn activate_window(&self, window_id: WindowId) -> anyhow::Result<()> {
            self.activated_windows.lock().unwrap().push(window_id);
            Ok(())
        }

        fn stack_windows(
            &self,
            _windows: &[EveWindow],
            _config: &crate::config::Config,
        ) -> anyhow::Result<()> {
            Ok(())
        }

        fn get_active_window(&self) -> anyhow::Result<WindowId> {
            Ok(0)
        }

        fn minimize_window(&self, _window_id: WindowId) -> anyhow::Result<()> {
            Ok(())
        }

        fn restore_window(&self, _window_id: WindowId) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_switch_to_by_index_no_character_order() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
            create_test_window(300, "Gamma"),
        ];
        state.update_windows(windows);

        let wm = MockWindowManager::new();

        // Switch to target 2 (0-indexed: 1)
        state.switch_to(2, &wm, false, None).unwrap();
        assert_eq!(state.get_current_index(), 1);
        assert_eq!(wm.get_activated(), vec![200]);
    }

    #[test]
    fn test_switch_to_with_character_order() {
        let mut state = CycleState::new();
        // Windows in random order
        let windows = vec![
            create_test_window(100, "Gamma"),
            create_test_window(200, "Alpha"),
            create_test_window(300, "Beta"),
        ];
        state.update_windows(windows);

        let wm = MockWindowManager::new();

        // Character order defines: 1=Alpha, 2=Beta, 3=Gamma
        let char_order = vec!["Alpha".to_string(), "Beta".to_string(), "Gamma".to_string()];

        // Switch to target 1 (Alpha) - should find window 200
        state.switch_to(1, &wm, false, Some(&char_order)).unwrap();
        assert_eq!(state.get_current_index(), 1); // Index of Alpha in windows
        assert_eq!(wm.get_activated(), vec![200]);
    }

    #[test]
    fn test_switch_to_same_window_does_nothing() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
        ];
        state.update_windows(windows);
        state.current_index = 0;

        let wm = MockWindowManager::new();

        // Switch to target 1 when already on index 0
        state.switch_to(1, &wm, false, None).unwrap();

        // Should not have activated anything
        assert!(wm.get_activated().is_empty());
    }

    #[test]
    fn test_switch_to_out_of_range() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
        ];
        state.update_windows(windows);

        let wm = MockWindowManager::new();

        // Switch to target 5 when only 2 windows exist
        let result = state.switch_to(5, &wm, false, None);
        assert!(result.is_err());
    }

    #[test]
    fn test_switch_to_character_not_logged_in() {
        let mut state = CycleState::new();
        let windows = vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
        ];
        state.update_windows(windows);

        let wm = MockWindowManager::new();

        // Character order includes a character not in windows
        let char_order = vec!["Alpha".to_string(), "Beta".to_string(), "Gamma".to_string()];

        // Switch to target 3 (Gamma) - not logged in
        let result = state.switch_to(3, &wm, false, Some(&char_order));
        assert!(result.is_err());
    }

    #[test]
    fn test_switch_to_zero_does_nothing() {
        let mut state = CycleState::new();
        let windows = vec![create_test_window(100, "Alpha")];
        state.update_windows(windows);

        let wm = MockWindowManager::new();

        // Switch to target 0 should do nothing
        state.switch_to(0, &wm, false, None).unwrap();
        assert!(wm.get_activated().is_empty());
    }

    #[test]
    fn test_switch_to_empty_windows_does_nothing() {
        let mut state = CycleState::new();

        let wm = MockWindowManager::new();

        // Switch with no windows
        state.switch_to(1, &wm, false, None).unwrap();
        assert!(wm.get_activated().is_empty());
    }

    #[test]
    fn test_character_order_getter_starts_none() {
        let state = CycleState::new();
        assert!(state.character_order().is_none());
    }

    #[test]
    fn test_character_order_getter_returns_latest_after_set() {
        // Regression guard for the daemon-side cycle-order staleness
        // bug. Daemon used to hold its own snapshot of the order and
        // pass it to switch_to; the hot-reload thread only refreshed
        // the copy on CycleState, so `nicotine N` used the stale
        // snapshot from daemon startup. The fix routes the daemon's
        // Switch handler through this getter — so we need to know
        // that the getter sees every update.
        let mut state = CycleState::new();
        let first = vec!["Alpha".to_string(), "Beta".to_string()];
        state.set_character_order(Some(first.clone()));
        assert_eq!(state.character_order(), Some(first.as_slice()));

        let second = vec!["Beta".to_string(), "Alpha".to_string(), "Gamma".to_string()];
        state.set_character_order(Some(second.clone()));
        assert_eq!(state.character_order(), Some(second.as_slice()));

        state.set_character_order(None);
        assert!(state.character_order().is_none());
    }

    #[test]
    fn test_switch_to_uses_latest_character_order() {
        // End-to-end: after set_character_order replaces the order,
        // a switch_to call driven by the daemon (which now pulls the
        // order from CycleState every call) hits the right window.
        // The numbered targets must follow the latest list, not any
        // earlier one.
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
            create_test_window(300, "Gamma"),
        ]);

        let wm = MockWindowManager::new();

        // Old order: target 1 = Alpha (id 100).
        let old_order = vec!["Alpha".to_string(), "Beta".to_string(), "Gamma".to_string()];
        state.set_character_order(Some(old_order));

        // User edits the panel — new order puts Gamma first.
        let new_order = vec!["Gamma".to_string(), "Alpha".to_string(), "Beta".to_string()];
        state.set_character_order(Some(new_order));

        // The fixed daemon code reads the live order each call:
        let order: Vec<String> = state.character_order().unwrap().to_vec();
        state.switch_to(1, &wm, false, Some(&order)).unwrap();

        // Target 1 under the new order is Gamma (id 300). If the
        // daemon still cached the old order, this would have been
        // 100 (Alpha).
        assert_eq!(wm.get_activated(), vec![300]);
    }

    // --- switch_to_character (used by the Linux per-character
    // hotkeys path; previously only had Windows callers) ----------

    #[test]
    fn switch_to_character_activates_matching_window() {
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
        ]);
        state.current_index = 0;

        let wm = MockWindowManager::new();
        state.switch_to_character("Beta", &wm, false).unwrap();

        assert_eq!(wm.get_activated(), vec![200]);
        assert_eq!(state.get_current_index(), 1);
    }

    #[test]
    fn switch_to_character_unknown_name_is_noop() {
        // Per-character hotkey for a character that isn't logged in
        // should silently do nothing — not error, not change focus.
        // Matches the user-facing expectation that "I have F4 bound
        // to Charlie, but Charlie isn't logged in right now" doesn't
        // dump a stack trace.
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
        ]);
        state.current_index = 0;

        let wm = MockWindowManager::new();
        state.switch_to_character("Charlie", &wm, false).unwrap();

        assert!(wm.get_activated().is_empty());
        assert_eq!(state.get_current_index(), 0);
    }

    #[test]
    fn switch_to_character_already_active_reactivates() {
        // If the user's hotkey fires for the currently-active
        // character, the previous behavior of "no-op" felt broken —
        // the user expects the window to be re-foregrounded in case
        // some other app stole focus. switch_to_character should
        // re-activate without touching the index.
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
        ]);
        state.current_index = 1; // On Beta

        let wm = MockWindowManager::new();
        state.switch_to_character("Beta", &wm, false).unwrap();

        assert_eq!(wm.get_activated(), vec![200]);
        assert_eq!(state.get_current_index(), 1);
    }

    // --- activation-grace resync guard ------------------------------

    #[test]
    fn sync_is_ignored_within_activation_grace() {
        // Regression guard for the rapid-cycle "jump back": right after we
        // drive an activation, the compositor's _NET_ACTIVE_WINDOW can still
        // report the previous window. sync_with_active must NOT rewind to it.
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
            create_test_window(300, "Gamma"),
        ]);
        state.current_index = 2; // we just cycled to Gamma
        state.last_activated = Some(std::time::Instant::now());

        // Compositor still reports the old window (Alpha) — must be ignored.
        state.sync_with_active(100);
        assert_eq!(state.get_current_index(), 2);
    }

    #[test]
    fn sync_applies_after_activation_grace() {
        // Once grace elapses, a genuine external focus change (user clicked /
        // alt-tabbed to another client) is honored again.
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
            create_test_window(300, "Gamma"),
        ]);
        state.current_index = 2;
        state.last_activated = std::time::Instant::now().checked_sub(ACTIVATION_GRACE * 2);
        assert!(state.last_activated.is_some(), "test clock underflow");

        state.sync_with_active(100);
        assert_eq!(state.get_current_index(), 0);
    }

    #[test]
    fn sync_applies_when_never_activated() {
        // Fresh daemon, no activation yet: the first sync adopts whatever
        // client is currently focused.
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(100, "Alpha"),
            create_test_window(200, "Beta"),
        ]);
        assert!(state.last_activated.is_none());
        state.sync_with_active(200);
        assert_eq!(state.get_current_index(), 1);
    }

    // --- update_windows identity stability --------------------------

    #[test]
    fn update_windows_remaps_current_index_by_id_on_reorder() {
        // The rescan can return the same clients in a different order. The
        // index must follow the client we were on (by id), not stay put.
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(1, "A"),
            create_test_window(2, "B"),
            create_test_window(3, "C"),
        ]);
        state.current_index = 2; // on C (id 3)

        // Same clients, reordered: C is now first.
        state.update_windows(vec![
            create_test_window(3, "C"),
            create_test_window(1, "A"),
            create_test_window(2, "B"),
        ]);
        assert_eq!(state.get_current_index(), 0); // still on C
    }

    #[test]
    fn update_windows_falls_back_when_current_window_closes() {
        let mut state = CycleState::new();
        state.update_windows(vec![
            create_test_window(1, "A"),
            create_test_window(2, "B"),
            create_test_window(3, "C"),
        ]);
        state.current_index = 2; // on C (id 3)

        // C closed; only A and B remain. Index can't follow C, so it clamps
        // back into range rather than pointing past the end.
        state.update_windows(vec![create_test_window(1, "A"), create_test_window(2, "B")]);
        assert_eq!(state.get_current_index(), 0);
    }
}
