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
use crate::evdev_util::{EventDeviceHelper, InputDeviceType};
use crate::window_manager::WindowManager;
use anyhow::Result;
use evdev::InputEventKind;
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

/// Coalescing window for cycle triggers. Some mice expose the same physical
/// side buttons on two `/dev/input/event*` nodes, so one click is delivered
/// twice; in autodetect mode we listen on both, which would cycle twice. A
/// second trigger within this window is dropped. 30 ms is far below a human
/// double-press (caps cycling at ~33/s) but well above the sub-millisecond
/// gap between a duplicated press's two reports.
const CYCLE_DEBOUNCE: Duration = Duration::from_millis(30);

/// How often autodetect mode re-reads `/dev/input` for mice that
/// appeared after the last scan.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

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
        let mut mice = EventDeviceHelper::new(InputDeviceType::Mouse);
        let mut last_rescan: Option<Instant> = None;

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
                mice.reset();
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            mice.check_update_pinned_device(
                snap.mouse_device_name.as_deref(),
                snap.mouse_device_path.as_deref(),
            );

            if mice.needs_scan() || last_rescan.is_none_or(|t| t.elapsed() >= RESCAN_INTERVAL) {
                last_rescan = Some(Instant::now());

                match mice.scan_devices() {
                    Ok(res) => {
                        if res.removed_devices {
                            announced_idle = false;
                            announced_listening = false;
                        }

                        if !res.new_devices.is_empty() {
                            announced_idle = false;
                            announced_listening = false;

                            for result in &res.new_devices {
                                println!(
                                    "Listening on mouse device: {} ({})",
                                    result.name,
                                    result.path.display()
                                );
                            }
                        }
                    }

                    Err(e) => {
                        eprintln!("Mouse device scan failed ({}); retrying in 2s.", e);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }

                if mice.num_devices() == 0 {
                    eprintln!(
                        "No mouse devices with side buttons found; retrying in 2s. \
                         Check permissions on /dev/input/event* and whether your \
                         user is in the `input` group."
                    );
                    std::thread::sleep(Duration::from_secs(2));
                    continue;
                }
            }

            if !announced_listening {
                println!(
                    "Listening on {} mouse device(s) for buttons forward={} backward={}",
                    mice.num_devices(),
                    snap.forward_button,
                    snap.backward_button
                );
                announced_listening = true;
            }

            let devices_dropped = mice.poll_devices(|event| {
                if let InputEventKind::Key(key) = event.kind() {
                    if event.value() != 1 {
                        return;
                    }
                    let code = key.code();
                    let is_cycle = code == snap.forward_button || code == snap.backward_button;
                    if is_cycle {
                        // Drop a duplicate press echoed by a second device
                        // node within the debounce window.
                        let now = Instant::now();
                        if last_cycle.is_some_and(|t| now.duration_since(t) < CYCLE_DEBOUNCE) {
                            return;
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
            });

            if devices_dropped > 0 {
                announced_listening = false;
            }
        }
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
