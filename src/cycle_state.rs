use crate::paths;
use crate::window_manager::{EveWindow, WindowManager};
use anyhow::Result;
use std::fs;

pub struct CycleState {
    current_index: usize,
    windows: Vec<EveWindow>,
    /// Optional ordered list of character names from characters.txt. When
    /// set, forward/backward cycling traverses this order, skipping any
    /// listed names that aren't currently logged in. When None, cycles
    /// through windows in whatever order the window manager reports them.
    character_order: Option<Vec<String>>,
}

impl CycleState {
    pub fn new() -> Self {
        Self {
            current_index: 0,
            windows: Vec::new(),
            character_order: None,
        }
    }

    pub fn set_character_order(&mut self, order: Option<Vec<String>>) {
        self.character_order = order;
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
        self.windows = windows;
        // Clamp current index
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
            return Ok(());
        }

        let previous_index = self.current_index;
        self.current_index = target_idx;
        self.write_index();

        let new_id = self.windows[target_idx].id;
        if minimize_inactive {
            let _ = wm.restore_window(new_id);
        }
        wm.activate_window(new_id)?;
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
        self.write_index();

        let new_window_id = self.windows[self.current_index].id;

        if minimize_inactive {
            let _ = wm.restore_window(new_window_id);
        }

        wm.activate_window(new_window_id)?;

        if minimize_inactive && previous_index != self.current_index {
            let previous_window_id = self.windows[previous_index].id;
            let _ = wm.minimize_window(previous_window_id);
        }

        Ok(())
    }

    fn write_index(&self) {
        let _ = fs::write(paths::index_file_path(), self.current_index.to_string());
    }

    // The next three methods are called by the Linux overlay and the
    // unit tests but not by any release-mode Windows code path.
    // `#[allow(dead_code)]` keeps them defined cross-platform without
    // tripping the Windows `cargo clippy -- -D warnings` job.

    #[allow(dead_code)]
    pub fn read_index_from_file() -> Option<usize> {
        let path = paths::index_file_path();
        if path.exists() {
            fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
        } else {
            None
        }
    }

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

    pub fn sync_with_active(&mut self, active_window: u32) {
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
        self.write_index();

        let new_window_id = self.windows[self.current_index].id;

        if minimize_inactive {
            let _ = wm.restore_window(new_window_id);
        }

        wm.activate_window(new_window_id)?;

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

    fn create_test_window(id: u32, title: &str) -> EveWindow {
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
        activated_windows: std::sync::Mutex<Vec<u32>>,
    }

    impl MockWindowManager {
        fn new() -> Self {
            Self {
                activated_windows: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn get_activated(&self) -> Vec<u32> {
            self.activated_windows.lock().unwrap().clone()
        }
    }

    impl WindowManager for MockWindowManager {
        fn get_eve_windows(&self) -> anyhow::Result<Vec<EveWindow>> {
            Ok(vec![])
        }

        fn activate_window(&self, window_id: u32) -> anyhow::Result<()> {
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

        fn get_active_window(&self) -> anyhow::Result<u32> {
            Ok(0)
        }

        fn find_window_by_title(&self, _title: &str) -> anyhow::Result<Option<u32>> {
            Ok(None)
        }

        fn minimize_window(&self, _window_id: u32) -> anyhow::Result<()> {
            Ok(())
        }

        fn restore_window(&self, _window_id: u32) -> anyhow::Result<()> {
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
}
