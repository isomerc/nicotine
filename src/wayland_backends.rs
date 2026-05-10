use crate::config::Config;
use crate::window_manager::{EveWindow, WindowManager};
use crate::xdg_activation::XdgActivation;
use anyhow::{Context, Result};
use serde_json::Value;
use std::process::Command;
use std::sync::Arc;
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as _, EventMask, InputFocus,
    PropMode, CLIENT_MESSAGE_EVENT,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

// ============================================================================
// KDE Plasma / KWin Backend
// ============================================================================
//
// Window enumeration + window-state queries go through `wmctrl` (it's the
// shortest path to a stable list of EWMH-aware X11 windows under XWayland
// on KDE). Window *activation* uses an EWMH _NET_ACTIVE_WINDOW client
// message sent directly via x11rb — KWin reliably honors that for
// XWayland clients (EVE in Wine/Proton is one), whereas `wmctrl -i -a`
// silently succeeds without actually foregrounding the target on KDE
// Plasma Wayland. Same activation primitive `X11Manager` uses, which is
// why click-to-activate from the Linux preview manager has always
// worked while side-button cycling did not.

pub struct KWinManager {
    /// Auxiliary X11 connection used only for the EWMH activation
    /// ClientMessage path. wmctrl/xdotool/swaymsg-style shelling stays
    /// elsewhere for compatibility; this is one extra socket but it
    /// removes a class of "activate succeeded but nothing happened"
    /// bugs that only show up under KDE Plasma Wayland.
    conn: Arc<RustConnection>,
    screen_num: usize,
    net_active_window_atom: u32,
    /// `_NET_STARTUP_ID` — the X11-side carrier for a Wayland
    /// xdg-activation token. Set on the target EVE window before
    /// sending the EWMH activate ClientMessage so KWin can promote the
    /// activation to Wayland surface focus, not just X11 focus.
    net_startup_id_atom: u32,
    /// Wayland-side companion that mints activation tokens. `None`
    /// means we couldn't set it up — either we're not on a Wayland
    /// session, or the compositor doesn't advertise xdg_activation_v1.
    /// activate_window falls back to the token-less EWMH path in that
    /// case, which matches the pre-token behavior.
    xdg_activation: Option<XdgActivation>,
}

impl KWinManager {
    pub fn new() -> Result<Self> {
        Command::new("wmctrl")
            .arg("-m")
            .output()
            .context("wmctrl not found. Install wmctrl package")?;

        let (conn, screen_num) =
            x11rb::connect(None).context("X11 connect failed (XWayland not running?)")?;
        let conn = Arc::new(conn);
        let net_active_window_atom = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let net_startup_id_atom = conn.intern_atom(false, b"_NET_STARTUP_ID")?.reply()?.atom;

        // Best-effort: open the Wayland connection for xdg-activation
        // tokens. Failures here are expected on X11-only sessions and on
        // compositors that don't advertise xdg_activation_v1; activate
        // still works without tokens, just without the Wayland focus
        // bridge that fixes the "first click is consumed" symptom.
        let xdg_activation = XdgActivation::new()
            .map_err(|e| {
                eprintln!(
                    "xdg-activation unavailable ({e:?}); cycle activation will use the \
                     token-less EWMH path. Subsequent clicks on the newly-active EVE \
                     client may need a second click to register on KWin Wayland."
                );
            })
            .ok();

        Ok(Self {
            conn,
            screen_num,
            net_active_window_atom,
            net_startup_id_atom,
            xdg_activation,
        })
    }

    /// Run `wmctrl -lp` and return `(window_id_hex, pid, title)` triples.
    /// `-lp` adds a pid column we need to process-filter EVE clients
    /// (see eve_match::pid_is_eve_client). Columns: `<id> <desktop>
    /// <pid> <host> <title>`.
    fn get_all_windows(&self) -> Result<Vec<(String, u32, String)>> {
        let output = Command::new("wmctrl")
            .args(["-l", "-p"])
            .output()
            .context("Failed to execute wmctrl")?;

        if !output.status.success() {
            anyhow::bail!("wmctrl failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let mut windows = Vec::new();
        let lines = String::from_utf8_lossy(&output.stdout);

        for line in lines.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // wmctrl -lp columns: <id> <desktop> <pid> <host> <title…>.
            // In rare cases (containers / sandboxed sessions where
            // HOSTNAME is empty) the host token collapses out under
            // split_whitespace and we see only 4 columns with the
            // title starting at parts[3]. Accept both layouts.
            if parts.len() < 4 {
                continue;
            }
            let window_id = parts[0];
            let pid: u32 = parts[2].parse().unwrap_or(0);
            let title_start = if parts.len() >= 5 { 4 } else { 3 };
            let title = parts[title_start..].join(" ");
            windows.push((window_id.to_string(), pid, title));
        }

        Ok(windows)
    }

    // `get_window_title_by_id` was used by the old kdotool fallback in
    // activate_window. The EWMH ClientMessage path doesn't need it, but
    // keeping it dead-allowed in case a future restore-from-iconic
    // codepath needs to look up titles for X11 calls that take names
    // rather than ids.
    #[allow(dead_code)]
    fn get_window_title_by_id(&self, hex_id: &str) -> Option<String> {
        let output = Command::new("wmctrl").args(["-l", "-p"]).output().ok()?;
        if !output.status.success() {
            return None;
        }

        let lines = String::from_utf8_lossy(&output.stdout);
        for line in lines.lines() {
            if line.starts_with(hex_id) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                // `wmctrl -lp` columns: id desktop pid host title…
                // Empty-host fallback to parts[3..]; see get_all_windows.
                if parts.len() < 4 {
                    return None;
                }
                let title_start = if parts.len() >= 5 { 4 } else { 3 };
                return Some(parts[title_start..].join(" "));
            }
        }
        None
    }
}

impl WindowManager for KWinManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let windows = self.get_all_windows()?;
        let mut eve_windows = Vec::new();

        for (id_str, pid, title) in windows {
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process filter: reject anything titled "EVE - …" whose
            // owning process isn't the actual game (`exefile.exe`).
            // Browser tabs / Discord / etc. all get filtered here.
            if pid == 0 || !crate::eve_match::pid_is_eve_client(pid) {
                continue;
            }
            // Parse hex window ID (e.g., "0x06e00008") to u32.
            let id = if let Some(hex) = id_str.strip_prefix("0x") {
                u32::from_str_radix(hex, 16).unwrap_or(0)
            } else {
                id_str.parse::<u32>().unwrap_or(0)
            };
            if id == 0 {
                continue;
            }
            eve_windows.push(EveWindow {
                id,
                title: title.trim_start_matches("EVE - ").to_string(),
            });
        }

        Ok(eve_windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        // xdg-activation bridge: if we have a Wayland connection, mint a
        // fresh token and stamp it on the target's `_NET_STARTUP_ID`
        // X11 property. KWin reads this property when handling the
        // _NET_ACTIVE_WINDOW ClientMessage below, and uses the token to
        // authorize Wayland surface focus — not just X11 focus —
        // transferring to the target. Without the token, X11 focus moves
        // but the next click on EVE is consumed by KWin's click-to-focus
        // logic at the Wayland layer instead of being delivered to EVE.
        //
        // Token-mint failures (timeout, compositor declines, connection
        // dropped) are non-fatal — we keep going through the existing
        // EWMH path, matching the pre-token behavior.
        if let Some(activation) = &self.xdg_activation {
            match activation.request_token() {
                Ok(token) => {
                    let _ = self.conn.change_property8(
                        PropMode::REPLACE,
                        window_id,
                        self.net_startup_id_atom,
                        AtomEnum::STRING,
                        token.as_bytes(),
                    );
                }
                Err(e) => {
                    eprintln!("xdg_activation token request failed: {e:?}");
                }
            }
        }

        // Send the EWMH activation message directly. wmctrl -i -a
        // silently no-ops under KDE Wayland for XWayland clients; the
        // ClientMessage path is what KWin actually honors.
        let screen = &self.conn.setup().roots[self.screen_num];
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: window_id,
            type_: self.net_active_window_atom,
            // EWMH source: 2 = pager (avoids the focus-stealing-
            // prevention penalty KWin applies to source 1 = application).
            data: ClientMessageData::from([2, x11rb::CURRENT_TIME, 0, 0, 0]),
        };
        self.conn.send_event(
            false,
            screen.root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;
        // Belt-and-suspenders SetInputFocus for compositors that don't
        // act on the ClientMessage alone. KWin alone is usually fine
        // but a few KDE setups under strict focus-stealing-prevention
        // need the explicit focus request.
        let _ = self
            .conn
            .set_input_focus(InputFocus::PARENT, window_id, x11rb::CURRENT_TIME);
        self.conn.flush()?;
        Ok(())
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width;
        let height = config.display_height - config.panel_height;

        for window in windows {
            // Convert u32 to hex format for wmctrl
            let hex_id = format!("0x{:08x}", window.id);

            // Move and resize window using wmctrl
            Command::new("wmctrl")
                .arg("-i")
                .arg("-r")
                .arg(&hex_id)
                .arg("-e")
                .arg(format!("0,{},{},{},{}", x, y, width, height))
                .output()?;
        }

        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        // Use xdotool to get active window (works through XWayland)
        let output = Command::new("xdotool")
            .arg("getactivewindow")
            .output()
            .context("Failed to get active window")?;

        let window_id = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse::<u32>()
            .context("Failed to parse active window ID")?;

        Ok(window_id)
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        let hex_id = format!("0x{:08x}", window_id);
        Command::new("xdotool")
            .args(["windowminimize", &hex_id])
            .output()
            .context("Failed to minimize window")?;
        Ok(())
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        let hex_id = format!("0x{:08x}", window_id);
        // wmctrl -i -a activates and restores from minimized state
        Command::new("wmctrl")
            .args(["-i", "-a", &hex_id])
            .output()
            .context("Failed to restore window")?;
        Ok(())
    }
}

// ============================================================================
// Sway Backend (via swaymsg)
// ============================================================================

pub struct SwayManager;

impl SwayManager {
    pub fn new() -> Result<Self> {
        // Verify swaymsg is available
        Command::new("swaymsg")
            .arg("--version")
            .output()
            .context("swaymsg not found. Make sure you're running Sway")?;

        Ok(Self)
    }

    fn get_all_windows(&self) -> Result<Vec<Value>> {
        let output = Command::new("swaymsg")
            .arg("-t")
            .arg("get_tree")
            .output()
            .context("Failed to execute swaymsg")?;

        if !output.status.success() {
            anyhow::bail!(
                "swaymsg failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let tree: Value =
            serde_json::from_slice(&output.stdout).context("Failed to parse swaymsg output")?;

        let mut windows = Vec::new();
        Self::extract_windows(&tree, &mut windows);

        Ok(windows)
    }

    fn extract_windows(node: &Value, windows: &mut Vec<Value>) {
        if let Some(node_type) = node.get("type").and_then(|t| t.as_str()) {
            if node_type == "con" || node_type == "floating_con" {
                if let Some(app_id) = node.get("app_id") {
                    if !app_id.is_null() {
                        windows.push(node.clone());
                    }
                } else if let Some(window_properties) = node.get("window_properties") {
                    if !window_properties.is_null() {
                        windows.push(node.clone());
                    }
                }
            }
        }

        if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
            for child in nodes {
                Self::extract_windows(child, windows);
            }
        }

        if let Some(floating_nodes) = node.get("floating_nodes").and_then(|n| n.as_array()) {
            for child in floating_nodes {
                Self::extract_windows(child, windows);
            }
        }
    }

    fn get_window_title(window: &Value) -> Option<String> {
        window
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
    }

    fn get_window_id(window: &Value) -> Option<u32> {
        window.get("id").and_then(|i| i.as_u64()).map(|i| i as u32)
    }

    /// Sway's IPC tree exposes the X11/XWayland or native pid on each
    /// container node. Used to filter EVE windows by process identity
    /// rather than title alone — see eve_match::pid_is_eve_client.
    fn get_window_pid(window: &Value) -> Option<u32> {
        window.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32)
    }
}

impl WindowManager for SwayManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let windows = self.get_all_windows()?;
        let mut eve_windows = Vec::new();

        for window in windows {
            let Some(title) = Self::get_window_title(&window) else {
                continue;
            };
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process gate before title-only acceptance — see KWin
            // backend above for the rationale.
            let Some(pid) = Self::get_window_pid(&window) else {
                continue;
            };
            if !crate::eve_match::pid_is_eve_client(pid) {
                continue;
            }
            let Some(id) = Self::get_window_id(&window) else {
                continue;
            };
            eve_windows.push(EveWindow {
                id,
                title: title.trim_start_matches("EVE - ").to_string(),
            });
        }

        Ok(eve_windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        let output = Command::new("swaymsg")
            .arg(format!("[con_id={}] focus", window_id))
            .output()
            .context("Failed to activate window")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to activate window: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width as i32;
        let height = (config.display_height - config.panel_height) as i32;

        for window in windows {
            // Sway uses floating mode for positioning
            Command::new("swaymsg")
                .arg(format!("[con_id={}] floating enable", window.id))
                .output()?;

            Command::new("swaymsg")
                .arg(format!("[con_id={}] move position {} {}", window.id, x, y))
                .output()?;

            Command::new("swaymsg")
                .arg(format!(
                    "[con_id={}] resize set {} {}",
                    window.id, width, height
                ))
                .output()?;
        }

        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        let windows = self.get_all_windows()?;

        for window in windows {
            if let Some(focused) = window.get("focused").and_then(|f| f.as_bool()) {
                if focused {
                    if let Some(id) = Self::get_window_id(&window) {
                        return Ok(id);
                    }
                }
            }
        }

        anyhow::bail!("No active window found")
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        Command::new("swaymsg")
            .arg(format!("[con_id={}] move scratchpad", window_id))
            .output()
            .context("Failed to minimize window")?;
        Ok(())
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        // Show from scratchpad restores it
        Command::new("swaymsg")
            .arg(format!("[con_id={}] scratchpad show", window_id))
            .output()
            .context("Failed to restore window")?;
        Ok(())
    }
}

// ============================================================================
// Hyprland Backend (via hyprctl)
// ============================================================================

pub struct HyprlandManager;

impl HyprlandManager {
    pub fn new() -> Result<Self> {
        // Verify hyprctl is available
        Command::new("hyprctl")
            .arg("version")
            .output()
            .context("hyprctl not found. Make sure you're running Hyprland")?;

        Ok(Self)
    }

    fn get_all_windows(&self) -> Result<Vec<Value>> {
        let output = Command::new("hyprctl")
            .arg("clients")
            .arg("-j")
            .output()
            .context("Failed to execute hyprctl")?;

        if !output.status.success() {
            anyhow::bail!(
                "hyprctl failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let windows: Vec<Value> =
            serde_json::from_slice(&output.stdout).context("Failed to parse hyprctl output")?;

        Ok(windows)
    }
}

impl WindowManager for HyprlandManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let windows = self.get_all_windows()?;
        let mut eve_windows = Vec::new();

        for window in windows {
            let Some(title) = window.get("title").and_then(|t| t.as_str()) else {
                continue;
            };
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process gate. Hyprland's IPC reports each client's pid
            // via the "pid" field; verify it's the actual EVE client
            // (`exefile.exe`) before accepting.
            let pid = window.get("pid").and_then(|p| p.as_u64()).unwrap_or(0) as u32;
            if pid == 0 || !crate::eve_match::pid_is_eve_client(pid) {
                continue;
            }
            if let Some(address) = window.get("address").and_then(|a| a.as_str()) {
                // Hyprland addresses are hex; lossy-narrow to u32 for
                // the cross-platform EveWindow.id we use elsewhere.
                let id = if let Some(hex) = address.strip_prefix("0x") {
                    u32::from_str_radix(hex, 16).unwrap_or(0)
                } else {
                    0
                };
                eve_windows.push(EveWindow {
                    id,
                    title: title.trim_start_matches("EVE - ").to_string(),
                });
            }
        }

        Ok(eve_windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        // Convert u32 back to hex address
        let address = format!("0x{:x}", window_id);

        let output = Command::new("hyprctl")
            .arg("dispatch")
            .arg("focuswindow")
            .arg(format!("address:{}", address))
            .output()
            .context("Failed to activate window")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to activate window: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width as i32;
        let height = (config.display_height - config.panel_height) as i32;

        for window in windows {
            let address = format!("0x{:x}", window.id);

            // Enable floating
            Command::new("hyprctl")
                .arg("dispatch")
                .arg("togglefloating")
                .arg(format!("address:{}", address))
                .output()?;

            // Move window
            Command::new("hyprctl")
                .arg("dispatch")
                .arg("movewindowpixel")
                .arg(format!("exact {} {},address:{}", x, y, address))
                .output()?;

            // Resize window
            Command::new("hyprctl")
                .arg("dispatch")
                .arg("resizewindowpixel")
                .arg(format!("exact {} {},address:{}", width, height, address))
                .output()?;
        }

        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        let output = Command::new("hyprctl")
            .arg("activewindow")
            .arg("-j")
            .output()
            .context("Failed to get active window")?;

        let window: Value =
            serde_json::from_slice(&output.stdout).context("Failed to parse hyprctl output")?;

        if let Some(address) = window.get("address").and_then(|a| a.as_str()) {
            let id = if let Some(hex) = address.strip_prefix("0x") {
                u32::from_str_radix(hex, 16).unwrap_or(0)
            } else {
                0
            };
            return Ok(id);
        }

        anyhow::bail!("Failed to get active window ID")
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        let address = format!("0x{:x}", window_id);
        Command::new("hyprctl")
            .args([
                "dispatch",
                "movetoworkspacesilent",
                &format!("special,address:{}", address),
            ])
            .output()
            .context("Failed to minimize window")?;
        Ok(())
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        let address = format!("0x{:x}", window_id);
        // Move back to current workspace
        Command::new("hyprctl")
            .args([
                "dispatch",
                "movetoworkspace",
                &format!("e+0,address:{}", address),
            ])
            .output()
            .context("Failed to restore window")?;
        Ok(())
    }
}
