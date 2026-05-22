//! Linux-specific fake-EVE harness. Creates real X11 top-level
//! windows whose titles match Nicotine's filter and whose
//! `_NET_WM_PID` points at forked children that have
//! `prctl(PR_SET_NAME, "exefile.exe")`'d themselves. This is enough
//! state to exercise the full enumeration + filtering path (Nicotine's
//! `get_eve_windows`, `pid_is_eve_client`) without launching the
//! actual game.
//!
//! Cleanup runs on `Drop`: child processes are killed and reaped;
//! X11 windows go away with the connection.

use evdev::{
    uinput::{VirtualDevice, VirtualDeviceBuilder},
    AttributeSet, EventType, InputEvent, Key,
};
use nix::libc;
use nix::sys::prctl;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag};
use nix::unistd::{fork, ForkResult, Pid};
use std::ffi::CStr;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const FAKE_COMM: &CStr = c"exefile.exe";

/// One fake EVE client: a forked child whose comm is `exefile.exe`,
/// paired with a real X11 window whose title is `EVE - <name>` and
/// whose `_NET_WM_PID` points at that child. Fields are public so
/// future tests can match on pid/window IDs; the existing tests only
/// read `name` indirectly via the harness so the unused-field warning
/// would fire without this allow.
#[allow(dead_code)]
pub struct FakeEveClient {
    pub name: String,
    pub pid: u32,
    pub window: u32,
}

/// Owns the fixture. Holding this alive holds the windows + processes
/// alive; dropping it tears both down.
pub struct FakeEveHarness {
    pub clients: Vec<FakeEveClient>,
    conn: RustConnection,
}

impl FakeEveHarness {
    /// Build a harness with one fake client per name. The names are
    /// what should appear after "EVE - " in window titles.
    pub fn new(names: &[&str]) -> anyhow::Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let net_wm_pid = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;

        let mut clients = Vec::with_capacity(names.len());
        for (idx, name) in names.iter().enumerate() {
            let pid = spawn_fake_eve_process()?;

            let window = conn.generate_id()?;
            // 320x200 + a solid background_pixel — KWin under
            // Wayland XWayland refuses to focus windows that don't
            // have a real surface to back them. Smaller windows
            // with no background_pixel got created and mapped but
            // never gave KWin a Wayland surface to assign focus to,
            // which broke the cycle / switch tests with no active
            // client visible post-activation. Offset each window so
            // they don't all overlap (helps the WM treat them as
            // independent toplevels).
            let aux = CreateWindowAux::new().background_pixel(screen.black_pixel);
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                (idx as i16) * 20,
                0,
                320,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &aux,
            )?;

            let title = format!("EVE - {}", name);
            conn.change_property(
                PropMode::REPLACE,
                window,
                net_wm_name,
                utf8_string,
                8,
                title.len() as u32,
                title.as_bytes(),
            )?;
            conn.change_property32(
                PropMode::REPLACE,
                window,
                net_wm_pid,
                AtomEnum::CARDINAL,
                &[pid],
            )?;
            conn.map_window(window)?;

            clients.push(FakeEveClient {
                name: name.to_string(),
                pid,
                window,
            });
        }

        conn.flush()?;
        std::thread::sleep(Duration::from_millis(250));

        Ok(Self { clients, conn })
    }

    /// Spawn an extra "lookalike" client whose title starts with
    /// `EVE - ` but whose backing process is NOT named `exefile.exe`.
    /// Verifies the process-name filter rejects non-EVE windows that
    /// happen to share the title prefix.
    pub fn add_lookalike(&mut self, title_suffix: &str) -> anyhow::Result<u32> {
        let pid = match unsafe { fork() }? {
            ForkResult::Parent { child } => child.as_raw() as u32,
            ForkResult::Child => {
                // SAFETY: only async-signal-safe calls below — pause + _exit.
                // _exit returns ! so the match arm has type !, coercing to
                // u32 for the parent's value.
                unsafe {
                    libc::pause();
                    libc::_exit(0)
                }
            }
        };

        let screen = &self.conn.setup().roots[0];
        let root = screen.root;
        let net_wm_name = self.conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let net_wm_pid = self.conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;

        let window = self.conn.generate_id()?;
        let aux = CreateWindowAux::new().background_pixel(screen.black_pixel);
        self.conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            320,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &aux,
        )?;
        let title = format!("EVE - {}", title_suffix);
        self.conn.change_property(
            PropMode::REPLACE,
            window,
            net_wm_name,
            utf8_string,
            8,
            title.len() as u32,
            title.as_bytes(),
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            net_wm_pid,
            AtomEnum::CARDINAL,
            &[pid],
        )?;
        self.conn.map_window(window)?;
        self.conn.flush()?;

        self.clients.push(FakeEveClient {
            name: format!("LOOKALIKE:{}", title_suffix),
            pid,
            window,
        });

        std::thread::sleep(Duration::from_millis(250));
        Ok(window)
    }
}

impl Drop for FakeEveHarness {
    fn drop(&mut self) {
        for c in &self.clients {
            let pid = Pid::from_raw(c.pid as i32);
            let _ = kill(pid, Signal::SIGTERM);
            // Reap with WNOHANG first; if the child hasn't exited
            // yet, SIGKILL it and reap again. Without the reap step
            // the child becomes a zombie and lingers until the test
            // binary exits, polluting later runs that look at
            // /proc/<pid>/comm.
            let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
            let _ = kill(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
        }
    }
}

/// Fork a child whose `/proc/<pid>/comm` reads as `exefile.exe`, and
/// which blocks forever (pause(2)) until killed by the test
/// teardown. Returns the child's PID.
fn spawn_fake_eve_process() -> anyhow::Result<u32> {
    match unsafe { fork() }? {
        ForkResult::Parent { child } => Ok(child.as_raw() as u32),
        ForkResult::Child => {
            // SAFETY: every call here is documented async-signal-safe.
            // prctl is on the safe list (signal-safety(7)); pause and
            // _exit are too. We intentionally do NOT call exit() or
            // any Drop machinery — that would run the parent's
            // destructors against shared state (libc allocator,
            // x11rb buffers) and could deadlock.
            let _ = prctl::set_name(FAKE_COMM);
            unsafe {
                libc::pause();
                libc::_exit(0)
            }
        }
    }
}

/// Activate one of the harness's fake EVE windows directly via X11,
/// bypassing nicotine. Used as the "set known starting point" step
/// before testing what `nicotine forward` does to focus. Returns
/// only after the WM has actually applied the change (polled with a
/// short timeout) so subsequent cycle commands see a deterministic
/// initial state.
pub fn activate_window_directly(window: u32) -> anyhow::Result<()> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let net_active = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window,
        type_: net_active,
        data: ClientMessageData::from([2u32, x11rb::CURRENT_TIME, 0, 0, 0]),
    };
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event,
    )?;
    conn.flush()?;
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let reply = conn
            .get_property(false, root, net_active, AtomEnum::WINDOW, 0, 1)?
            .reply()?;
        let active = reply.value32().and_then(|mut v| v.next()).unwrap_or(0);
        if active == window {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Read root-relative geometry of a window — its position + size as
/// the WM has placed it.
pub fn window_root_geometry(window: u32) -> anyhow::Result<(i32, i32, u32, u32)> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let geom = conn.get_geometry(window)?.reply()?;
    let translated = conn.translate_coordinates(window, root, 0, 0)?.reply()?;
    Ok((
        translated.dst_x as i32,
        translated.dst_y as i32,
        geom.width as u32,
        geom.height as u32,
    ))
}

/// Synthetic input device: registers as a real keyboard / mouse with
/// the kernel via uinput, so events emitted through it flow through
/// the standard /dev/input/eventN node the daemon's evdev listener
/// already polls. Tests use this to drive the
/// keyboard_listener / mouse_listener layer end-to-end without
/// needing a real keyboard or mouse on the runner.
///
/// On Linux the test runner needs read/write access to `/dev/uinput`.
/// Local NixOS sessions usually get an ACL granting the active user.
/// GitHub `ubuntu-latest` runners require a one-line `chmod` in the
/// CI step (`sudo chmod 666 /dev/uinput`) — applied in the workflow.
pub struct VirtualInput {
    dev: VirtualDevice,
    devnode: PathBuf,
}

impl VirtualInput {
    /// Build a virtual keyboard advertising the given key codes. The
    /// path returned by [`Self::devnode`] is what the test's
    /// daemon-config should put in `keyboard_device_path`.
    pub fn keyboard(keys: &[Key]) -> anyhow::Result<Self> {
        let mut keyset = AttributeSet::<Key>::new();
        for k in keys {
            keyset.insert(*k);
        }
        let mut dev = VirtualDeviceBuilder::new()?
            .name(b"nicotine-test-vkbd")
            .with_keys(&keyset)?
            .build()?;
        let devnode = Self::resolve_devnode(&mut dev)?;
        Ok(Self { dev, devnode })
    }

    /// Build a virtual mouse advertising the given button codes (e.g.
    /// `Key::BTN_SIDE`, `Key::BTN_EXTRA`). Same `devnode` convention
    /// as `keyboard` — feed it to `mouse_device_path` in the test
    /// config.
    pub fn mouse(buttons: &[Key]) -> anyhow::Result<Self> {
        let mut buttonset = AttributeSet::<Key>::new();
        for b in buttons {
            buttonset.insert(*b);
        }
        let mut dev = VirtualDeviceBuilder::new()?
            .name(b"nicotine-test-vmouse")
            .with_keys(&buttonset)?
            .build()?;
        let devnode = Self::resolve_devnode(&mut dev)?;
        Ok(Self { dev, devnode })
    }

    /// `/dev/input/eventN` for this device. Used by tests to populate
    /// the daemon's `keyboard_device_path` / `mouse_device_path`
    /// config so the listener attaches to THIS virtual device instead
    /// of auto-detecting whatever real hardware is plugged in.
    pub fn devnode(&self) -> &PathBuf {
        &self.devnode
    }

    /// Press then release a key. Emits two `EV_KEY` events + the
    /// trailing `EV_SYN` so the evdev consumer treats them as a
    /// single atomic press+release.
    pub fn tap(&mut self, key: Key) -> anyhow::Result<()> {
        self.dev.emit(&[
            InputEvent::new(EventType::KEY, key.code(), 1),
            InputEvent::new(EventType::KEY, key.code(), 0),
        ])?;
        Ok(())
    }

    /// Press `modifier`, tap `key`, release `modifier`. Used for
    /// Shift+F1-style hotkeys where the listener needs to see the
    /// modifier state held across the main key event.
    pub fn tap_with_modifier(&mut self, modifier: Key, key: Key) -> anyhow::Result<()> {
        self.dev.emit(&[
            InputEvent::new(EventType::KEY, modifier.code(), 1),
            InputEvent::new(EventType::KEY, key.code(), 1),
            InputEvent::new(EventType::KEY, key.code(), 0),
            InputEvent::new(EventType::KEY, modifier.code(), 0),
        ])?;
        Ok(())
    }

    /// Newly-created uinput devices don't publish their /dev/input
    /// node instantly — udev needs to process the kernel uevent and
    /// create the node. Poll the device's enumerated devnodes for up
    /// to a second; this is plenty even on a slow CI runner.
    fn resolve_devnode(dev: &mut VirtualDevice) -> anyhow::Result<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if let Ok(mut nodes) = dev.enumerate_dev_nodes_blocking() {
                if let Some(Ok(path)) = nodes.next() {
                    return Ok(path);
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        anyhow::bail!("virtual uinput device never published a /dev/input/eventN node within 2s")
    }
}
