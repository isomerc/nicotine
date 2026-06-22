use crate::config::Config;
use anyhow::Result;

#[derive(Debug, Clone)]
pub struct EveWindow {
    pub id: u32,
    pub title: String,
}

/// Trait for window management across different display servers and compositors
pub trait WindowManager: Send + Sync {
    /// Get all EVE Online client windows
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>>;

    /// Activate/focus a specific window by ID
    fn activate_window(&self, window_id: u32) -> Result<()>;

    /// Stack all EVE windows at the same position (centered)
    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()>;

    /// Get the currently active window ID
    fn get_active_window(&self) -> Result<u32>;

    /// Whether the currently-focused window belongs to an EVE client.
    ///
    /// Used to gate cycling/character hotkeys so they don't fire while some
    /// other application is focused and grab inputs meant for it. The
    /// default checks that the active window is one of our EVE windows
    /// (both queried from the same backend, so the ids are comparable).
    ///
    /// Fails *open* (returns true) if focus can't be determined, so cycling
    /// never silently dies on a transient query error. Nicotine's own
    /// overlay windows are no-activate / click-through and never hold focus,
    /// so "EVE focused" is the practical condition; cycling while the config
    /// panel is focused is intentionally inert (you're configuring, not
    /// flying).
    fn focus_is_eve(&self) -> bool {
        let Ok(active) = self.get_active_window() else {
            return true;
        };
        match self.get_eve_windows() {
            Ok(windows) => windows.iter().any(|w| w.id == active),
            Err(_) => true,
        }
    }

    /// Minimize a window
    fn minimize_window(&self, window_id: u32) -> Result<()>;

    /// Restore a minimized window
    fn restore_window(&self, window_id: u32) -> Result<()>;
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayServer {
    X11,
    Wayland,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaylandCompositor {
    Kde,      // KDE Plasma / KWin
    Sway,     // Sway (wlroots)
    Hyprland, // Hyprland
    Gnome,    // GNOME Shell
    Other,    // Other/unknown compositor
}

/// Detect which display server is running
#[cfg(unix)]
pub fn detect_display_server() -> DisplayServer {
    if let Ok(session_type) = std::env::var("XDG_SESSION_TYPE") {
        if session_type == "wayland" {
            return DisplayServer::Wayland;
        }
    }

    // Fallback: check if WAYLAND_DISPLAY is set
    if std::env::var("WAYLAND_DISPLAY").is_ok() {
        return DisplayServer::Wayland;
    }

    // Default to X11
    DisplayServer::X11
}

/// Detect which Wayland compositor is running
#[cfg(unix)]
pub fn detect_wayland_compositor() -> WaylandCompositor {
    // Check XDG_CURRENT_DESKTOP first
    if let Ok(desktop) = std::env::var("XDG_CURRENT_DESKTOP") {
        let desktop_lower = desktop.to_lowercase();
        if desktop_lower.contains("kde") {
            return WaylandCompositor::Kde;
        }
        if desktop_lower.contains("gnome") {
            return WaylandCompositor::Gnome;
        }
        if desktop_lower.contains("sway") {
            return WaylandCompositor::Sway;
        }
        if desktop_lower.contains("hyprland") {
            return WaylandCompositor::Hyprland;
        }
    }

    // Check for compositor-specific environment variables
    if std::env::var("SWAYSOCK").is_ok() {
        return WaylandCompositor::Sway;
    }

    if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
        return WaylandCompositor::Hyprland;
    }

    WaylandCompositor::Other
}

/// Whether the running session can honor `stack_windows` (move/resize EVE
/// clients to absolute coordinates). This is the ONE operation GNOME's
/// Wayland session forbids: Mutter implements no client-facing window
/// positioning, so an X11/EWMH move-resize on an XWayland window is
/// silently dropped. Every other session — plain X11, GNOME-on-Xorg
/// (positioning works under a real X server), KWin, Sway, Hyprland —
/// supports it. The config panel uses this to hide the Restack button
/// where it would do nothing.
#[cfg(unix)]
pub fn restack_supported(server: DisplayServer, compositor: WaylandCompositor) -> bool {
    !(server == DisplayServer::Wayland && compositor == WaylandCompositor::Gnome)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn restack_unsupported_only_on_gnome_wayland() {
        // The single blocked case.
        assert!(!restack_supported(
            DisplayServer::Wayland,
            WaylandCompositor::Gnome
        ));
        // GNOME-on-Xorg keeps positioning — it's a real X server, so the
        // compositor value is GNOME but the server is X11 and restack works.
        assert!(restack_supported(
            DisplayServer::X11,
            WaylandCompositor::Gnome
        ));
        // Every other Wayland compositor supports positioning.
        for c in [
            WaylandCompositor::Kde,
            WaylandCompositor::Sway,
            WaylandCompositor::Hyprland,
            WaylandCompositor::Other,
        ] {
            assert!(
                restack_supported(DisplayServer::Wayland, c),
                "{c:?} should support restack"
            );
        }
    }
}
