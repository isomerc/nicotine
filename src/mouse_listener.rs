//! evdev mouse-button cycle listener with **live** binding reload and
//! **multi-device** listening.
//!
//! Multi-device matters: typical setups have a gaming mouse plus a
//! keyboard with a built-in pointer or scroll cluster, both of which
//! advertise `BTN_SIDE` / `BTN_EXTRA` in their capability bitmap. Old
//! behavior picked one (alphabetically) and ignored the other —
//! whichever mouse actually had the user's cycling buttons might
//! not be the one we listened on. Now we open every candidate and
//! `nix::poll` across all their fds in a single call. Side-button
//! presses on any of them cycle the active client.
//!
//! The hot-reload contract from the earlier refactor is preserved: a
//! shared `Arc<Mutex<MouseConfig>>` carries the latest button codes /
//! enable flag, the listener re-snapshots it every iteration, and a
//! 200 ms poll timeout caps the worst-case latency for panel-driven
//! changes to take effect.

use crate::config::Config;
use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use anyhow::Result;
use evdev::{Device, InputEventKind, Key};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Poll timeout. Caps latency from a panel-driven binding change to the
/// listener noticing. 200 ms is responsive without burning CPU.
const POLL_TIMEOUT_MS: u16 = 200;

/// Coalescing window for cycle triggers. Some mice expose the same physical
/// side buttons on two `/dev/input/event*` nodes, so one click is delivered
/// twice; in autodetect mode we listen on both, which would cycle twice. A
/// second trigger within this window is dropped. 30 ms is far below a human
/// double-press (caps cycling at ~33/s) but well above the sub-millisecond
/// gap between a duplicated press's two reports.
const CYCLE_DEBOUNCE: Duration = Duration::from_millis(30);

pub struct MouseListener;

impl MouseListener {
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
        // Every side-button-capable device we're currently listening on.
        // Empty between scans (e.g. all devices unplugged) or while
        // `enable` is false. Populated by `find_mouse_devices` — if the
        // user pinned a specific device by name/path, it's the only
        // entry; in autodetect mode it's every matching event device.
        let mut devices: Vec<Device> = Vec::new();
        let mut current_dev_key: (Option<String>, Option<String>) = (None, None);
        let mut announced_listening = false;
        let mut announced_idle = false;
        // Last time we acted on a cycle trigger, for de-duping echoed presses
        // across multiple device nodes. See `CYCLE_DEBOUNCE`.
        let mut last_cycle: Option<Instant> = None;

        loop {
            let snap = shared.lock().unwrap().clone();

            if !snap.enable {
                if !announced_idle {
                    println!("Mouse listener idle (enable_mouse_buttons = false)");
                    announced_idle = true;
                    announced_listening = false;
                }
                devices.clear();
                current_dev_key = (None, None);
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            // Rebuild the device list on (a) startup / empty list (e.g.
            // all devices died), or (b) any panel-driven change to the
            // configured name/path. Otherwise keep the live list — it
            // only shrinks via the POLLHUP / read-error paths below.
            let dev_key = (
                snap.mouse_device_name.clone(),
                snap.mouse_device_path.clone(),
            );
            if devices.is_empty() || dev_key != current_dev_key {
                devices.clear();
                current_dev_key = dev_key;
                match Self::find_mouse_devices(
                    snap.mouse_device_name.as_deref(),
                    snap.mouse_device_path.as_deref(),
                ) {
                    Ok(found) if !found.is_empty() => {
                        announced_idle = false;
                        announced_listening = false;
                        devices = found;
                    }
                    Ok(_) => {
                        eprintln!(
                            "No mouse devices with side buttons found; retrying in 2s. \
                             Check permissions on /dev/input/event* and whether your \
                             user is in the `input` group."
                        );
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    Err(e) => {
                        eprintln!("Mouse device scan failed ({}); retrying in 2s.", e);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            }

            if !announced_listening {
                println!(
                    "Listening on {} mouse device(s) for buttons forward={} backward={}",
                    devices.len(),
                    snap.forward_button,
                    snap.backward_button
                );
                announced_listening = true;
            }

            // Build the pollfd array. We wrap each device's raw fd in a
            // BorrowedFd; the fds outlive `pollfds` because we don't
            // mutate `devices` while pollfds is alive (the borrow
            // checker doesn't know this — borrow_raw is unsafe — but
            // the control flow guarantees it).
            //
            // SAFETY: each fd belongs to a `Device` in `devices`. We
            // never drop / remove from `devices` while pollfds is in
            // scope; we collect ready/dead indices first, drop pollfds,
            // then act on `devices`.
            let mut pollfds: Vec<PollFd> = devices
                .iter()
                .map(|d| {
                    let raw = d.as_raw_fd();
                    let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
                    PollFd::new(borrowed, PollFlags::POLLIN)
                })
                .collect();

            let n = match poll(&mut pollfds, PollTimeout::from(POLL_TIMEOUT_MS)) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!(
                        "Mouse poll failed ({}); dropping devices and rescanning.",
                        e
                    );
                    devices.clear();
                    continue;
                }
            };
            if n == 0 {
                // Timeout: jump back to top, re-snapshot config.
                continue;
            }

            // Note which indices are ready vs. hung up. Collect both
            // BEFORE touching `devices` so pollfds (and its raw-fd
            // BorrowedFds) can drop first.
            let mut ready: Vec<usize> = Vec::new();
            let mut dead: Vec<usize> = Vec::new();
            for (i, pfd) in pollfds.iter().enumerate() {
                let revents = pfd.revents().unwrap_or_else(PollFlags::empty);
                if revents.contains(PollFlags::POLLIN) {
                    ready.push(i);
                } else if revents.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
                    dead.push(i);
                }
            }
            // Now safe to mutate devices.

            for &i in &ready {
                // Detach events from the device borrow — Vec<InputEvent>
                // releases the iterator's reference before we may need
                // to remove `devices[i]` from the list below.
                let events_result = devices[i].fetch_events().map(|it| it.collect::<Vec<_>>());
                let events = match events_result {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "Mouse device '{}' read failed ({}); dropping.",
                            devices[i].name().unwrap_or("Unknown"),
                            e
                        );
                        dead.push(i);
                        continue;
                    }
                };
                for event in events {
                    if let InputEventKind::Key(key) = event.kind() {
                        if event.value() != 1 {
                            continue;
                        }
                        let code = key.code();
                        let is_cycle = code == snap.forward_button || code == snap.backward_button;
                        if is_cycle {
                            // Drop a duplicate press echoed by a second device
                            // node within the debounce window.
                            let now = Instant::now();
                            if last_cycle.is_some_and(|t| now.duration_since(t) < CYCLE_DEBOUNCE) {
                                continue;
                            }
                            last_cycle = Some(now);
                            let result = if code == snap.forward_button {
                                Self::cycle_forward(&wm, &state, snap.minimize_inactive)
                            } else {
                                Self::cycle_backward(&wm, &state, snap.minimize_inactive)
                            };
                            if let Err(e) = result {
                                eprintln!("Failed to cycle: {}", e);
                            }
                        }
                    }
                }
            }

            // Remove dead devices in reverse index order so earlier
            // indices stay valid. dedup() because a device that errored
            // on read after being in `ready` ends up in both lists.
            dead.sort_unstable();
            dead.dedup();
            for &i in dead.iter().rev() {
                let name = devices[i].name().unwrap_or("Unknown").to_string();
                eprintln!("Mouse device '{}' hung up; dropping from listener.", name);
                devices.remove(i);
                announced_listening = false;
            }
        }
    }

    /// Build the list of mouse devices to listen on.
    ///
    /// - `configured_name`: exact match against `Device::name()`. If
    ///   found returns just that device; if not, falls through to path.
    /// - `configured_path`: open that specific event node. If it opens,
    ///   returns just that device.
    /// - Otherwise: every `/dev/input/event*` that advertises
    ///   `BTN_SIDE` or `BTN_EXTRA`. The listener polls them all so
    ///   the user doesn't have to know which device their cycling
    ///   buttons live on.
    fn find_mouse_devices(
        configured_name: Option<&str>,
        configured_path: Option<&str>,
    ) -> Result<Vec<Device>> {
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
                        return Ok(vec![device]);
                    }
                }
            }
            eprintln!(
                "Warning: Failed to find device with name '{}'. Falling back to autodetect.",
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
                    return Ok(vec![device]);
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

        // Autodetect: open every event node that advertises side
        // buttons. Sorted by filename for deterministic startup logs.
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

        let mut found: Vec<Device> = Vec::new();
        for path in &event_paths {
            if let Ok(device) = Device::open(path) {
                if device.supported_keys().is_some_and(|keys| {
                    keys.contains(Key::BTN_SIDE) || keys.contains(Key::BTN_EXTRA)
                }) {
                    println!(
                        "Listening on mouse device: {} ({})",
                        device.name().unwrap_or("Unknown"),
                        path.display()
                    );
                    found.push(device);
                }
            }
        }

        Ok(found)
    }

    fn cycle_forward(
        wm: &Arc<dyn WindowManager>,
        state: &Arc<Mutex<CycleState>>,
        minimize_inactive: bool,
    ) -> Result<()> {
        Self::run_cycle(wm, state, minimize_inactive, 1)
    }

    fn cycle_backward(
        wm: &Arc<dyn WindowManager>,
        state: &Arc<Mutex<CycleState>>,
        minimize_inactive: bool,
    ) -> Result<()> {
        Self::run_cycle(wm, state, minimize_inactive, -1)
    }

    /// Shared lock → (grace-gated) sync-with-active → cycle path for both
    /// directions. The `get_active_window` round-trip is skipped inside the
    /// activation grace window, where `sync_with_active` would no-op anyway,
    /// so it's pure latency on a fast burst.
    fn run_cycle(
        wm: &Arc<dyn WindowManager>,
        state: &Arc<Mutex<CycleState>>,
        minimize_inactive: bool,
        step: isize,
    ) -> Result<()> {
        let mut state = state.lock().unwrap();
        if !state.in_activation_grace() {
            if let Ok(active) = wm.get_active_window() {
                state.sync_with_active(active);
            }
        }
        if step >= 0 {
            state.cycle_forward(&**wm, minimize_inactive)?;
        } else {
            state.cycle_backward(&**wm, minimize_inactive)?;
        }
        Ok(())
    }
}
