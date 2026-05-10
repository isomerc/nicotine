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
                current_dev_path = snap.keyboard_device_path.clone();
                pressed_modifiers.clear();
                device = match Self::find_keyboard_device(snap.keyboard_device_path.as_deref()) {
                    Ok(d) => {
                        announced_idle = false;
                        announced_listening = false;
                        Some(d)
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
                };
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

                // Per-character hotkey. vk == 0 means the panel saved a
                // modifier-only placeholder before a key was bound;
                // skip those.
                let target = snap
                    .character_hotkeys
                    .iter()
                    .find(|(_, hk)| {
                        hk.vk != 0
                            && hk.vk == code
                            && match hk.modifier {
                                None => true,
                                Some(m) => pressed_modifiers.contains(&m),
                            }
                    })
                    .map(|(name, _)| name.clone());
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
