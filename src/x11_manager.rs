use crate::config::Config;
use crate::eve_match::pid_is_eve_client;
use crate::window_manager::{EveWindow, WindowId, WindowManager};
use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;

/// Latches once we've warned the user that an EVE-titled window had
/// no `_NET_WM_PID`. The window-enumeration loop runs every 500ms
/// from the daemon's background thread, so a per-call eprintln would
/// be log spam; a single breadcrumb is enough to diagnose a future
/// XWayland / launcher regression that stops setting the property.
static WARNED_MISSING_PID: AtomicBool = AtomicBool::new(false);

pub struct X11Manager {
    conn: Arc<RustConnection>,
    screen_num: usize,
    net_active_window_atom: Atom,
    net_wm_pid_atom: Atom,
}

impl X11Manager {
    fn x11_id(window_id: WindowId) -> Result<u32> {
        u32::try_from(window_id).context("Window ID exceeds the X11 32-bit range")
    }

    pub fn new() -> Result<Self> {
        let (conn, screen_num) =
            RustConnection::connect(None).context("Failed to connect to X11 server")?;

        let conn = Arc::new(conn);

        // Pre-cache the _NET_ACTIVE_WINDOW atom (do roundtrip once at startup)
        let net_active_window_atom = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        // Used to filter the EVE-window enumeration by process identity
        // (`exefile.exe`) so we don't cycle into browser tabs or other
        // apps that happen to be titled "EVE - …".
        let net_wm_pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;

        Ok(Self {
            conn,
            screen_num,
            net_active_window_atom,
            net_wm_pid_atom,
        })
    }

    /// Read _NET_WM_PID from a window. Returns None for missing /
    /// malformed / zero — treated as "we don't know what owns this
    /// window" and rejected by the EVE filter.
    fn read_net_wm_pid(&self, window: u32) -> Option<u32> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.net_wm_pid_atom,
                AtomEnum::CARDINAL,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        let pid = reply.value32()?.next()?;
        if pid == 0 {
            None
        } else {
            Some(pid)
        }
    }

    pub fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let screen = &self.conn.setup().roots[self.screen_num];
        let root = screen.root;

        // Get _NET_CLIENT_LIST atom
        let net_client_list = self
            .conn
            .intern_atom(false, b"_NET_CLIENT_LIST")?
            .reply()?
            .atom;

        // Get list of all windows
        let client_list_reply = self
            .conn
            .get_property(false, root, net_client_list, AtomEnum::WINDOW, 0, u32::MAX)?
            .reply()?;

        let windows: Vec<u32> = client_list_reply
            .value32()
            .ok_or_else(|| anyhow::anyhow!("Failed to get window list"))?
            .collect();

        let mut eve_windows = Vec::new();

        for &window in &windows {
            let Ok(title) = self.get_window_title(window) else {
                continue;
            };
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Reliable filter: confirm the underlying process is the
            // actual EVE client (`exefile.exe`). Without this we'd
            // also match browser tabs and other "EVE - …" titled
            // windows. See src/eve_match.rs.
            let Some(pid) = self.read_net_wm_pid(window) else {
                if !WARNED_MISSING_PID.swap(true, Ordering::Relaxed) {
                    eprintln!(
                        "Warning: window titled '{}' has no _NET_WM_PID; \
                         skipping EVE-client process check. If EVE clients \
                         are missing from cycling, verify your compositor \
                         is setting _NET_WM_PID on XWayland clients.",
                        title
                    );
                }
                continue;
            };
            if !pid_is_eve_client(pid) {
                continue;
            }
            eve_windows.push(EveWindow {
                id: window.into(),
                title: title.trim_start_matches("EVE - ").to_string(),
            });
        }

        Ok(eve_windows)
    }

    pub fn get_active_window(&self) -> Result<u32> {
        let screen = &self.conn.setup().roots[self.screen_num];
        let root = screen.root;

        let net_active_window = self
            .conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;

        let reply = self
            .conn
            .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)?
            .reply()?;

        let active: Vec<u32> = reply
            .value32()
            .ok_or_else(|| anyhow::anyhow!("Failed to get active window"))?
            .collect();

        Ok(*active.first().unwrap_or(&0))
    }

    pub fn activate_window(&self, window_id: u32) -> Result<()> {
        let screen = &self.conn.setup().roots[self.screen_num];
        let root = screen.root;

        let current_active = self.get_active_window().unwrap_or(0);

        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: window_id,
            type_: self.net_active_window_atom,
            data: ClientMessageData::from([2, x11rb::CURRENT_TIME, current_active, 0, 0]),
        };

        self.conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;

        self.conn
            .set_input_focus(InputFocus::PARENT, window_id, x11rb::CURRENT_TIME)?;

        self.conn.flush()?;
        Ok(())
    }

    pub fn stack_windows_internal(
        &self,
        windows: &[EveWindow],
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) -> Result<()> {
        for window in windows {
            let window_id = Self::x11_id(window.id)?;
            // Move and resize window
            let values = ConfigureWindowAux::new()
                .x(x)
                .y(y)
                .width(width)
                .height(height);

            self.conn.configure_window(window_id, &values)?;
        }

        self.conn.flush()?;
        Ok(())
    }

    fn get_window_title(&self, window: u32) -> Result<String> {
        // Try _NET_WM_NAME first (UTF-8)
        let net_wm_name = self.conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;

        let utf8_string = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;

        if let Ok(reply) = self
            .conn
            .get_property(false, window, net_wm_name, utf8_string, 0, 1024)?
            .reply()
        {
            if !reply.value.is_empty() {
                if let Ok(title) = String::from_utf8(reply.value.clone()) {
                    return Ok(title);
                }
            }
        }

        // Fall back to WM_NAME
        if let Ok(reply) = self
            .conn
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)?
            .reply()
        {
            if !reply.value.is_empty() {
                return Ok(String::from_utf8_lossy(&reply.value).to_string());
            }
        }

        Ok(String::new())
    }

    pub fn minimize_window(&self, window_id: u32) -> Result<()> {
        // Use WM_CHANGE_STATE with IconicState to minimize
        let wm_change_state = self
            .conn
            .intern_atom(false, b"WM_CHANGE_STATE")?
            .reply()?
            .atom;

        let screen = &self.conn.setup().roots[self.screen_num];
        let root = screen.root;

        // IconicState = 3
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: window_id,
            type_: wm_change_state,
            data: ClientMessageData::from([3u32, 0, 0, 0, 0]),
        };

        self.conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;

        self.conn.flush()?;
        Ok(())
    }

    pub fn restore_window(&self, window_id: u32) -> Result<()> {
        // Map the window to restore it from minimized state
        self.conn.map_window(window_id)?;
        self.conn.flush()?;
        Ok(())
    }
}

impl WindowManager for X11Manager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        self.get_eve_windows()
    }

    fn activate_window(&self, window_id: WindowId) -> Result<()> {
        self.activate_window(Self::x11_id(window_id)?)
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width;
        let height = config.display_height - config.panel_height;

        self.stack_windows_internal(windows, x, y, width, height)
    }

    fn get_active_window(&self) -> Result<WindowId> {
        self.get_active_window().map(Into::into)
    }

    fn minimize_window(&self, window_id: WindowId) -> Result<()> {
        self.minimize_window(Self::x11_id(window_id)?)
    }

    fn restore_window(&self, window_id: WindowId) -> Result<()> {
        self.restore_window(Self::x11_id(window_id)?)
    }
}
