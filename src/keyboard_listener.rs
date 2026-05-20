//! evdev keyboard cycle + per-character hotkey listener with **live**
//! binding reload. See the matching commentary on `mouse_listener` —
//! same shape, same hot-reload contract, plus the character_hotkeys
//! dispatch path that was missing from Linux entirely until now.

use crate::config::{CharacterHotkey, Config};
use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use anyhow::Result;
use evdev::{Device, InputEventKind, Key};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::collections::{HashMap, HashSet};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, PartialEq, Eq)]
pub struct KeyboardConfig {
    pub enable: bool,
    pub forward_key: u16,
    pub backward_key: u16,
    pub modifier_key: Option<u16>,
    pub keyboard_device_path: Option<String>,
    pub minimize_inactive: bool,
    pub character_hotkeys: HashMap<String, CharacterHotkey>,
}

impl KeyboardConfig {
    pub fn from_config(c: &Config) -> Self {
        Self {
            enable: c.enable_keyboard_buttons,
            forward_key: c.forward_key,
            backward_key: c.backward_key,
            modifier_key: c.modifier_key,
            keyboard_device_path: c.keyboard_device_path.clone(),
            minimize_inactive: c.minimize_inactive,
            character_hotkeys: c.character_hotkeys.clone(),
        }
    }
}

const POLL_TIMEOUT_MS: u16 = 200;

pub struct KeyboardListener;

impl KeyboardListener {
    pub fn spawn(
        shared: Arc<Mutex<KeyboardConfig>>,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || Self::run_listener(shared, wm, state))
    }

    fn run_listener(
        shared: Arc<Mutex<KeyboardConfig>>,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
    ) {
        let mut device: Option<Device> = None;
        let mut current_dev_path: Option<String> = None;
        // Modifier-down state. Reset on (re)connect because the kernel
        // doesn't replay release events for keys held during a device
        // disappearance.
        let mut pressed_modifiers: HashSet<u16> = HashSet::new();
        let mut announced_listening = false;
        let mut announced_idle = false;

        loop {
            let snap = shared.lock().unwrap().clone();

            if !snap.enable {
                if !announced_idle {
                    println!("Keyboard listener idle (enable_keyboard_buttons = false)");
                    announced_idle = true;
                    announced_listening = false;
                }
                device = None;
                current_dev_path = None;
                pressed_modifiers.clear();
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            if device.is_none() || current_dev_path != snap.keyboard_device_path {
                // Drop the old device first — see the matching note in
                // mouse_listener: a `device = match { Err => continue }`
                // doesn't assign on the Err path, which would otherwise
                // leave us using the stale device after a failed
                // device-path change.
                device = None;
                current_dev_path = snap.keyboard_device_path.clone();
                pressed_modifiers.clear();
                match Self::find_keyboard_device(snap.keyboard_device_path.as_deref()) {
                    Ok(d) => {
                        announced_idle = false;
                        announced_listening = false;
                        device = Some(d);
                    }
                    Err(e) => {
                        eprintln!(
                            "Keyboard device not available ({}); retrying in 2s. \
                             Check permissions on /dev/input/event* and whether \
                             your user is in the `input` group.",
                            e
                        );
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            }

            if !announced_listening {
                println!(
                    "Listening for keyboard keys: forward={} backward={} (+{} character hotkey(s))",
                    snap.forward_key,
                    snap.backward_key,
                    snap.character_hotkeys.len()
                );
                announced_listening = true;
            }

            // Re-derive the modifier-code set from the current snapshot.
            // Set of every key code that's used as a modifier somewhere
            // (main modifier_key + every character hotkey's optional
            // modifier). Recomputed each iteration so swapping
            // modifiers in the panel takes effect immediately.
            let modifier_codes: HashSet<u16> = std::iter::empty()
                .chain(snap.modifier_key)
                .chain(snap.character_hotkeys.values().filter_map(|hk| hk.modifier))
                .collect();
            // Forget pressed modifiers that are no longer modifiers in
            // any binding — otherwise a key the user un-bound stays
            // "stuck" in the pressed set.
            pressed_modifiers.retain(|c| modifier_codes.contains(c));

            // See mouse_listener::run_listener for why we go through
            // BorrowedFd::borrow_raw: evdev::Device is AsRawFd but not
            // AsFd, and the immutable borrow has to release before we
            // mutably borrow for fetch_events.
            let raw_fd = device.as_ref().expect("device set above").as_raw_fd();
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
            let mut pollfds = [PollFd::new(borrowed, PollFlags::POLLIN)];
            let n = match poll(&mut pollfds, PollTimeout::from(POLL_TIMEOUT_MS)) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Keyboard poll failed ({}); reconnecting.", e);
                    device = None;
                    continue;
                }
            };
            if n == 0 {
                continue;
            }
            let revents = pollfds[0].revents().unwrap_or_else(PollFlags::empty);
            if !revents.contains(PollFlags::POLLIN) {
                eprintln!("Keyboard device hung up; reconnecting.");
                device = None;
                continue;
            }
            // Detach events from the device borrow — see the same
            // collect() note in mouse_listener.
            let events_result = device
                .as_mut()
                .unwrap()
                .fetch_events()
                .map(|it| it.collect::<Vec<_>>());
            let events = match events_result {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Keyboard device read failed ({}); reconnecting.", e);
                    device = None;
                    continue;
                }
            };
            for event in events {
                let InputEventKind::Key(key) = event.kind() else {
                    continue;
                };
                let code = key.code();

                // Track press/release of modifier-eligible keys.
                if modifier_codes.contains(&code) {
                    if event.value() != 0 {
                        pressed_modifiers.insert(code);
                    } else {
                        pressed_modifiers.remove(&code);
                    }
                }

                // Only act on press / repeat.
                if event.value() == 0 {
                    continue;
                }

                // Modifier+backward must be checked first so the
                // same-key (Tab + Shift+Tab) pattern works.
                let main_modifier_held = snap
                    .modifier_key
                    .map(|m| pressed_modifiers.contains(&m))
                    .unwrap_or(false);
                if code == snap.backward_key && main_modifier_held {
                    if let Err(e) = Self::cycle_backward(&wm, &state, snap.minimize_inactive) {
                        eprintln!("Failed to cycle backward: {}", e);
                    }
                    continue;
                }
                if code == snap.forward_key {
                    if let Err(e) = Self::cycle_forward(&wm, &state, snap.minimize_inactive) {
                        eprintln!("Failed to cycle forward: {}", e);
                    }
                    continue;
                }
                if code == snap.backward_key {
                    if let Err(e) = Self::cycle_backward(&wm, &state, snap.minimize_inactive) {
                        eprintln!("Failed to cycle backward: {}", e);
                    }
                    continue;
                }

                let target =
                    resolve_character_hotkey(code, &pressed_modifiers, &snap.character_hotkeys);
                if let Some(name) = target {
                    if let Err(e) =
                        Self::switch_to_character(&name, &wm, &state, snap.minimize_inactive)
                    {
                        eprintln!("Failed to switch to {}: {}", name, e);
                    }
                }
            }
        }
    }

    fn find_keyboard_device(configured_path: Option<&str>) -> Result<Device> {
        if let Some(path_str) = configured_path {
            let path = Path::new(path_str);
            match Device::open(path) {
                Ok(device) => {
                    println!(
                        "Using configured keyboard device {} ({})",
                        device.name().unwrap_or("Unknown"),
                        path.display()
                    );
                    return Ok(device);
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to open configured keyboard device '{}': {}",
                        path_str, e
                    );
                    eprintln!("Falling back to automatic device detection...");
                }
            }
        }

        let devices_path = Path::new("/dev/input");
        let mut event_paths: Vec<_> = std::fs::read_dir(devices_path)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|s| s.starts_with("event"))
            })
            .collect();
        event_paths.sort();

        for path in event_paths {
            if let Ok(device) = Device::open(&path) {
                if device.supported_keys().is_some_and(|keys| {
                    keys.contains(Key::KEY_TAB)
                        || keys.contains(Key::KEY_LEFTSHIFT)
                        || keys.contains(Key::KEY_Z)
                }) {
                    println!(
                        "Found keyboard device: {} ({})",
                        device.name().unwrap_or("Unknown"),
                        path.display()
                    );
                    return Ok(device);
                }
            }
        }

        anyhow::bail!("No keyboard device found in /dev/input")
    }

    fn cycle_forward(
        wm: &Arc<dyn WindowManager>,
        state: &Arc<Mutex<CycleState>>,
        minimize_inactive: bool,
    ) -> Result<()> {
        let mut state = state.lock().unwrap();
        if let Ok(active) = wm.get_active_window() {
            state.sync_with_active(active);
        }
        state.cycle_forward(&**wm, minimize_inactive)?;
        Ok(())
    }

    fn cycle_backward(
        wm: &Arc<dyn WindowManager>,
        state: &Arc<Mutex<CycleState>>,
        minimize_inactive: bool,
    ) -> Result<()> {
        let mut state = state.lock().unwrap();
        if let Ok(active) = wm.get_active_window() {
            state.sync_with_active(active);
        }
        state.cycle_backward(&**wm, minimize_inactive)?;
        Ok(())
    }

    fn switch_to_character(
        name: &str,
        wm: &Arc<dyn WindowManager>,
        state: &Arc<Mutex<CycleState>>,
        minimize_inactive: bool,
    ) -> Result<()> {
        let mut state = state.lock().unwrap();
        if let Ok(active) = wm.get_active_window() {
            state.sync_with_active(active);
        }
        state.switch_to_character(name, &**wm, minimize_inactive)?;
        Ok(())
    }
}

/// Pick the character (if any) whose hotkey matches a key press.
///
/// Two-pass match so a binding *with* a modifier wins when its modifier
/// is held, even if a different character has the same key bound
/// *without* a modifier. The naive single-pass `HashMap::iter().find()`
/// returns whichever entry the hash function happens to visit first —
/// pressing "Shift+F1" when both "Shift+F1 → Alpha" and "F1 → Beta" are
/// bound would non-deterministically resolve to Alpha or Beta.
///
/// `vk == 0` is treated as a placeholder (the panel writes a
/// modifier-only entry before any key is bound) and never matches.
fn resolve_character_hotkey(
    code: u16,
    pressed_modifiers: &HashSet<u16>,
    hotkeys: &HashMap<String, CharacterHotkey>,
) -> Option<String> {
    // First pass: modifier-bearing bindings whose modifier is held.
    let with_modifier = hotkeys.iter().find(|(_, hk)| {
        hk.vk != 0
            && hk.vk == code
            && match hk.modifier {
                Some(m) => pressed_modifiers.contains(&m),
                None => false,
            }
    });
    if let Some((name, _)) = with_modifier {
        return Some(name.clone());
    }
    // Second pass: no-modifier bindings.
    hotkeys
        .iter()
        .find(|(_, hk)| hk.vk != 0 && hk.vk == code && hk.modifier.is_none())
        .map(|(name, _)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hk(vk: u16, modifier: Option<u16>) -> CharacterHotkey {
        CharacterHotkey { vk, modifier }
    }

    #[test]
    fn unmatched_code_returns_none() {
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, None)); // F1
        let pressed: HashSet<u16> = HashSet::new();
        assert_eq!(resolve_character_hotkey(0x71, &pressed, &hotkeys), None);
    }

    #[test]
    fn placeholder_vk_zero_never_matches() {
        // Panel writes vk=0 when the user picks a modifier before a
        // key. Pressing any key (including code 0, theoretically)
        // shouldn't dispatch a placeholder.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0, Some(42)));
        let pressed: HashSet<u16> = [42].into_iter().collect();
        assert_eq!(resolve_character_hotkey(0, &pressed, &hotkeys), None);
    }

    #[test]
    fn no_modifier_binding_matches_when_no_modifier_held() {
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, None));
        let pressed: HashSet<u16> = HashSet::new();
        assert_eq!(
            resolve_character_hotkey(0x70, &pressed, &hotkeys),
            Some("Alpha".to_string())
        );
    }

    #[test]
    fn modifier_binding_requires_modifier_held() {
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x10)));
        // Press F1 without Shift held.
        assert_eq!(
            resolve_character_hotkey(0x70, &HashSet::new(), &hotkeys),
            None
        );
        // Press F1 with Shift held.
        let pressed: HashSet<u16> = [0x10].into_iter().collect();
        assert_eq!(
            resolve_character_hotkey(0x70, &pressed, &hotkeys),
            Some("Alpha".to_string())
        );
    }

    #[test]
    fn modifier_binding_beats_no_modifier_binding_when_modifier_held() {
        // Regression guard for Bug C: HashMap iteration order would
        // otherwise pick either binding non-deterministically. This
        // pins down the contract: modifier-bearing match wins.
        //
        // We run the resolution many times so that even if a future
        // change re-introduces HashMap-order dependence, the test
        // catches it in expectation rather than relying on a single
        // hash seed.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("AlphaWithShift".to_string(), hk(0x70, Some(0x10))); // Shift+F1
        hotkeys.insert("BetaPlain".to_string(), hk(0x70, None)); // F1
        let pressed: HashSet<u16> = [0x10].into_iter().collect();
        for _ in 0..32 {
            assert_eq!(
                resolve_character_hotkey(0x70, &pressed, &hotkeys),
                Some("AlphaWithShift".to_string())
            );
        }
    }

    #[test]
    fn falls_back_to_no_modifier_binding_when_modifier_not_held() {
        // Same setup as above, but no modifier is held — should
        // resolve to the no-modifier binding.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("AlphaWithShift".to_string(), hk(0x70, Some(0x10)));
        hotkeys.insert("BetaPlain".to_string(), hk(0x70, None));
        let pressed: HashSet<u16> = HashSet::new();
        assert_eq!(
            resolve_character_hotkey(0x70, &pressed, &hotkeys),
            Some("BetaPlain".to_string())
        );
    }

    #[test]
    fn multiple_distinct_modifiers_pick_the_matching_one() {
        // Hotkeys: Shift+F1 → Alpha, Ctrl+F1 → Beta.
        // Pressing F1 with Ctrl held should pick Beta. Pressing F1
        // with Shift held should pick Alpha.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x10)));
        hotkeys.insert("Beta".to_string(), hk(0x70, Some(0x11)));

        let shift_only: HashSet<u16> = [0x10].into_iter().collect();
        assert_eq!(
            resolve_character_hotkey(0x70, &shift_only, &hotkeys),
            Some("Alpha".to_string())
        );

        let ctrl_only: HashSet<u16> = [0x11].into_iter().collect();
        assert_eq!(
            resolve_character_hotkey(0x70, &ctrl_only, &hotkeys),
            Some("Beta".to_string())
        );
    }

    #[test]
    fn modifier_binding_doesnt_match_when_no_modifier_held() {
        // Hotkey "Shift+F1 → Alpha" should NOT fire on plain F1, even
        // if no other binding exists for F1. The first-pass filter
        // requires the modifier to be in pressed_modifiers.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x10)));
        assert_eq!(
            resolve_character_hotkey(0x70, &HashSet::new(), &hotkeys),
            None
        );
    }
}
