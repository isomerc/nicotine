//! evdev mouse-button cycle listener with **live** binding reload.
//!
//! The previous incarnation captured `forward_button`, `backward_button`,
//! device path, and the `enable` flag by value at thread spawn and sat
//! inside a blocking `Device::fetch_events()`. Once you bound a new
//! button in the config panel, the listener thread couldn't see the
//! change until the daemon restarted. Same gap on `enable_mouse_buttons`:
//! flipping it on after startup did nothing, because the listener was
//! never spawned in the first place when it started out false.
//!
//! The fix: a shared `Arc<Mutex<MouseConfig>>` that the daemon's
//! hot-reload thread updates on every config change, and a `nix::poll`
//! with a short timeout in the listener so the thread wakes up
//! periodically to read the latest snapshot. Real input still arrives
//! immediately (poll returns as soon as the fd has data); the only thing
//! the timeout costs is up to 200 ms of latency on a binding change
//! actually taking effect — well below the bar of "user typing in the
//! panel notices a difference."

use crate::config::Config;
use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use anyhow::{Context, Result};
use evdev::{Device, InputEventKind, Key};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Snapshot of mouse-cycle settings shared with the daemon hot-reload
/// loop. Cheaply `Clone`-able — the listener clones it every iteration
/// and releases the mutex immediately so the daemon never blocks on
/// listener I/O.
#[derive(Clone, PartialEq, Eq)]
pub struct MouseConfig {
    pub enable: bool,
    pub forward_button: u16,
    pub backward_button: u16,
    pub mouse_device_name: Option<String>,
    pub mouse_device_path: Option<String>,
    pub minimize_inactive: bool,
}

impl MouseConfig {
    pub fn from_config(c: &Config) -> Self {
        Self {
            enable: c.enable_mouse_buttons,
            forward_button: c.forward_button,
            backward_button: c.backward_button,
            mouse_device_name: c.mouse_device_name.clone(),
            mouse_device_path: c.mouse_device_path.clone(),
            minimize_inactive: c.minimize_inactive,
        }
    }
}

/// Poll timeout for `nix::poll`. Caps the worst-case latency between a
/// hot-reload config change and the listener noticing. 200 ms is short
/// enough to feel instant in the panel and long enough that the
/// listener thread isn't burning CPU re-locking the mutex hundreds of
/// times a second.
const POLL_TIMEOUT_MS: u16 = 200;

pub struct MouseListener;

impl MouseListener {
    /// Spawn the listener thread. Always returns Ok — there's no
    /// startup work that can fail; the device-find / poll loop runs
    /// inside the thread and re-tries on its own.
    pub fn spawn(
        shared: Arc<Mutex<MouseConfig>>,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || Self::run_listener(shared, wm, state))
    }

    fn run_listener(
        shared: Arc<Mutex<MouseConfig>>,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
    ) {
        let mut device: Option<Device> = None;
        // Track the (name, path) pair we last used to find a device.
        // When the live config picks different ones we drop the device
        // and re-find on the next iteration.
        let mut current_dev_key: (Option<String>, Option<String>) = (None, None);
        // Single-shot log lines so we don't spam the daemon log on
        // every poll tick. Reset on transitions so the user gets one
        // line per significant state change.
        let mut announced_listening = false;
        let mut announced_idle = false;

        loop {
            let snap = shared.lock().unwrap().clone();

            if !snap.enable {
                if !announced_idle {
                    println!("Mouse listener idle (enable_mouse_buttons = false)");
                    announced_idle = true;
                    announced_listening = false;
                }
                device = None;
                current_dev_key = (None, None);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            let dev_key = (
                snap.mouse_device_name.clone(),
                snap.mouse_device_path.clone(),
            );
            if device.is_none() || dev_key != current_dev_key {
                current_dev_key = dev_key;
                device = match Self::find_mouse_device(
                    snap.mouse_device_name.as_deref(),
                    snap.mouse_device_path.as_deref(),
                ) {
                    Ok(d) => {
                        announced_idle = false;
                        announced_listening = false;
                        Some(d)
                    }
                    Err(e) => {
                        eprintln!(
                            "Mouse device not available ({}); retrying in 2s. \
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
                    "Listening for mouse buttons: forward={}, backward={}",
                    snap.forward_button, snap.backward_button
                );
                announced_listening = true;
            }

            // Pull the raw fd inside a tight scope so the immutable
            // borrow on `device` ends before we try to mutably borrow
            // it for fetch_events below — that conflict was the only
            // thing keeping `let Some(dev) = device.as_mut()` from
            // working. AsRawFd takes &self; the resulting i32 is Copy.
            let raw_fd = device.as_ref().expect("device set above").as_raw_fd();
            // SAFETY: the fd belongs to `device` which lives for the
            // duration of `pollfds`. We never drop `device` in this
            // block (only after deciding what to do next), so the
            // BorrowedFd outlives every use of `pollfds`.
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw_fd) };
            let mut pollfds = [PollFd::new(borrowed, PollFlags::POLLIN)];
            let n = match poll(&mut pollfds, PollTimeout::from(POLL_TIMEOUT_MS)) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Mouse poll failed ({}); reconnecting.", e);
                    device = None;
                    continue;
                }
            };
            if n == 0 {
                // Timeout — fall back to the top of the loop to
                // re-snapshot the config. No event was missed; the
                // kernel queues input until we read it.
                continue;
            }
            let revents = pollfds[0].revents().unwrap_or_else(PollFlags::empty);
            if !revents.contains(PollFlags::POLLIN) {
                // POLLHUP / POLLERR — device went away.
                eprintln!("Mouse device hung up; reconnecting.");
                device = None;
                continue;
            }
            // Collect into a Vec to detach the events from the device
            // borrow — otherwise the Err arm below can't reassign
            // `device = None` (the iterator's destructor would still
            // hold the borrow). InputEvent is Copy so this is cheap.
            let events_result = device
                .as_mut()
                .unwrap()
                .fetch_events()
                .map(|it| it.collect::<Vec<_>>());
            let events = match events_result {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Mouse device read failed ({}); reconnecting.", e);
                    device = None;
                    continue;
                }
            };
            for event in events {
                if let InputEventKind::Key(key) = event.kind() {
                    // Only handle press (value 1).
                    if event.value() != 1 {
                        continue;
                    }
                    let code = key.code();
                    if code == snap.forward_button {
                        if let Err(e) = Self::cycle_forward(&wm, &state, snap.minimize_inactive) {
                            eprintln!("Failed to cycle forward: {}", e);
                        }
                    } else if code == snap.backward_button {
                        if let Err(e) = Self::cycle_backward(&wm, &state, snap.minimize_inactive) {
                            eprintln!("Failed to cycle backward: {}", e);
                        }
                    }
                }
            }
        }
    }

    /// Find a usable mouse device. Order:
    ///   1. `mouse_device_name` exact-match
    ///   2. `mouse_device_path` open
    ///   3. autodetect: any `/dev/input/event*` advertising BTN_SIDE or
    ///      BTN_EXTRA, sorted alphabetically so the pick is stable
    ///      across reboots / hot-plug reorders
    fn find_mouse_device(
        configured_name: Option<&str>,
        configured_path: Option<&str>,
    ) -> Result<Device> {
        let devices_path = Path::new("/dev/input");

        if let Some(device_name) = configured_name {
            println!("Searching for device by name: {}", device_name);
            for entry in std::fs::read_dir(devices_path)? {
                let entry = entry?;
                let path = entry.path();
                let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                if !filename.starts_with("event") {
                    continue;
                }
                if let Ok(device) = Device::open(&path) {
                    if device.name() == Some(device_name) {
                        println!(
                            "Using configured mouse device by name: {} ({})",
                            device_name,
                            path.display()
                        );
                        return Ok(device);
                    }
                }
            }
            eprintln!(
                "Warning: Failed to find device with name '{}'. Trying other methods...",
                device_name
            );
        }

        if let Some(path_str) = configured_path {
            let path = Path::new(path_str);
            match Device::open(path) {
                Ok(device) => {
                    println!(
                        "Using configured mouse device by path: {} ({})",
                        device.name().unwrap_or("Unknown"),
                        path.display()
                    );
                    return Ok(device);
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to open configured mouse device '{}': {}",
                        path_str, e
                    );
                    eprintln!("Falling back to automatic device detection...");
                }
            }
        }

        // Autodetect with deterministic ordering. Logs all candidates so
        // multi-mouse users (e.g. gaming mouse + keyboard with built-in
        // pointer) can copy the right name into `mouse_device_name`.
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

        let mut candidates: Vec<(std::path::PathBuf, String)> = Vec::new();
        for path in &event_paths {
            if let Ok(device) = Device::open(path) {
                if device.supported_keys().is_some_and(|keys| {
                    keys.contains(Key::BTN_SIDE) || keys.contains(Key::BTN_EXTRA)
                }) {
                    candidates.push((path.clone(), device.name().unwrap_or("Unknown").to_string()));
                }
            }
        }

        if candidates.is_empty() {
            anyhow::bail!("No mouse device with side buttons found in /dev/input");
        }
        if candidates.len() > 1 {
            eprintln!("Multiple mice with side buttons detected:");
            for (path, name) in &candidates {
                eprintln!("  {} ({})", name, path.display());
            }
            eprintln!(
                "Auto-picked the first one alphabetically. To override, set \
                 `mouse_device_name` in ~/.config/nicotine/config.toml to the \
                 exact device name."
            );
        }
        let (path, name) = &candidates[0];
        println!("Found mouse device: {} ({})", name, path.display());
        Device::open(path).context("failed to reopen selected mouse device")
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
}
