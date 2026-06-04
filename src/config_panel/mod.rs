use crate::config::{CharacterHotkey, Config, LiveSettings};
use iced::{Color, Element, Subscription, Task, Theme};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod ui;

/// After any config edit we wait this long with no further edits before
/// flushing to disk. Slider drags and text input coalesce into a single
/// write rather than hammering disk on every pixel / keystroke.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(300);

// ---- Brand palette (matches the Linux preview/list windows). ----
pub(super) const NICOTINE_RED: Color = Color {
    r: 196.0 / 255.0,
    g: 30.0 / 255.0,
    b: 58.0 / 255.0,
    a: 1.0,
};
pub(super) const NICOTINE_GOLD: Color = Color {
    r: 180.0 / 255.0,
    g: 155.0 / 255.0,
    b: 105.0 / 255.0,
    a: 1.0,
};
pub(super) const NICOTINE_CREAM: Color = Color {
    r: 252.0 / 255.0,
    g: 250.0 / 255.0,
    b: 242.0 / 255.0,
    a: 1.0,
};
pub(super) const NICOTINE_BLACK: Color = Color {
    r: 30.0 / 255.0,
    g: 30.0 / 255.0,
    b: 30.0 / 255.0,
    a: 1.0,
};
pub(super) const NICOTINE_GREEN: Color = Color {
    r: 60.0 / 255.0,
    g: 140.0 / 255.0,
    b: 70.0 / 255.0,
    a: 1.0,
};

// ---- Type scale (logical px). One deliberate set of sizes. ----
// TEXT_SIZE is the app-wide default (set in `run`); everything interactive
// inherits it. The others are the only sanctioned overrides.
pub(super) const TEXT_SIZE: f32 = 14.0;
pub(super) const SECTION_SIZE: f32 = 18.0;
pub(super) const CAPTION_SIZE: f32 = 12.0;
pub(super) const LOGO_SIZE: f32 = 44.0;

/// Which config field is currently capturing a live keypress. Only one
/// can capture at a time; `None` means no capture. `Character` carries the
/// character name so per-character bindings survive list reorders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CaptureTarget {
    ForwardKey,
    BackwardKey,
    ModifierKey,
    Character(String),
}

/// Which settings tab is shown in the left nav rail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Tab {
    Display,
    Characters,
    Hotkeys,
}

/// One entry in the modifier dropdown. Codes are platform-specific:
/// Win32 VK on Windows, Linux evdev keycodes on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ModifierChoice {
    pub code: Option<u16>,
    pub label: &'static str,
}

impl std::fmt::Display for ModifierChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label)
    }
}

#[cfg(windows)]
pub(super) const MODIFIER_CHOICES: &[ModifierChoice] = &[
    ModifierChoice {
        code: None,
        label: "None",
    },
    ModifierChoice {
        code: Some(0x10),
        label: "Shift",
    },
    ModifierChoice {
        code: Some(0x11),
        label: "Ctrl",
    },
    ModifierChoice {
        code: Some(0x12),
        label: "Alt",
    },
];

#[cfg(unix)]
pub(super) const MODIFIER_CHOICES: &[ModifierChoice] = &[
    ModifierChoice {
        code: None,
        label: "None",
    },
    ModifierChoice {
        code: Some(42),
        label: "Shift",
    }, // KEY_LEFTSHIFT
    ModifierChoice {
        code: Some(29),
        label: "Ctrl",
    }, // KEY_LEFTCTRL
    ModifierChoice {
        code: Some(56),
        label: "Alt",
    }, // KEY_LEFTALT
];

pub(super) struct Panel {
    pub config: Config,
    /// Shared settings watched by the preview manager for live updates.
    pub live: Arc<Mutex<LiveSettings>>,
    /// Buffer for the "add character" text input.
    pub new_character_buffer: String,
    /// When `Some(..)`, the next keypress binds to this field.
    pub capturing: Option<CaptureTarget>,
    /// Timestamp of the last edit; the debounce subscription flushes to
    /// disk once this is older than `AUTOSAVE_DEBOUNCE`.
    pub last_change: Option<Instant>,
    /// Active left-rail settings tab.
    pub active_tab: Tab,
}

impl Panel {
    fn new(config: Config, live: Arc<Mutex<LiveSettings>>) -> Self {
        Self {
            config,
            live,
            new_character_buffer: String::new(),
            capturing: None,
            last_change: None,
            active_tab: Tab::Display,
        }
    }

    /// Mark the config as edited so the debounce subscription saves it
    /// once the user goes idle.
    pub(super) fn touch(&mut self) {
        self.last_change = Some(Instant::now());
    }

    fn handle_capture_key(&mut self, event: iced::keyboard::Event) -> Task<Message> {
        use iced::keyboard::key::Named;
        use iced::keyboard::{Event, Key};

        let Some(target) = self.capturing.clone() else {
            return Task::none();
        };

        let Event::KeyPressed { key, .. } = event else {
            return Task::none();
        };

        // Escape cancels capture without binding.
        if matches!(key, Key::Named(Named::Escape)) {
            self.capturing = None;
            #[cfg(windows)]
            self.end_windows_capture();
            return Task::none();
        }

        // On Windows, OEM/punctuation VKs are layout-dependent; ask the OS
        // for the actually-held VK first so the captured code matches what
        // RegisterHotKey will see at runtime regardless of keyboard layout.
        #[cfg(windows)]
        let code = oem_vk_currently_pressed().or_else(|| iced_key_to_code(&key));
        #[cfg(unix)]
        let code = iced_key_to_code(&key);

        if let Some(vk) = code {
            match &target {
                CaptureTarget::ForwardKey => self.config.forward_key = vk,
                CaptureTarget::BackwardKey => self.config.backward_key = vk,
                CaptureTarget::ModifierKey => self.config.modifier_key = Some(vk),
                CaptureTarget::Character(name) => {
                    let modifier = self
                        .config
                        .character_hotkeys
                        .get(name)
                        .and_then(|h| h.modifier);
                    self.config
                        .character_hotkeys
                        .insert(name.clone(), CharacterHotkey { vk, modifier });
                }
            }
            self.capturing = None;
            self.touch();
            #[cfg(windows)]
            self.end_windows_capture();
        }

        Task::none()
    }

    /// Windows: flush config.toml synchronously so the listener's
    /// `Config::load()` sees the new binding, then resume global hotkeys
    /// (paused during capture so RegisterHotKey doesn't eat the key).
    #[cfg(windows)]
    fn end_windows_capture(&mut self) {
        if self.last_change.is_some() {
            let _ = self.config.save();
            self.last_change = None;
        }
        crate::windows_input::resume_hotkeys();
    }
}

#[derive(Debug, Clone)]
pub(super) enum Message {
    DisplayModeChanged(crate::config::DisplayMode),
    LockToggled(bool),
    RestackClicked,
    CharacterNameChanged(usize, String),
    MoveCharacterUp(usize),
    MoveCharacterDown(usize),
    RemoveCharacter(usize),
    NewCharacterChanged(String),
    AddCharacter,
    CharacterModifierChanged(String, ModifierChoice),
    ClearCharacterHotkey(String),
    KeyboardEnabledToggled(bool),
    MouseEnabledToggled(bool),
    ClearModifier,
    StartCapture(CaptureTarget),
    TabSelected(Tab),
    KeyEvent(iced::keyboard::Event),
    ShowPreviewsToggled(bool),
    PreviewWidthChanged(u32),
    PreviewHeightChanged(u32),
    OpenLink(String),
    FlushIfIdle,
}

fn update(panel: &mut Panel, message: Message) -> Task<Message> {
    match message {
        Message::DisplayModeChanged(mode) => {
            panel.config.display_mode = mode;
            panel.live.lock().unwrap().display_mode = mode;
            panel.touch();
        }
        Message::LockToggled(v) => {
            panel.config.positions_locked = v;
            panel.live.lock().unwrap().positions_locked = v;
            panel.touch();
        }
        Message::RestackClicked => {
            let _ = crate::daemon::send_command("stack");
        }
        Message::CharacterNameChanged(i, name) => {
            if let Some(slot) = panel.config.characters.get_mut(i) {
                *slot = name;
                panel.touch();
            }
        }
        Message::MoveCharacterUp(i) => {
            if i > 0 && i < panel.config.characters.len() {
                panel.config.characters.swap(i, i - 1);
                panel.touch();
            }
        }
        Message::MoveCharacterDown(i) => {
            if i + 1 < panel.config.characters.len() {
                panel.config.characters.swap(i, i + 1);
                panel.touch();
            }
        }
        Message::RemoveCharacter(i) => {
            if i < panel.config.characters.len() {
                let name = panel.config.characters.remove(i);
                panel.config.character_hotkeys.remove(&name);
                panel.touch();
            }
        }
        Message::NewCharacterChanged(s) => {
            panel.new_character_buffer = s;
        }
        Message::AddCharacter => {
            let name = panel.new_character_buffer.trim().to_string();
            if !name.is_empty() {
                panel.config.characters.push(name);
                panel.new_character_buffer.clear();
                panel.touch();
            }
        }
        Message::CharacterModifierChanged(name, choice) => {
            let entry = panel
                .config
                .character_hotkeys
                .entry(name)
                .or_insert(CharacterHotkey {
                    vk: 0,
                    modifier: None,
                });
            entry.modifier = choice.code;
            panel.touch();
        }
        Message::ClearCharacterHotkey(name) => {
            panel.config.character_hotkeys.remove(&name);
            panel.touch();
        }
        Message::KeyboardEnabledToggled(v) => {
            panel.config.enable_keyboard_buttons = v;
            panel.touch();
        }
        Message::MouseEnabledToggled(v) => {
            panel.config.enable_mouse_buttons = v;
            panel.touch();
        }
        Message::ClearModifier => {
            panel.config.modifier_key = None;
            panel.touch();
        }
        Message::StartCapture(target) => {
            if panel.capturing.as_ref() == Some(&target) {
                // Clicking the active bind button again cancels.
                panel.capturing = None;
                #[cfg(windows)]
                panel.end_windows_capture();
            } else {
                #[cfg(windows)]
                if panel.capturing.is_none() {
                    crate::windows_input::pause_hotkeys();
                }
                panel.capturing = Some(target);
            }
        }
        Message::TabSelected(tab) => {
            panel.active_tab = tab;
        }
        Message::KeyEvent(event) => {
            return panel.handle_capture_key(event);
        }
        Message::ShowPreviewsToggled(v) => {
            panel.config.show_previews = v;
            panel.live.lock().unwrap().show_previews = v;
            panel.touch();
        }
        Message::PreviewWidthChanged(w) => {
            panel.config.preview_width = w;
            panel.live.lock().unwrap().preview_width = w;
            panel.touch();
        }
        Message::PreviewHeightChanged(h) => {
            panel.config.preview_height = h;
            panel.live.lock().unwrap().preview_height = h;
            panel.touch();
        }
        Message::OpenLink(url) => {
            open_url(&url);
        }
        Message::FlushIfIdle => {
            if let Some(t) = panel.last_change {
                if t.elapsed() >= AUTOSAVE_DEBOUNCE {
                    if let Err(e) = panel.config.save() {
                        eprintln!("config autosave failed: {e}");
                    }
                    panel.last_change = None;
                }
            }
        }
    }
    Task::none()
}

fn view(panel: &Panel) -> Element<'_, Message> {
    use iced::widget::{column, row, scrollable};
    use iced::Length;

    column![
        ui::header(),
        row![
            ui::tab_sidebar(panel),
            ui::vdivider(),
            scrollable(ui::tab_content(panel))
                .width(Length::Fill)
                .height(Length::Fill),
        ]
        .height(Length::Fill),
        ui::footer(),
    ]
    .width(Length::Fill)
    .height(Length::Fill)
    .into()
}

fn theme(_panel: &Panel) -> Theme {
    Theme::custom(
        "Nicotine".to_string(),
        iced::theme::Palette {
            background: NICOTINE_CREAM,
            text: NICOTINE_BLACK,
            primary: NICOTINE_RED,
            success: NICOTINE_GREEN,
            warning: NICOTINE_GOLD,
            danger: NICOTINE_RED,
        },
    )
}

fn subscription(panel: &Panel) -> Subscription<Message> {
    let mut subs = Vec::new();
    // Only listen for raw key events while a bind button is armed.
    if panel.capturing.is_some() {
        subs.push(iced::keyboard::listen().map(Message::KeyEvent));
    }
    // Poll only while a save is pending so the debounce fires even with
    // no further input; stops once flushed.
    if panel.last_change.is_some() {
        subs.push(iced::time::every(Duration::from_millis(100)).map(|_| Message::FlushIfIdle));
    }
    Subscription::batch(subs)
}

fn open_url(url: &str) {
    #[cfg(unix)]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
}

/// Map an iced key to the Linux evdev keycode (from
/// <linux/input-event-codes.h>). Letters/digits/punctuation arrive as
/// `Key::Character`; everything else as `Key::Named`.
#[cfg(unix)]
fn iced_key_to_code(key: &iced::keyboard::Key) -> Option<u16> {
    use iced::keyboard::key::Named;
    use iced::keyboard::Key;
    match key {
        Key::Named(named) => Some(match named {
            Named::F1 => 59,
            Named::F2 => 60,
            Named::F3 => 61,
            Named::F4 => 62,
            Named::F5 => 63,
            Named::F6 => 64,
            Named::F7 => 65,
            Named::F8 => 66,
            Named::F9 => 67,
            Named::F10 => 68,
            Named::F11 => 87,
            Named::F12 => 88,
            Named::F13 => 183,
            Named::F14 => 184,
            Named::F15 => 185,
            Named::F16 => 186,
            Named::F17 => 187,
            Named::F18 => 188,
            Named::F19 => 189,
            Named::F20 => 190,
            Named::F21 => 191,
            Named::F22 => 192,
            Named::F23 => 193,
            Named::F24 => 194,
            Named::Tab => 15,
            Named::Space => 57,
            Named::Enter => 28,
            Named::Backspace => 14,
            Named::Insert => 110,
            Named::Delete => 111,
            Named::Home => 102,
            Named::End => 107,
            Named::PageUp => 104,
            Named::PageDown => 109,
            Named::ArrowUp => 103,
            Named::ArrowDown => 108,
            Named::ArrowLeft => 105,
            Named::ArrowRight => 106,
            _ => return None,
        }),
        Key::Character(s) => char_to_evdev(s.as_str()),
        _ => None,
    }
}

/// QWERTY-position evdev codes for printable keys. Mirrors the prior egui
/// mapping (scancode order, not alphabetical).
#[cfg(unix)]
fn char_to_evdev(s: &str) -> Option<u16> {
    let c = s.chars().next()?.to_ascii_lowercase();
    Some(match c {
        'q' => 16,
        'w' => 17,
        'e' => 18,
        'r' => 19,
        't' => 20,
        'y' => 21,
        'u' => 22,
        'i' => 23,
        'o' => 24,
        'p' => 25,
        'a' => 30,
        's' => 31,
        'd' => 32,
        'f' => 33,
        'g' => 34,
        'h' => 35,
        'j' => 36,
        'k' => 37,
        'l' => 38,
        'z' => 44,
        'x' => 45,
        'c' => 46,
        'v' => 47,
        'b' => 48,
        'n' => 49,
        'm' => 50,
        '1' => 2,
        '2' => 3,
        '3' => 4,
        '4' => 5,
        '5' => 6,
        '6' => 7,
        '7' => 8,
        '8' => 9,
        '9' => 10,
        '0' => 11,
        '`' => 41,
        '-' => 12,
        '=' => 13,
        '[' => 26,
        ']' => 27,
        '\\' => 43,
        ';' => 39,
        '\'' => 40,
        ',' => 51,
        '.' => 52,
        '/' => 53,
        _ => return None,
    })
}

/// Map an iced key to the Windows Virtual-Key code.
#[cfg(windows)]
fn iced_key_to_code(key: &iced::keyboard::Key) -> Option<u16> {
    use iced::keyboard::key::Named;
    use iced::keyboard::Key;
    match key {
        Key::Named(named) => Some(match named {
            Named::F1 => 0x70,
            Named::F2 => 0x71,
            Named::F3 => 0x72,
            Named::F4 => 0x73,
            Named::F5 => 0x74,
            Named::F6 => 0x75,
            Named::F7 => 0x76,
            Named::F8 => 0x77,
            Named::F9 => 0x78,
            Named::F10 => 0x79,
            Named::F11 => 0x7A,
            Named::F12 => 0x7B,
            Named::F13 => 0x7C,
            Named::F14 => 0x7D,
            Named::F15 => 0x7E,
            Named::F16 => 0x7F,
            Named::F17 => 0x80,
            Named::F18 => 0x81,
            Named::F19 => 0x82,
            Named::F20 => 0x83,
            Named::F21 => 0x84,
            Named::F22 => 0x85,
            Named::F23 => 0x86,
            Named::F24 => 0x87,
            Named::Tab => 0x09,
            Named::Space => 0x20,
            Named::Enter => 0x0D,
            Named::Backspace => 0x08,
            Named::Insert => 0x2D,
            Named::Delete => 0x2E,
            Named::Home => 0x24,
            Named::End => 0x23,
            Named::PageUp => 0x21,
            Named::PageDown => 0x22,
            Named::ArrowUp => 0x26,
            Named::ArrowDown => 0x28,
            Named::ArrowLeft => 0x25,
            Named::ArrowRight => 0x27,
            _ => return None,
        }),
        Key::Character(s) => {
            let c = s.chars().next()?.to_ascii_lowercase();
            Some(match c {
                'a'..='z' => 0x41 + (c as u16 - 'a' as u16),
                '0'..='9' => 0x30 + (c as u16 - '0' as u16),
                '`' => 0xC0,
                '-' => 0xBD,
                '=' => 0xBB,
                '[' => 0xDB,
                ']' => 0xDD,
                '\\' => 0xDC,
                ';' => 0xBA,
                '\'' => 0xDE,
                ',' => 0xBC,
                '.' => 0xBE,
                '/' => 0xBF,
                _ => return None,
            })
        }
        _ => None,
    }
}

/// Scan the Windows OEM/punctuation VK range and return the first VK whose
/// "down" bit is set — bypasses layout-dependent translation for keys
/// whose VK varies by keyboard layout.
#[cfg(windows)]
fn oem_vk_currently_pressed() -> Option<u16> {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    for &vk in OEM_VK_RANGE {
        let state = unsafe { GetAsyncKeyState(vk as i32) };
        if (state as u32 & 0x8000) != 0 {
            return Some(vk);
        }
    }
    None
}

/// Translate a Windows OEM VK to its current keyboard layout's character
/// via `MapVirtualKeyW(MAPVK_VK_TO_CHAR)`. Returns the unshifted printable
/// character (e.g. `VK_OEM_5` is `\` on US, `#` on German).
#[cfg(windows)]
fn oem_vk_to_char(vk: u16) -> Option<String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{MapVirtualKeyW, MAPVK_VK_TO_CHAR};
    let result = unsafe { MapVirtualKeyW(vk as u32, MAPVK_VK_TO_CHAR) };
    if result == 0 {
        return None;
    }
    let ch = (result & 0xFFFF) as u32;
    if ch == 0 {
        return None;
    }
    char::from_u32(ch).map(|c| c.to_string())
}

/// The complete VK range whose layout-dependence we route through the OS.
#[cfg(windows)]
const OEM_VK_RANGE: &[u16] = &[
    0xBA, 0xBB, 0xBC, 0xBD, 0xBE, 0xBF, 0xC0, // OEM_1, +, ,, -, ., /, ~
    0xDB, 0xDC, 0xDD, 0xDE, 0xDF, // OEM_4-8
    0xE2, // OEM_102 (ISO key)
];

/// Human label for a Win32 VK code, used on the bind button.
#[cfg(windows)]
pub(super) fn code_to_label(vk: u16) -> String {
    match vk {
        0x70 => "F1".into(),
        0x71 => "F2".into(),
        0x72 => "F3".into(),
        0x73 => "F4".into(),
        0x74 => "F5".into(),
        0x75 => "F6".into(),
        0x76 => "F7".into(),
        0x77 => "F8".into(),
        0x78 => "F9".into(),
        0x79 => "F10".into(),
        0x7A => "F11".into(),
        0x7B => "F12".into(),
        0x7C => "F13".into(),
        0x7D => "F14".into(),
        0x7E => "F15".into(),
        0x7F => "F16".into(),
        0x80 => "F17".into(),
        0x81 => "F18".into(),
        0x82 => "F19".into(),
        0x83 => "F20".into(),
        0x84 => "F21".into(),
        0x85 => "F22".into(),
        0x86 => "F23".into(),
        0x87 => "F24".into(),
        0x09 => "Tab".into(),
        0x20 => "Space".into(),
        0x0D => "Enter".into(),
        0x08 => "Backspace".into(),
        0x1B => "Escape".into(),
        0x10 | 0xA0 | 0xA1 => "Shift".into(),
        0x11 | 0xA2 | 0xA3 => "Ctrl".into(),
        0x12 | 0xA4 | 0xA5 => "Alt".into(),
        0x30..=0x39 => format!("{}", (vk - 0x30) as u8 as char),
        0x41..=0x5A => format!("{}", vk as u8 as char),
        0x26 => "Up".into(),
        0x28 => "Down".into(),
        0x25 => "Left".into(),
        0x27 => "Right".into(),
        0xBA..=0xC0 | 0xDB..=0xDF | 0xE2 => {
            oem_vk_to_char(vk).unwrap_or_else(|| format!("VK 0x{:02X}", vk))
        }
        other => format!("VK 0x{:02X}", other),
    }
}

/// Human label for a Linux evdev keycode. Codes from
/// <linux/input-event-codes.h>.
#[cfg(unix)]
pub(super) fn code_to_label(code: u16) -> String {
    match code {
        59 => "F1".into(),
        60 => "F2".into(),
        61 => "F3".into(),
        62 => "F4".into(),
        63 => "F5".into(),
        64 => "F6".into(),
        65 => "F7".into(),
        66 => "F8".into(),
        67 => "F9".into(),
        68 => "F10".into(),
        87 => "F11".into(),
        88 => "F12".into(),
        183 => "F13".into(),
        184 => "F14".into(),
        185 => "F15".into(),
        186 => "F16".into(),
        187 => "F17".into(),
        188 => "F18".into(),
        189 => "F19".into(),
        190 => "F20".into(),
        191 => "F21".into(),
        192 => "F22".into(),
        193 => "F23".into(),
        194 => "F24".into(),
        15 => "Tab".into(),
        57 => "Space".into(),
        28 => "Enter".into(),
        14 => "Backspace".into(),
        1 => "Escape".into(),
        110 => "Insert".into(),
        111 => "Delete".into(),
        102 => "Home".into(),
        107 => "End".into(),
        104 => "PageUp".into(),
        109 => "PageDown".into(),
        103 => "Up".into(),
        108 => "Down".into(),
        105 => "Left".into(),
        106 => "Right".into(),
        42 | 54 => "Shift".into(),
        29 | 97 => "Ctrl".into(),
        56 | 100 => "Alt".into(),
        16 => "Q".into(),
        17 => "W".into(),
        18 => "E".into(),
        19 => "R".into(),
        20 => "T".into(),
        21 => "Y".into(),
        22 => "U".into(),
        23 => "I".into(),
        24 => "O".into(),
        25 => "P".into(),
        30 => "A".into(),
        31 => "S".into(),
        32 => "D".into(),
        33 => "F".into(),
        34 => "G".into(),
        35 => "H".into(),
        36 => "J".into(),
        37 => "K".into(),
        38 => "L".into(),
        44 => "Z".into(),
        45 => "X".into(),
        46 => "C".into(),
        47 => "V".into(),
        48 => "B".into(),
        49 => "N".into(),
        50 => "M".into(),
        2 => "1".into(),
        3 => "2".into(),
        4 => "3".into(),
        5 => "4".into(),
        6 => "5".into(),
        7 => "6".into(),
        8 => "7".into(),
        9 => "8".into(),
        10 => "9".into(),
        11 => "0".into(),
        41 => "`".into(),
        12 => "-".into(),
        13 => "=".into(),
        26 => "[".into(),
        27 => "]".into(),
        43 => "\\".into(),
        39 => ";".into(),
        40 => "'".into(),
        51 => ",".into(),
        52 => ".".into(),
        53 => "/".into(),
        other => format!("KEY {}", other),
    }
}

/// Open the config panel as a native window. Blocks until the user closes
/// it. Takes a shared LiveSettings so slider changes apply to the running
/// preview manager instantly.
///
/// NOTE: we deliberately do NOT force the winit X11 backend here. On
/// Wayland sessions winit picks Wayland and wgpu renders natively; forcing
/// X11 (XWayland) panics wgpu with "Invalid surface" on KWin.
pub fn run(config: Config, live: Arc<Mutex<LiveSettings>>) -> iced::Result {
    let icon =
        iced::window::icon::from_file_data(include_bytes!("../../assets/icon.png"), None).ok();

    iced::application(
        move || Panel::new(config.clone(), Arc::clone(&live)),
        update,
        view,
    )
    .title("Nicotine")
    .settings(iced::Settings {
        default_text_size: iced::Pixels(TEXT_SIZE),
        ..Default::default()
    })
    .theme(theme)
    .subscription(subscription)
    .default_font(iced::Font::with_name("JetBrains Mono"))
    .font(include_bytes!("../../assets/fonts/JetBrainsMono-Regular.ttf").as_slice())
    .font(include_bytes!("../../assets/fonts/Marlboro.ttf").as_slice())
    .window(iced::window::Settings {
        size: iced::Size::new(720.0, 680.0),
        min_size: Some(iced::Size::new(560.0, 420.0)),
        resizable: true,
        icon,
        ..Default::default()
    })
    .run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::keyboard::key::Named;
    use iced::keyboard::Key;

    fn f_keys_16_to_24() -> Vec<(Named, &'static str)> {
        vec![
            (Named::F16, "F16"),
            (Named::F17, "F17"),
            (Named::F18, "F18"),
            (Named::F19, "F19"),
            (Named::F20, "F20"),
            (Named::F21, "F21"),
            (Named::F22, "F22"),
            (Named::F23, "F23"),
            (Named::F24, "F24"),
        ]
    }

    #[test]
    fn iced_key_to_code_maps_f16_through_f24() {
        for (key, name) in f_keys_16_to_24() {
            assert!(
                iced_key_to_code(&Key::Named(key)).is_some(),
                "{name} has no native key code mapping"
            );
        }
    }

    #[test]
    fn code_to_label_round_trips_f16_through_f24() {
        for (key, name) in f_keys_16_to_24() {
            let code = iced_key_to_code(&Key::Named(key))
                .unwrap_or_else(|| panic!("{name} unmapped, can't round-trip"));
            assert_eq!(
                code_to_label(code),
                name,
                "code {code} for {name} renders as raw fallback instead of the F-key name"
            );
        }
    }

    #[test]
    fn character_keys_map_to_codes() {
        // A printable key arrives as Key::Character; ensure letters/digits map.
        assert!(iced_key_to_code(&Key::Character("a".into())).is_some());
        assert!(iced_key_to_code(&Key::Character("1".into())).is_some());
    }

    #[cfg(windows)]
    #[test]
    fn oem_vk_range_covers_all_documented_oem_vks() {
        let expected: Vec<u16> = (0xBAu16..=0xC0)
            .chain(0xDBu16..=0xDF)
            .chain([0xE2u16])
            .collect();
        assert_eq!(
            OEM_VK_RANGE,
            expected.as_slice(),
            "OEM_VK_RANGE drifted from the documented MSDN OEM range"
        );
    }
}
