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
// Shared XWayland / EWMH helpers (KWin + GNOME)
// ============================================================================
//
// Both the KWin and GNOME backends run EVE as XWayland clients and drive
// focus through an EWMH `_NET_ACTIVE_WINDOW` ClientMessage rather than a
// Wayland-native protocol. The activation primitive (xdg-activation token
// stamped on `_NET_STARTUP_ID`, then a pager-sourced activate message) and
// the active-window read are byte-for-byte identical across the two
// compositors, so they live here instead of being duplicated per backend.

/// Mint an xdg-activation token (best-effort) and send the EWMH activate
/// ClientMessage for `window_id`. The token, stamped on `_NET_STARTUP_ID`,
/// lets the compositor promote X11 focus to Wayland surface focus so the
/// next click reaches EVE; source = 2 (pager) dodges the focus-stealing
/// penalty applied to source = 1 (application). Token-mint failures are
/// non-fatal — we fall through to the token-less EWMH path.
fn ewmh_activate(
    conn: &RustConnection,
    screen_num: usize,
    net_active_window_atom: u32,
    net_startup_id_atom: u32,
    xdg_activation: &Option<XdgActivation>,
    window_id: u32,
) -> Result<()> {
    if let Some(activation) = xdg_activation {
        match activation.request_token() {
            Ok(token) => {
                let _ = conn.change_property8(
                    PropMode::REPLACE,
                    window_id,
                    net_startup_id_atom,
                    AtomEnum::STRING,
                    token.as_bytes(),
                );
            }
            Err(e) => {
                eprintln!("xdg_activation token request failed: {e:?}");
            }
        }
    }

    let screen = &conn.setup().roots[screen_num];
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: window_id,
        type_: net_active_window_atom,
        data: ClientMessageData::from([2, x11rb::CURRENT_TIME, 0, 0, 0]),
    };
    conn.send_event(
        false,
        screen.root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event,
    )?;
    // Belt-and-suspenders explicit focus for compositors that don't act on
    // the ClientMessage alone under strict focus-stealing prevention.
    let _ = conn.set_input_focus(InputFocus::PARENT, window_id, x11rb::CURRENT_TIME);
    conn.flush()?;
    Ok(())
}

/// Read `_NET_ACTIVE_WINDOW` off the root and return the focused window id
/// (0 if unset / no X11 window focused).
fn ewmh_active_window(
    conn: &RustConnection,
    screen_num: usize,
    net_active_window_atom: u32,
) -> Result<u32> {
    let root = conn.setup().roots[screen_num].root;
    let reply = conn
        .get_property(false, root, net_active_window_atom, AtomEnum::WINDOW, 0, 1)
        .context("get_property _NET_ACTIVE_WINDOW")?
        .reply()
        .context("get_property reply")?;
    Ok(reply.value32().and_then(|mut v| v.next()).unwrap_or(0))
}

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
        // Shared XWayland/EWMH activation. `wmctrl -i -a` silently no-ops
        // under KDE Wayland for XWayland clients, so we send the
        // `_NET_ACTIVE_WINDOW` ClientMessage ourselves (with the
        // xdg-activation token bridge); see `ewmh_activate`.
        ewmh_activate(
            &self.conn,
            self.screen_num,
            self.net_active_window_atom,
            self.net_startup_id_atom,
            &self.xdg_activation,
            window_id,
        )
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
        // Read _NET_ACTIVE_WINDOW directly via x11rb under XWayland —
        // matches what X11Manager does and avoids an `xdotool` runtime dep.
        ewmh_active_window(&self.conn, self.screen_num, self.net_active_window_atom)
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

// ============================================================================
// GNOME / Mutter Backend (XWayland + EWMH, no restack)
// ============================================================================
//
// GNOME Shell on Wayland exposes no client-facing window-management IPC and
// deliberately implements no wlroots control protocol, so — like every other
// backend — we drive EVE (which runs as XWayland X11 windows under Proton)
// through the X server directly. Enumeration reads `_NET_CLIENT_LIST` and
// filters by the EVE title + process identity; activation reuses the shared
// EWMH path (xdg-activation token + pager-sourced `_NET_ACTIVE_WINDOW`),
// exactly as KWin does.
//
// We deliberately go straight to x11rb rather than shelling out to
// wmctrl/xdotool the way the KWin backend does for some calls: under Mutter's
// XWayland several EWMH conveniences are only partly wired up (e.g. `wmctrl
// -m`'s supporting-WM check fails), so the direct property reads are the
// reliable path — the same approach other Linux-native EVE tools take.
//
// `stack_windows` is a no-op: Wayland forbids a client positioning another
// window and there is no extension-free way around it on Mutter. The config
// panel hides the Restack button on this session (see
// `window_manager::restack_supported`).

pub struct GnomeManager {
    conn: Arc<RustConnection>,
    screen_num: usize,
    net_active_window_atom: u32,
    net_startup_id_atom: u32,
    net_client_list_atom: u32,
    net_wm_pid_atom: u32,
    net_wm_name_atom: u32,
    utf8_string_atom: u32,
    wm_change_state_atom: u32,
    /// Wayland-side token minter for the activation focus bridge. `None`
    /// when xdg_activation_v1 isn't available; activation then uses the
    /// token-less EWMH path. Mutter advertises xdg_activation_v1, so this
    /// is normally `Some`.
    xdg_activation: Option<XdgActivation>,
}

impl GnomeManager {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .context("X11 connect failed — is XWayland running under GNOME?")?;
        let conn = Arc::new(conn);

        let net_active_window_atom = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let net_startup_id_atom = conn.intern_atom(false, b"_NET_STARTUP_ID")?.reply()?.atom;
        let net_client_list_atom = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
        let net_wm_pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
        let net_wm_name_atom = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let wm_change_state_atom = conn.intern_atom(false, b"WM_CHANGE_STATE")?.reply()?.atom;

        // Best-effort: open the Wayland connection for xdg-activation tokens.
        // Same focus-bridge rationale as the KWin backend.
        let xdg_activation = XdgActivation::new()
            .map_err(|e| {
                eprintln!(
                    "xdg-activation unavailable ({e:?}); cycle activation will use the \
                     token-less EWMH path. The first click on a newly-active EVE client \
                     may need a second click on Mutter."
                );
            })
            .ok();

        Ok(Self {
            conn,
            screen_num,
            net_active_window_atom,
            net_startup_id_atom,
            net_client_list_atom,
            net_wm_pid_atom,
            net_wm_name_atom,
            utf8_string_atom,
            wm_change_state_atom,
            xdg_activation,
        })
    }

    /// Read a window's title, preferring `_NET_WM_NAME` (UTF-8) and falling
    /// back to the legacy `WM_NAME` (Latin-1). EVE sets the former.
    fn window_title(&self, window: u32) -> Option<String> {
        if let Some(reply) = self
            .conn
            .get_property(
                false,
                window,
                self.net_wm_name_atom,
                self.utf8_string_atom,
                0,
                u32::MAX,
            )
            .ok()
            .and_then(|c| c.reply().ok())
        {
            if !reply.value.is_empty() {
                if let Ok(s) = String::from_utf8(reply.value) {
                    return Some(s);
                }
            }
        }
        if let Some(reply) = self
            .conn
            .get_property(
                false,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                0,
                u32::MAX,
            )
            .ok()
            .and_then(|c| c.reply().ok())
        {
            if !reply.value.is_empty() {
                return Some(String::from_utf8_lossy(&reply.value).into_owned());
            }
        }
        None
    }

    /// Read a window's owning PID from `_NET_WM_PID` (0 if unset).
    fn window_pid(&self, window: u32) -> u32 {
        self.conn
            .get_property(
                false,
                window,
                self.net_wm_pid_atom,
                AtomEnum::CARDINAL,
                0,
                1,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .unwrap_or(0)
    }
}

impl WindowManager for GnomeManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let root = self.conn.setup().roots[self.screen_num].root;
        let reply = self
            .conn
            .get_property(
                false,
                root,
                self.net_client_list_atom,
                AtomEnum::WINDOW,
                0,
                u32::MAX,
            )
            .context("get_property _NET_CLIENT_LIST")?
            .reply()
            .context("_NET_CLIENT_LIST reply")?;
        let ids: Vec<u32> = reply.value32().map(|it| it.collect()).unwrap_or_default();

        let mut eve_windows = Vec::new();
        for id in ids {
            let Some(title) = self.window_title(id) else {
                continue;
            };
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process gate: reject anything titled "EVE - …" whose owning
            // process isn't the actual game (`exefile.exe`) — browser tabs,
            // Discord channels, etc. Matches the other backends.
            let pid = self.window_pid(id);
            if pid == 0 || !crate::eve_match::pid_is_eve_client(pid) {
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
        ewmh_activate(
            &self.conn,
            self.screen_num,
            self.net_active_window_atom,
            self.net_startup_id_atom,
            &self.xdg_activation,
            window_id,
        )
    }

    fn stack_windows(&self, _windows: &[EveWindow], _config: &Config) -> Result<()> {
        // No-op on GNOME Wayland: Mutter implements no client-facing window
        // positioning, and there is no extension-free way to move another
        // window to absolute coordinates. The panel hides the Restack button
        // here; the CLI `stack` falls through to this and intentionally does
        // nothing rather than failing loudly.
        eprintln!(
            "Restack is unavailable on GNOME Wayland — the compositor does not allow \
             applications to position windows. Skipping."
        );
        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        ewmh_active_window(&self.conn, self.screen_num, self.net_active_window_atom)
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        // ICCCM iconify: send WM_CHANGE_STATE → IconicState (3) to the root.
        // This is exactly what XIconifyWindow does; Mutter honors it for X11
        // windows under XWayland.
        let root = self.conn.setup().roots[self.screen_num].root;
        let event = ClientMessageEvent {
            response_type: CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: window_id,
            type_: self.wm_change_state_atom,
            data: ClientMessageData::from([3, 0, 0, 0, 0]),
        };
        self.conn
            .send_event(
                false,
                root,
                EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )
            .context("send WM_CHANGE_STATE")?;
        self.conn.flush()?;
        Ok(())
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        // Activating an iconified window both un-minimizes and focuses it.
        self.activate_window(window_id)
    }
}
