//! Shared evdev helpers for the Linux mouse + keyboard listeners.

use anyhow::Result;
use evdev::{Device, InputEvent, Key};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::path::{Path, PathBuf};

const INPUT_DIR: &str = "/dev/input";

const POLL_TIMEOUT_MS: u16 = 200;

pub enum InputDeviceType {
    Keyboard,
    Mouse,
}

pub struct ScannedDevice {
    pub name: String,
    pub path: PathBuf,
}

pub struct ScanDevicesResult {
    pub removed_devices: bool,
    pub new_devices: Vec<ScannedDevice>,
}

pub struct EventDeviceHelper {
    device_type: InputDeviceType,
    pinned_device_name: Option<String>,
    pinned_device_path: Option<String>,
    devices: Vec<(PathBuf, Device)>,
    needs_scan: bool,
}

impl EventDeviceHelper {
    pub fn new(device_type: InputDeviceType) -> Self {
        Self {
            device_type,
            pinned_device_name: None,
            pinned_device_path: None,
            devices: Vec::new(),
            needs_scan: true,
        }
    }

    pub fn reset(&mut self) {
        self.pinned_device_name = None;
        self.pinned_device_path = None;
        self.devices.clear();
        self.needs_scan = true;
    }

    pub fn num_devices(&self) -> usize {
        self.devices.len()
    }

    pub fn needs_scan(&self) -> bool {
        self.needs_scan || self.devices.is_empty()
    }

    /// Updates the pinned device details and makes `needs_scan` return true if they changed
    pub fn check_update_pinned_device(
        &mut self,
        device_name: Option<&str>,
        device_path: Option<&str>,
    ) {
        if self.pinned_device_name.as_deref() != device_name {
            self.pinned_device_name = device_name.map(String::from);
            self.needs_scan = true;
        }

        if self.pinned_device_path.as_deref() != device_path {
            self.pinned_device_path = device_path.map(String::from);
            self.needs_scan = true;
        }
    }

    /// Scans for new input devices and returns a list of newly detected devices
    pub fn scan_devices(&mut self) -> Result<ScanDevicesResult> {
        if let Some(device_name) = self.pinned_device_name.as_deref() {
            let removed_devices = {
                let len = self.devices.len();
                self.devices.retain(|(_, d)| d.name() == Some(device_name));
                len != self.devices.len()
            };

            if !self.devices.is_empty() {
                return Ok(ScanDevicesResult {
                    removed_devices,
                    new_devices: Vec::new()
                });
            }

            println!("Searching for device by name: {}", device_name);
            for path in event_nodes()? {
                if let Ok(device) = Device::open(&path) {
                    if device.name() == Some(device_name) {
                        println!(
                            "Found device: {} ({})",
                            device.name().unwrap(),
                            path.display()
                        );
                        let new_device_info = ScannedDevice {
                            name: device_name.to_string(),
                            path: path.clone(),
                        };

                        self.devices.push((path, device));

                        self.needs_scan = false;
                        return Ok(ScanDevicesResult {
                            removed_devices,
                            new_devices: vec![new_device_info],
                        });
                    }
                }
            }
            eprintln!(
                "Warning: Failed to find device with name '{}'. Falling back to autodetect.",
                device_name
            );
        }

        if let Some(path_str) = self.pinned_device_path.as_deref() {
            let removed_devices = {
                let len = self.devices.len();
                self.devices.retain(|(p, _)| p == path_str);
                len != self.devices.len()
            };

            let path = Path::new(path_str);
            match Device::open(path) {
                Ok(device) => {
                    let new_device_info = ScannedDevice {
                        name: device.name().map_or("Unknown".to_string(), String::from),
                        path: path.to_path_buf(),
                    };

                    self.devices.push((path.to_path_buf(), device));
                    self.needs_scan = false;
                    return Ok(ScanDevicesResult {
                        removed_devices,
                        new_devices: vec![new_device_info],
                    });
                }
                Err(e) => {
                    eprintln!("Warning: Failed to open device '{}': {}", path_str, e);
                    eprintln!("Falling back to automatic device detection...")
                }
            }
        }

        let mut results = Vec::new();
        for path in event_nodes()? {
            // Only open nodes we haven't opened yet
            if self.contains_device_path(&path) {
                continue;
            }

            let Ok(device) = Device::open(&path) else {
                continue;
            };

            let is_match = match self.device_type {
                InputDeviceType::Keyboard => Self::is_keyboard(&device),
                InputDeviceType::Mouse => Self::is_mouse(&device),
            };

            if is_match {
                results.push(ScannedDevice {
                    name: device.name().map_or(String::from("Unknown"), String::from),
                    path: path.clone(),
                });

                self.devices.push((path, device));
            }
        }

        self.needs_scan = self.devices.is_empty();
        Ok(ScanDevicesResult {
            removed_devices: false,
            new_devices: results
        })
    }

    /// Polls the currently tracked devices and calls `f` for every event produced by those devices.
    /// Returns the number of devices dropped
    pub fn poll_devices<F>(&mut self, mut f: F) -> usize
    where
        F: FnMut(&InputEvent),
    {
        let mut poll_result = match poll_ready_or_dead(self.devices.iter().map(|(_, d)| d)) {
            Ok(res) => res,
            Err(e) => {
                eprintln!("EventDevice poll failed ({}); dropping devices.", e);

                let num_devices = self.devices.len();
                self.devices.clear();
                return num_devices;
            }
        };

        if poll_result.timedout {
            return 0;
        }

        for &i in &poll_result.ready {
            // Detach events from the device borrow — Vec<InputEvent>
            // releases the iterator's reference before we may need
            // to remove `devices[i]` from the list below.
            let events_result = self.devices[i]
                .1
                .fetch_events()
                .map(|it| it.collect::<Vec<_>>());
            let events = match events_result {
                Ok(v) => v,
                Err(e) => {
                    eprintln!(
                        "EventDevice '{}' read failed ({}); dropping.",
                        self.devices[i].1.name().unwrap_or("Unknown"),
                        e
                    );
                    poll_result.dead.push(i);
                    continue;
                }
            };

            for event in &events {
                f(event);
            }
        }

        // Remove dead devices in reverse index order so earlier
        // indices stay valid. dedup() because a device that errored
        // on read after being in `ready` ends up in both lists.
        poll_result.dead.sort_unstable();
        poll_result.dead.dedup();
        for &i in poll_result.dead.iter().rev() {
            let (path, device) = self.devices.remove(i);
            eprintln!(
                "EventDevice '{}' ({}) hung up; dropping from listener.",
                device.name().unwrap_or("Unknown"),
                path.display()
            );
        }

        poll_result.dead.len()
    }

    fn contains_device_path(&self, path: &PathBuf) -> bool {
        for (p, _) in &self.devices {
            if p == path {
                return true;
            }
        }

        false
    }

    fn is_keyboard(device: &Device) -> bool {
        device.supported_keys().is_some_and(|keys| {
            keys.contains(Key::KEY_TAB)
                || keys.contains(Key::KEY_LEFTSHIFT)
                || keys.contains(Key::KEY_Z)
        })
    }

    fn is_mouse(device: &Device) -> bool {
        device
            .supported_keys()
            .is_some_and(|keys| keys.contains(Key::BTN_SIDE) || keys.contains(Key::BTN_EXTRA))
    }
}

/// Every `/dev/input/event*` node, sorted **numerically** by the
/// `eventN` suffix. Numeric order keeps startup logs and any first-match
/// consumers predictable.
fn event_nodes() -> Result<Vec<PathBuf>> {
    let mut nodes: Vec<PathBuf> = std::fs::read_dir(INPUT_DIR)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| event_index(p).is_some())
        .collect();
    nodes.sort_by_key(|p| event_index(p));
    Ok(nodes)
}

/// `N` from a `/dev/input/eventN` path, or `None` for anything else
fn event_index(path: &Path) -> Option<u32> {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|s| s.strip_prefix("event"))
        .and_then(|n| n.parse().ok())
}

struct PollReadyDeadResult {
    timedout: bool,
    ready: Vec<usize>,
    dead: Vec<usize>,
}

fn poll_ready_or_dead<'a>(
    devices: impl IntoIterator<Item = &'a Device>,
) -> Result<PollReadyDeadResult> {
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
        .into_iter()
        .map(|d| {
            let raw = d.as_raw_fd();
            let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
            PollFd::new(borrowed, PollFlags::POLLIN)
        })
        .collect();

    let n = poll(&mut pollfds, PollTimeout::from(POLL_TIMEOUT_MS))?;
    if n == 0 {
        return Ok(PollReadyDeadResult {
            timedout: true,
            ready: vec![],
            dead: vec![],
        });
    }

    // Note which indices are ready vs. hung up.
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

    Ok(PollReadyDeadResult {
        timedout: false,
        ready,
        dead,
    })
}
