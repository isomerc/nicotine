use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

/// A single per-character hotkey binding. `vk` is a Win32 Virtual-Key
/// code (or evdev code on Linux); `modifier` is an optional second VK
/// that must be held down (typically Shift/Ctrl/Alt).
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq)]
pub struct CharacterHotkey {
    pub vk: u16,
    #[serde(default)]
    pub modifier: Option<u16>,
}

/// What kind of input triggers a [`Hotkey`]: a keyboard key, a mouse
/// button, or a scroll-wheel notch. The `code` is interpreted per-kind —
/// an evdev key/button code on Linux, a Win32 VK / XBUTTON on Windows; for
/// `Wheel` it's [`WHEEL_UP`] / [`WHEEL_DOWN`].
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default)]
pub enum TriggerKind {
    #[default]
    Key,
    Mouse,
    Wheel,
}

/// Wheel-direction sentinels, used as [`Hotkey::code`] when the kind is
/// `Wheel`. Small constants that never collide with real key/button codes.
pub const WHEEL_UP: u16 = 1;
pub const WHEEL_DOWN: u16 = 2;

/// A unified input binding: a trigger (key, mouse button, or wheel notch)
/// plus the set of modifier keys that must be held. Replaces the old
/// single-key + optional-single-modifier shape so chords (Ctrl+Shift+J) and
/// mouse / wheel bindings are all expressible, and the same widget can drive
/// every bind site.
#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, Default)]
pub struct Hotkey {
    /// Modifier codes (Ctrl/Shift/Alt as platform codes) that must be held.
    /// Empty = bare trigger. Compared as a set at match time; kept sorted
    /// for stable serialization.
    #[serde(default)]
    pub mods: Vec<u16>,
    /// Whether `code` names a key, a mouse button, or a wheel direction.
    #[serde(default)]
    pub kind: TriggerKind,
    /// The trigger code, interpreted per `kind`.
    pub code: u16,
}

#[cfg_attr(not(test), allow(dead_code))] // wired into the bind sites in following commits
impl Hotkey {
    /// A bare keyboard-key binding (no modifiers).
    pub fn key(code: u16) -> Self {
        Self {
            mods: Vec::new(),
            kind: TriggerKind::Key,
            code,
        }
    }

    /// A keyboard binding with held modifiers. The modifier list is sorted
    /// and de-duplicated so equal chords compare equal regardless of the
    /// order the user pressed them.
    pub fn key_with_mods(code: u16, mut mods: Vec<u16>) -> Self {
        mods.sort_unstable();
        mods.dedup();
        Self {
            mods,
            kind: TriggerKind::Key,
            code,
        }
    }

    /// True when the currently-held modifier set is exactly this binding's
    /// modifiers — no more, no fewer (so Ctrl+J doesn't fire a bare-J bind,
    /// and vice-versa).
    pub fn mods_match(&self, held: &std::collections::HashSet<u16>) -> bool {
        self.mods.len() == held.len() && self.mods.iter().all(|m| held.contains(m))
    }

    /// True if `code`/`kind` and modifiers all match the given event.
    pub fn matches(
        &self,
        kind: TriggerKind,
        code: u16,
        held: &std::collections::HashSet<u16>,
    ) -> bool {
        self.kind == kind && self.code == code && self.mods_match(held)
    }
}

/// How the visible-at-a-glance view of clients is rendered.
#[derive(Debug, Serialize, Deserialize, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// One DWM thumbnail window per EVE client (default).
    Previews,
    /// A single always-on-top window listing each character name. Active
    /// character shown in Nicotine red with a 🚬 marker.
    List,
}

/// Settings that components watch for *live* changes — e.g. the preview
/// manager resizes windows as soon as these change, without waiting for a
/// save-to-disk + hot-reload cycle. Shared via Arc<Mutex<>> between the
/// config panel (writer) and the preview manager (reader).
// LiveSettings fields are read by the Windows preview manager only.
// On Linux they're allocated and written by `from_config` but never
// read, so suppress the unused-field lint there.
#[derive(Debug, Clone)]
#[cfg_attr(unix, allow(dead_code))]
pub struct LiveSettings {
    pub preview_width: u32,
    pub preview_height: u32,
    pub preview_opacity: u32,
    pub display_mode: DisplayMode,
    /// When true, both preview windows and the client-list window
    /// ignore mouse drags so they can't accidentally be knocked out of
    /// position mid-game. Click-to-activate still works on previews.
    pub positions_locked: bool,
    /// Mirror of `config.show_previews`, updated by the panel toggle so
    /// the preview manager can self-gate live (tear down its windows
    /// when false, spawn them when flipped back on) without a daemon
    /// restart.
    pub show_previews: bool,
    /// When true, the preview of the currently-active EVE client is
    /// hidden (you're already looking at that client full-size). On
    /// cycle, the newly-active client's preview hides and the one cycled
    /// away from reappears. Read live so the toggle takes effect without
    /// a restart.
    pub hide_active_preview: bool,
}

impl LiveSettings {
    pub fn from_config(config: &Config) -> Arc<Mutex<Self>> {
        Arc::new(Mutex::new(Self {
            preview_width: config.preview_width,
            preview_height: config.preview_height,
            preview_opacity: config.preview_opacity,
            display_mode: config.display_mode,
            positions_locked: config.positions_locked,
            show_previews: config.show_previews,
            hide_active_preview: config.hide_active_preview,
        }))
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Config {
    pub display_width: u32,
    pub display_height: u32,
    pub panel_height: u32,
    pub eve_width: u32,
    pub eve_height: u32,
    #[serde(default = "default_enable_mouse")]
    pub enable_mouse_buttons: bool,
    #[serde(default = "default_forward_button")]
    pub forward_button: u16, // BTN_SIDE (mouse button 9)
    #[serde(default = "default_backward_button")]
    pub backward_button: u16, // BTN_EXTRA (mouse button 8)
    #[serde(default = "default_enable_keyboard")]
    pub enable_keyboard_buttons: bool,
    #[serde(default = "default_forward_key")]
    pub forward_key: u16, // KEY_TAB (15) - Tab for forward, Shift+Tab for backward
    #[serde(default = "default_backward_key")]
    pub backward_key: u16, // KEY_TAB (15) - Track SHIFT modifier internally
    #[serde(default = "default_mouse_device_name")]
    pub mouse_device_name: Option<String>,
    #[serde(default = "default_mouse_device_path")]
    pub mouse_device_path: Option<String>,
    #[serde(default = "default_minimize_inactive")]
    pub minimize_inactive: bool,
    #[serde(default = "default_keyboard_device_path")]
    pub keyboard_device_path: Option<String>,
    #[serde(default = "default_modifier_key")]
    pub modifier_key: Option<u16>,
    /// Width of preview windows in pixels (Windows only). Single global value
    /// — every preview gets the same size. Aspect ratio is preserved on the
    /// thumbnail; the window is sized exactly as configured.
    #[serde(default = "default_preview_width")]
    pub preview_width: u32,
    /// Height of preview windows in pixels (Windows only).
    #[serde(default = "default_preview_height")]
    pub preview_height: u32,
    /// Preview-window opacity as a percentage, 10–100 (100 = fully opaque).
    #[serde(default = "default_preview_opacity")]
    pub preview_opacity: u32,
    /// Whether DWM preview windows are spawned at all (Windows only). When
    /// false, the daemon runs headless and you cycle via hotkeys / CLI only.
    #[serde(default = "default_show_previews")]
    pub show_previews: bool,
    /// When true, hide the preview of whichever EVE client is currently
    /// active — its thumbnail is redundant while you're looking at the
    /// real window. Cycling moves the hidden preview to follow the active
    /// client.
    #[serde(default = "default_hide_active_preview")]
    pub hide_active_preview: bool,
    /// When true, the config panel locks preview width and height to a
    /// single aspect ratio and offers one "size" slider instead of
    /// separate width/height sliders. Purely a panel-side concern — the
    /// preview managers still just read `preview_width`/`preview_height`.
    #[serde(default = "default_constrain_aspect")]
    pub constrain_aspect: bool,
    /// Optional global hotkey (platform key code — evdev on Linux, Win32
    /// VK on Windows) that toggles preview-window visibility, i.e. flips
    /// `show_previews`. `None` = unbound. Bound via the panel's Hotkeys
    /// tab just like the cycle keys.
    #[serde(default)]
    pub toggle_previews_key: Option<u16>,
    /// Optional modifier (Shift/Ctrl/Alt, as a platform key code) that
    /// must be held with `toggle_previews_key` for the toggle to fire.
    /// `None` = no modifier (the bare key toggles).
    #[serde(default)]
    pub toggle_previews_modifier: Option<u16>,
    /// Ordered list of EVE character names. Forward/backward cycling
    /// traverses this order; `switch N` maps target N to entry N-1.
    /// Empty list = cycle through whatever order the window manager
    /// reports (no stable ordering).
    #[serde(default)]
    pub characters: Vec<String>,
    /// Which on-screen representation of running clients Nicotine shows.
    #[serde(default = "default_display_mode")]
    pub display_mode: DisplayMode,
    /// When true, drag is disabled on preview windows and the client
    /// list so they can't accidentally move during gameplay.
    #[serde(default)]
    pub positions_locked: bool,
    /// Map of character name → hotkey for jump-to-character. When the
    /// configured key (plus optional modifier) fires, Nicotine activates
    /// that EVE client directly — independent of the forward/backward
    /// cycle. Keyed by name so bindings follow reorders and renames
    /// without reassigning keys.
    #[serde(default)]
    pub character_hotkeys: HashMap<String, CharacterHotkey>,
    /// Config-panel window size in logical pixels. Persisted so a manual
    /// resize of the panel survives restarts; defaults to the original
    /// fixed window size.
    #[serde(default = "default_window_width")]
    pub window_width: u32,
    #[serde(default = "default_window_height")]
    pub window_height: u32,
}

#[cfg(unix)]
fn default_enable_mouse() -> bool {
    true
}

// Off by default on Windows — most users remap side buttons at the
// driver level (Logi Options+, etc.) and use Nicotine's keyboard
// hotkeys instead. When the native hook is on, it intercepts XBUTTON1/2
// from games and browsers (back/forward) which surprises users who
// didn't ask for cycling there.
#[cfg(windows)]
fn default_enable_mouse() -> bool {
    false
}

#[cfg(unix)]
fn default_forward_button() -> u16 {
    276 // BTN_SIDE (forward button, mouse button 9) — evdev code
}

#[cfg(windows)]
fn default_forward_button() -> u16 {
    2 // XBUTTON2 (forward side button)
}

#[cfg(unix)]
fn default_backward_button() -> u16 {
    275 // BTN_EXTRA (backward button, mouse button 8) — evdev code
}

#[cfg(windows)]
fn default_backward_button() -> u16 {
    1 // XBUTTON1 (backward side button)
}

#[cfg(unix)]
fn default_enable_keyboard() -> bool {
    false // Disabled by default to avoid conflicts with games that use Tab
}

#[cfg(windows)]
fn default_enable_keyboard() -> bool {
    true // F10/F11 are uncommon enough to enable by default for cycling
}

#[cfg(unix)]
fn default_forward_key() -> u16 {
    15 // KEY_TAB — evdev code
}

#[cfg(windows)]
fn default_forward_key() -> u16 {
    0x7A // VK_F11
}

#[cfg(unix)]
fn default_backward_key() -> u16 {
    15 // KEY_TAB (Modifier applied if set) — evdev code
}

#[cfg(windows)]
fn default_backward_key() -> u16 {
    0x79 // VK_F10
}

fn default_mouse_device_name() -> Option<String> {
    None
}

fn default_mouse_device_path() -> Option<String> {
    None
}

fn default_minimize_inactive() -> bool {
    false
}

fn default_keyboard_device_path() -> Option<String> {
    None
}

fn default_modifier_key() -> Option<u16> {
    None // No modifier for backward shifting by default
}

fn default_preview_width() -> u32 {
    320
}

fn default_preview_height() -> u32 {
    180
}

fn default_preview_opacity() -> u32 {
    100
}

fn default_show_previews() -> bool {
    true
}

fn default_hide_active_preview() -> bool {
    false
}

fn default_constrain_aspect() -> bool {
    false
}

fn default_display_mode() -> DisplayMode {
    DisplayMode::Previews
}

fn default_window_width() -> u32 {
    720
}

fn default_window_height() -> u32 {
    680
}

impl Config {
    /// Resolve the directory holding `config.toml`. Production callers
    /// get the platform-standard config dir (XDG on Linux, Roaming
    /// APPDATA on Windows) under a `nicotine` subdir.
    ///
    /// Integration tests set `NICOTINE_CONFIG_DIR` to a private temp
    /// directory so the test daemon reads/writes a config isolated
    /// from the user's real one. Necessary because on Windows
    /// `dirs::config_dir()` uses `SHGetKnownFolderPath`, which
    /// returns the canonical user folder regardless of the `APPDATA`
    /// env var — so env-overriding APPDATA alone doesn't isolate.
    /// The override IS the full directory (no implicit `nicotine`
    /// subdir suffix), matching the value the test fixture provides.
    fn config_dir() -> PathBuf {
        if let Ok(dir) = std::env::var("NICOTINE_CONFIG_DIR") {
            return PathBuf::from(dir);
        }
        let mut path = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        path.push("nicotine");
        path
    }

    fn config_path() -> PathBuf {
        let mut path = Self::config_dir();
        path.push("config.toml");
        path
    }

    /// Persist the current Config back to disk. Used by the config panel
    /// to commit user edits. Only called from the Windows config panel,
    /// hence the dead-code allow on Linux.
    #[cfg_attr(unix, allow(dead_code))]
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path();
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self).context("Failed to serialize config")?;
        fs::write(&config_path, contents).context("Failed to write config.toml")?;
        Ok(())
    }

    #[cfg(unix)]
    fn detect_display_size() -> (u32, u32) {
        if let Ok(output) = std::process::Command::new("xrandr")
            .args(["--current"])
            .output()
        {
            if let Ok(stdout) = String::from_utf8(output.stdout) {
                for line in stdout.lines() {
                    if line.contains("*") && line.contains("x") {
                        // Parse line like: "7680x2160     60.00*+"
                        if let Some(resolution) = line.split_whitespace().next() {
                            if let Some((w, h)) = resolution.split_once('x') {
                                if let (Ok(width), Ok(height)) = (w.parse(), h.parse()) {
                                    return (width, height);
                                }
                            }
                        }
                    }
                }
            }
        }
        (1920, 1080)
    }

    #[cfg(windows)]
    fn detect_display_size() -> (u32, u32) {
        use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
        let w = unsafe { GetSystemMetrics(SM_CXSCREEN) };
        let h = unsafe { GetSystemMetrics(SM_CYSCREEN) };
        if w > 0 && h > 0 {
            (w as u32, h as u32)
        } else {
            (1920, 1080)
        }
    }

    fn build_default(display_width: u32, display_height: u32) -> Self {
        Self {
            display_width,
            display_height,
            panel_height: 0,
            eve_width: (display_width as f32 * 0.54) as u32, // ~54% of width
            eve_height: display_height,
            enable_mouse_buttons: default_enable_mouse(),
            forward_button: default_forward_button(),
            backward_button: default_backward_button(),
            enable_keyboard_buttons: default_enable_keyboard(),
            forward_key: default_forward_key(),
            backward_key: default_backward_key(),
            mouse_device_name: default_mouse_device_name(),
            mouse_device_path: default_mouse_device_path(),
            minimize_inactive: default_minimize_inactive(),
            keyboard_device_path: default_keyboard_device_path(),
            modifier_key: default_modifier_key(),
            preview_width: default_preview_width(),
            preview_height: default_preview_height(),
            preview_opacity: default_preview_opacity(),
            show_previews: default_show_previews(),
            hide_active_preview: default_hide_active_preview(),
            constrain_aspect: default_constrain_aspect(),
            toggle_previews_key: None,
            toggle_previews_modifier: None,
            characters: Vec::new(),
            display_mode: default_display_mode(),
            positions_locked: false,
            character_hotkeys: HashMap::new(),
            window_width: default_window_width(),
            window_height: default_window_height(),
        }
    }

    pub fn load() -> Result<Self> {
        let config_path = Self::config_path();

        if let Ok(contents) = fs::read_to_string(&config_path) {
            return toml::from_str(&contents).context("Failed to parse config.toml");
        }

        println!("Generating config based on your display...");
        let (display_width, display_height) = Self::detect_display_size();
        println!("Detected display: {}x{}", display_width, display_height);

        let config = Self::build_default(display_width, display_height);

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(&config)?;
        fs::write(&config_path, contents)?;
        println!("Created config: {}", config_path.display());
        println!("Edit it to customize window sizes and positions");

        Ok(config)
    }

    pub fn save_default() -> Result<()> {
        let config_path = Self::config_path();
        let (display_width, display_height) = Self::detect_display_size();

        let config = Self::build_default(display_width, display_height);

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(&config)?;
        fs::write(&config_path, contents)?;
        println!("Created config: {}", config_path.display());
        Ok(())
    }

    pub fn eve_height_adjusted(&self) -> u32 {
        self.display_height - self.panel_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn hotkey_mods_sorted_and_deduped() {
        let hk = Hotkey::key_with_mods(30, vec![56, 29, 29, 42]);
        assert_eq!(hk.mods, vec![29, 42, 56]); // sorted, deduped
        assert_eq!(hk.kind, TriggerKind::Key);
    }

    #[test]
    fn hotkey_mods_match_is_exact() {
        let hk = Hotkey::key_with_mods(30, vec![29, 42]); // Ctrl+Shift+X
        let exact: HashSet<u16> = [29, 42].into_iter().collect();
        let subset: HashSet<u16> = [29].into_iter().collect();
        let superset: HashSet<u16> = [29, 42, 56].into_iter().collect();
        assert!(hk.mods_match(&exact));
        assert!(!hk.mods_match(&subset), "missing a required modifier");
        assert!(!hk.mods_match(&superset), "extra modifier held");
    }

    #[test]
    fn bare_key_requires_no_modifiers() {
        let hk = Hotkey::key(30);
        let none: HashSet<u16> = HashSet::new();
        let ctrl: HashSet<u16> = [29].into_iter().collect();
        assert!(hk.matches(TriggerKind::Key, 30, &none));
        assert!(!hk.matches(TriggerKind::Key, 30, &ctrl), "Ctrl held but bare bind");
        assert!(!hk.matches(TriggerKind::Mouse, 30, &none), "wrong kind");
    }

    #[test]
    fn hotkey_round_trips_through_toml() {
        let hk = Hotkey {
            mods: vec![29, 42],
            kind: TriggerKind::Wheel,
            code: WHEEL_UP,
        };
        let s = toml::to_string(&hk).unwrap();
        let back: Hotkey = toml::from_str(&s).unwrap();
        assert_eq!(hk, back);
    }

    #[test]
    fn test_eve_height_adjusted_with_panel() {
        let config = Config {
            display_width: 1920,
            display_height: 1080,
            panel_height: 40,
            window_width: 720,
            window_height: 680,
            eve_width: 1000,
            eve_height: 1080,
            enable_mouse_buttons: true,
            forward_button: 276,
            backward_button: 275,
            enable_keyboard_buttons: false,
            forward_key: 15,
            backward_key: 15,
            mouse_device_name: None,
            mouse_device_path: None,
            minimize_inactive: false,
            keyboard_device_path: None,
            modifier_key: None,
            preview_width: 320,
            preview_height: 180,
            preview_opacity: 100,
            show_previews: true,
            hide_active_preview: false,
            constrain_aspect: false,
            toggle_previews_key: None,
            toggle_previews_modifier: None,
            characters: Vec::new(),
            display_mode: DisplayMode::Previews,
            positions_locked: false,
            character_hotkeys: HashMap::new(),
        };

        // Height should be: 1080 - 40 = 1040
        assert_eq!(config.eve_height_adjusted(), 1040);
    }

    #[test]
    fn test_eve_height_adjusted_without_panel() {
        let config = Config {
            display_width: 1920,
            display_height: 1080,
            panel_height: 0,
            window_width: 720,
            window_height: 680,
            eve_width: 1000,
            eve_height: 1080,
            enable_mouse_buttons: true,
            forward_button: 276,
            backward_button: 275,
            enable_keyboard_buttons: false,
            forward_key: 15,
            backward_key: 15,
            mouse_device_name: None,
            mouse_device_path: None,
            minimize_inactive: false,
            keyboard_device_path: None,
            modifier_key: None,
            preview_width: 320,
            preview_height: 180,
            preview_opacity: 100,
            show_previews: true,
            hide_active_preview: false,
            constrain_aspect: false,
            toggle_previews_key: None,
            toggle_previews_modifier: None,
            characters: Vec::new(),
            display_mode: DisplayMode::Previews,
            positions_locked: false,
            character_hotkeys: HashMap::new(),
        };

        assert_eq!(config.eve_height_adjusted(), 1080);
    }

    #[test]
    fn test_config_serialization() {
        let config = Config {
            display_width: 7680,
            display_height: 2160,
            panel_height: 0,
            window_width: 900,
            window_height: 800,
            eve_width: 4147,
            eve_height: 2160,
            enable_mouse_buttons: true,
            forward_button: 276,
            backward_button: 275,
            enable_keyboard_buttons: false,
            forward_key: 15,
            backward_key: 15,
            mouse_device_name: None,
            mouse_device_path: None,
            minimize_inactive: false,
            keyboard_device_path: None,
            modifier_key: None,
            preview_width: 320,
            preview_height: 180,
            preview_opacity: 55,
            show_previews: true,
            hide_active_preview: true,
            constrain_aspect: true,
            toggle_previews_key: Some(67),
            toggle_previews_modifier: Some(42),
            characters: Vec::new(),
            display_mode: DisplayMode::Previews,
            positions_locked: false,
            character_hotkeys: HashMap::new(),
        };

        let toml_str = toml::to_string(&config).unwrap();
        let deserialized: Config = toml::from_str(&toml_str).unwrap();

        assert_eq!(deserialized.display_width, 7680);
        assert_eq!(deserialized.display_height, 2160);
        assert_eq!(deserialized.eve_width, 4147);
        // Non-default on purpose: proves the value is actually persisted,
        // not silently masked by the serde default on load.
        assert_eq!(deserialized.preview_opacity, 55);
        assert!(
            deserialized.hide_active_preview,
            "hide_active_preview must survive a save/load round-trip"
        );
        assert!(
            deserialized.constrain_aspect,
            "constrain_aspect must survive a save/load round-trip"
        );
        assert_eq!(
            deserialized.toggle_previews_key,
            Some(67),
            "toggle_previews_key binding must survive a save/load round-trip"
        );
        assert_eq!(
            deserialized.toggle_previews_modifier,
            Some(42),
            "toggle_previews_modifier must survive a save/load round-trip"
        );
        assert_eq!(
            deserialized.window_width, 900,
            "window_width must survive a save/load round-trip"
        );
        assert_eq!(
            deserialized.window_height, 800,
            "window_height must survive a save/load round-trip"
        );
    }
}
