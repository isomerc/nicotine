use crate::config::{Config, LiveSettings};
use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod ui;

/// After any config edit we wait this long with no further edits before
/// flushing to disk. 300ms is the sweet spot — saves feel instant when
/// the user taps a checkbox or clicks a binding, but slider drags and
/// text input coalesce into a single write rather than hammering disk
/// on every pixel / keystroke.
const AUTOSAVE_DEBOUNCE: Duration = Duration::from_millis(300);

/// Which config field is currently capturing a live keypress from the
/// panel. Only one can capture at a time; `None` means no capture.
/// `Character` carries the character name so per-character hotkey
/// bindings survive reorders of the characters list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum CaptureTarget {
    ForwardKey,
    BackwardKey,
    ModifierKey,
    Character(String),
}

/// Options for the per-character / main modifier dropdown.
/// Codes are platform-specific: Win32 VK on Windows, Linux evdev
/// keycodes (from <linux/input-event-codes.h>) on Linux. The Config
/// schema stores `u16` for both; the platform-specific code path
/// interprets it correctly.
#[cfg(windows)]
const MODIFIER_CHOICES: &[(Option<u16>, &str)] = &[
    (None, "None"),
    (Some(0x10), "Shift"),
    (Some(0x11), "Ctrl"),
    (Some(0x12), "Alt"),
];

#[cfg(unix)]
const MODIFIER_CHOICES: &[(Option<u16>, &str)] = &[
    (None, "None"),
    (Some(42), "Shift"), // KEY_LEFTSHIFT
    (Some(29), "Ctrl"),  // KEY_LEFTCTRL
    (Some(56), "Alt"),   // KEY_LEFTALT
];

/// Brand palette matching the existing Linux overlay.
const NICOTINE_RED: egui::Color32 = egui::Color32::from_rgb(196, 30, 58);
const NICOTINE_GOLD: egui::Color32 = egui::Color32::from_rgb(180, 155, 105);
const NICOTINE_CREAM: egui::Color32 = egui::Color32::from_rgb(252, 250, 242);
const NICOTINE_BLACK: egui::Color32 = egui::Color32::from_rgb(30, 30, 30);
/// Used only for the "LATEST VERSION" footer badge — chosen to read
/// clearly against cream while harmonizing with the warm palette.
const NICOTINE_GREEN: egui::Color32 = egui::Color32::from_rgb(60, 140, 70);

pub struct ConfigPanel {
    config: Config,
    /// Buffer for "add character" text input.
    new_character_buffer: String,
    /// Shared settings watched by the preview manager for live updates
    /// (resize windows while sliders are being dragged).
    live: Arc<Mutex<LiveSettings>>,
    /// When Some(...), the panel is listening for the next keypress /
    /// side-mouse click to bind it to the given field.
    capturing: Option<CaptureTarget>,
    /// Capture state from the previous frame. Used to detect edge
    /// transitions so we can pause the daemon's global hotkeys when
    /// the user enters capture mode (otherwise RegisterHotKey eats the
    /// key before egui can see it) and resume afterwards.
    last_capturing: Option<CaptureTarget>,
    /// Timestamp of the last config edit. When set and `AUTOSAVE_DEBOUNCE`
    /// has elapsed with no further edits, the panel flushes the config
    /// to disk. Kept as an Option so we can skip saving when nothing
    /// has changed since the last flush.
    last_change: Option<Instant>,
    /// Last inner-size we asked the OS viewport to be. Tracked so we
    /// only send a resize command when the measured content height
    /// actually changes — re-sending the same size every frame wastes
    /// work and can cause visual jitter.
    last_applied_height: f32,
}

impl ConfigPanel {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        config: Config,
        live: Arc<Mutex<LiveSettings>>,
    ) -> Self {
        // Load Nicotine's brand fonts so the header looks like the overlay.
        let mut fonts = egui::FontDefinitions::default();
        fonts.font_data.insert(
            "jetbrains_mono".to_owned(),
            egui::FontData::from_static(include_bytes!(
                "../../assets/fonts/JetBrainsMono-Regular.ttf"
            )),
        );
        fonts.font_data.insert(
            "logo_font".to_owned(),
            egui::FontData::from_static(include_bytes!("../../assets/fonts/Marlboro.ttf")),
        );
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "jetbrains_mono".to_owned());
        fonts
            .families
            .entry(egui::FontFamily::Name("logo".into()))
            .or_default()
            .push("logo_font".to_owned());
        cc.egui_ctx.set_fonts(fonts);

        // Nicotine-branded light theme. egui's default light visuals use
        // pale grays against our cream background, which makes hover /
        // active state changes basically invisible. Override with a
        // warmer palette so every interactive widget has a visible
        // idle / hover / pressed progression (cream → gold → red).
        cc.egui_ctx.set_visuals(build_visuals());

        Self {
            config,
            new_character_buffer: String::new(),
            live,
            capturing: None,
            last_capturing: None,
            last_change: None,
            last_applied_height: 0.0,
        }
    }

    /// Mark the config as edited. The next `update()` tick checks this
    /// timestamp and flushes to disk once the user has been idle for
    /// `AUTOSAVE_DEBOUNCE`.
    fn touch(&mut self) {
        self.last_change = Some(Instant::now());
    }
}

fn build_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::light();

    // Cream page / non-interactive surfaces.
    v.widgets.noninteractive.bg_fill = NICOTINE_CREAM;
    v.widgets.noninteractive.weak_bg_fill = NICOTINE_CREAM;
    v.widgets.noninteractive.fg_stroke.color = NICOTINE_BLACK;

    // Idle: slightly-off-cream so the widget is distinguishable from the
    // surrounding panel, with a gold-ish border.
    v.widgets.inactive.bg_fill = egui::Color32::from_rgb(240, 234, 218);
    v.widgets.inactive.weak_bg_fill = egui::Color32::from_rgb(244, 238, 224);
    v.widgets.inactive.fg_stroke.color = NICOTINE_BLACK;
    v.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, NICOTINE_GOLD);

    // Hover: strong gold — clearly different from idle so moving the
    // mouse over anything shows a visible change.
    v.widgets.hovered.bg_fill = NICOTINE_GOLD;
    v.widgets.hovered.weak_bg_fill = egui::Color32::from_rgb(228, 212, 176);
    v.widgets.hovered.fg_stroke.color = NICOTINE_BLACK;
    v.widgets.hovered.bg_stroke = egui::Stroke::new(1.5, NICOTINE_RED);

    // Pressed / active: Nicotine red with cream text.
    v.widgets.active.bg_fill = NICOTINE_RED;
    v.widgets.active.weak_bg_fill = egui::Color32::from_rgb(230, 176, 186);
    v.widgets.active.fg_stroke.color = NICOTINE_CREAM;
    v.widgets.active.bg_stroke = egui::Stroke::new(1.5, NICOTINE_RED);

    // Open popup / selected — e.g. radio selection, text edit focus.
    v.widgets.open.bg_fill = NICOTINE_GOLD;
    v.widgets.open.weak_bg_fill = egui::Color32::from_rgb(228, 212, 176);
    v.widgets.open.fg_stroke.color = NICOTINE_BLACK;
    v.widgets.open.bg_stroke = egui::Stroke::new(1.5, NICOTINE_RED);

    // Text selection highlight.
    v.selection.bg_fill = NICOTINE_RED.gamma_multiply(0.45);
    v.selection.stroke.color = NICOTINE_BLACK;

    // Hyperlinks / accents (rarely used here but keep the brand colour).
    v.hyperlink_color = NICOTINE_RED;

    v
}

impl eframe::App for ConfigPanel {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- Capture mode: listen for the next keypress ----
        // Runs before any widget draw so the event stream we inspect
        // reflects what the user just did.
        if let Some(target) = self.capturing.clone() {
            if let Some(vk) = captured_binding(ctx) {
                match &target {
                    CaptureTarget::ForwardKey => self.config.forward_key = vk,
                    CaptureTarget::BackwardKey => self.config.backward_key = vk,
                    CaptureTarget::ModifierKey => self.config.modifier_key = Some(vk),
                    CaptureTarget::Character(name) => {
                        // Preserve the existing modifier if already set,
                        // otherwise default to no modifier.
                        let modifier = self
                            .config
                            .character_hotkeys
                            .get(name)
                            .and_then(|h| h.modifier);
                        self.config.character_hotkeys.insert(
                            name.clone(),
                            crate::config::CharacterHotkey { vk, modifier },
                        );
                    }
                }
                self.capturing = None;
                self.touch();
            } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                // Escape cancels capture without binding.
                self.capturing = None;
            }
            // Keep requesting frames so a key press lands even when the
            // user isn't hovering over the panel.
            ctx.request_repaint();
        }

        // Edge-detect capture start/end so we can pause the daemon's
        // global hotkeys — otherwise RegisterHotKey swallows F10/F11
        // before egui sees them, and binding appears broken.
        if self.last_capturing != self.capturing {
            #[cfg(windows)]
            {
                if self.last_capturing.is_none() && self.capturing.is_some() {
                    crate::windows_input::pause_hotkeys();
                } else if self.last_capturing.is_some() && self.capturing.is_none() {
                    // Flush config.toml synchronously before resuming so
                    // the listener's Config::load() sees the new binding.
                    if self.last_change.is_some() {
                        let _ = self.config.save();
                        self.last_change = None;
                    }
                    crate::windows_input::resume_hotkeys();
                }
            }
            self.last_capturing = self.capturing.clone();
        }

        // ---- Branded header strip ----
        egui::TopBottomPanel::top("nicotine_header")
            .exact_height(72.0)
            .frame(
                egui::Frame::none()
                    .fill(NICOTINE_RED)
                    // Asymmetric vertical margin: the Marlboro font's
                    // glyph box has more descent than ascent, so a
                    // geometrically-centered layout reads as "logo too
                    // high." Bumping the top margin shifts the visual
                    // center down by a few pixels.
                    .inner_margin(egui::Margin {
                        left: 0.0,
                        right: 0.0,
                        top: 6.0,
                        bottom: 0.0,
                    }),
            )
            .show(ctx, |ui| {
                ui.with_layout(
                    egui::Layout::centered_and_justified(egui::Direction::TopDown),
                    |ui| {
                        ui.label(
                            egui::RichText::new("Nicotine")
                                .family(egui::FontFamily::Name("logo".into()))
                                .size(48.0)
                                .color(NICOTINE_CREAM),
                        );
                    },
                );
            });

        // ---- Branded footer with external links ----
        egui::TopBottomPanel::bottom("nicotine_footer")
            .exact_height(40.0)
            .frame(
                egui::Frame::none()
                    .fill(NICOTINE_CREAM)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0))
                    .stroke(egui::Stroke::new(1.0, NICOTINE_GOLD)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Explicit .color() on both RichText blocks — egui's
                    // hyperlink color doesn't always propagate through
                    // .strong() in 0.29, leaving the text near-invisible
                    // against the cream background.
                    ui.hyperlink_to(
                        egui::RichText::new("GITHUB").strong().color(NICOTINE_RED),
                        "https://github.com/isomerc",
                    );
                    ui.add_space(14.0);
                    ui.colored_label(NICOTINE_GOLD, "•");
                    ui.add_space(14.0);
                    ui.hyperlink_to(
                        egui::RichText::new("ILLUMINATED IS RECRUITING")
                            .strong()
                            .color(NICOTINE_RED),
                        "https://www.illuminatedcorp.com",
                    );

                    // Right-aligned update badge. `right_to_left`
                    // consumes the remaining horizontal space and lays
                    // out items from the right edge so this lands in
                    // the bottom-right corner regardless of panel width.
                    // Three states:
                    //   - `Outdated` → red "NEW VERSION AVAILABLE" link
                    //     to the GitHub release page
                    //   - `UpToDate` → green "LATEST VERSION" label
                    //   - `None` (check pending or failed) → render
                    //     nothing so we don't show stale claims
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match crate::version_check::get_update_status() {
                            Some(crate::version_check::UpdateStatus::Outdated { version, url }) => {
                                ui.hyperlink_to(
                                    egui::RichText::new(format!(
                                        "NEW VERSION AVAILABLE (v{})",
                                        version
                                    ))
                                    .strong()
                                    .color(NICOTINE_RED),
                                    url,
                                );
                            }
                            Some(crate::version_check::UpdateStatus::UpToDate) => {
                                ui.label(
                                    egui::RichText::new("LATEST VERSION")
                                        .strong()
                                        .color(NICOTINE_GREEN),
                                );
                            }
                            None => {}
                        }
                    });
                });
            });

        // ---- Body ----
        // Capture the central panel's content height from inside its
        // builder so we can size the window to it — `ctx.used_size()`
        // only reports what was *allocated* to the CentralPanel, which
        // is bounded by header+footer, so tall content would clip and
        // paint over the footer without this measurement.
        const HEADER_HEIGHT: f32 = 72.0;
        const FOOTER_HEIGHT: f32 = 40.0;
        const CENTRAL_V_MARGIN: f32 = 12.0;
        let mut central_content_height = 0.0f32;
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(NICOTINE_CREAM)
                    .inner_margin(egui::Margin::symmetric(16.0, CENTRAL_V_MARGIN)),
            )
            .show(ctx, |ui| {
                self.draw_display_mode_section(ui);
                ui.add_space(20.0);
                self.draw_characters_section(ui);
                ui.add_space(20.0);
                self.draw_hotkeys_section(ui);
                ui.add_space(20.0);
                self.draw_previews_section(ui);
                central_content_height = ui.min_rect().height();
            });

        // ---- Auto-size the window to fit the rendered content. ----
        let target_height =
            (HEADER_HEIGHT + FOOTER_HEIGHT + CENTRAL_V_MARGIN * 2.0 + central_content_height)
                .round()
                .clamp(300.0, 1500.0);
        if (target_height - self.last_applied_height).abs() > 1.0 {
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                600.0,
                target_height,
            )));
            self.last_applied_height = target_height;
        }

        // ---- Debounced auto-save ----
        // After the user has been idle for AUTOSAVE_DEBOUNCE, flush the
        // current config to disk. If they're actively editing (every
        // touch() resets last_change to "now"), we keep deferring; as
        // soon as they stop, the next tick saves. request_repaint_after
        // ensures we get a frame to actually perform the save even
        // when there's no other input.
        if let Some(changed_at) = self.last_change {
            let elapsed = changed_at.elapsed();
            if elapsed >= AUTOSAVE_DEBOUNCE {
                if let Err(e) = self.config.save() {
                    eprintln!("config autosave failed: {}", e);
                }
                self.last_change = None;
            } else {
                ctx.request_repaint_after(AUTOSAVE_DEBOUNCE - elapsed);
            }
        }
    }
}

/// All egui keys we're willing to bind, in the order we poll them.
/// Using `key_pressed` polling here (instead of matching Event::Key in
/// the event stream) is more reliable when a widget — like the bind
/// button the user just clicked — has focus: egui may consume some
/// keys before they surface as generic events, but `key_pressed` sees
/// the edge regardless.
const SUPPORTED_KEYS: &[egui::Key] = &[
    egui::Key::F1,
    egui::Key::F2,
    egui::Key::F3,
    egui::Key::F4,
    egui::Key::F5,
    egui::Key::F6,
    egui::Key::F7,
    egui::Key::F8,
    egui::Key::F9,
    egui::Key::F10,
    egui::Key::F11,
    egui::Key::F12,
    egui::Key::F13,
    egui::Key::F14,
    egui::Key::F15,
    egui::Key::Tab,
    egui::Key::Space,
    egui::Key::Enter,
    egui::Key::Backspace,
    egui::Key::Insert,
    egui::Key::Delete,
    egui::Key::Home,
    egui::Key::End,
    egui::Key::PageUp,
    egui::Key::PageDown,
    egui::Key::ArrowUp,
    egui::Key::ArrowDown,
    egui::Key::ArrowLeft,
    egui::Key::ArrowRight,
    egui::Key::A,
    egui::Key::B,
    egui::Key::C,
    egui::Key::D,
    egui::Key::E,
    egui::Key::F,
    egui::Key::G,
    egui::Key::H,
    egui::Key::I,
    egui::Key::J,
    egui::Key::K,
    egui::Key::L,
    egui::Key::M,
    egui::Key::N,
    egui::Key::O,
    egui::Key::P,
    egui::Key::Q,
    egui::Key::R,
    egui::Key::S,
    egui::Key::T,
    egui::Key::U,
    egui::Key::V,
    egui::Key::W,
    egui::Key::X,
    egui::Key::Y,
    egui::Key::Z,
    egui::Key::Num0,
    egui::Key::Num1,
    egui::Key::Num2,
    egui::Key::Num3,
    egui::Key::Num4,
    egui::Key::Num5,
    egui::Key::Num6,
    egui::Key::Num7,
    egui::Key::Num8,
    egui::Key::Num9,
    egui::Key::Backtick,
    egui::Key::Minus,
    egui::Key::Equals,
    egui::Key::OpenBracket,
    egui::Key::CloseBracket,
    egui::Key::Backslash,
    egui::Key::Semicolon,
    egui::Key::Quote,
    egui::Key::Comma,
    egui::Key::Period,
    egui::Key::Slash,
];

/// Poll egui for the first bindable key press this frame. Returns the
/// platform-native key code to bind, or None if no eligible press
/// happened. The code is a Win32 VK on Windows / a Linux evdev keycode
/// on Linux — same semantics the daemon's hotkey listener expects.
fn captured_binding(ctx: &egui::Context) -> Option<u16> {
    ctx.input(|i| {
        for key in SUPPORTED_KEYS {
            if *key == egui::Key::Escape {
                continue;
            }
            if i.key_pressed(*key) {
                return egui_key_to_code(*key);
            }
        }
        None
    })
}

/// Map an egui Key to the Windows Virtual-Key code. Returns None for
/// keys without a standard VK_ (exotic IME / media keys).
#[cfg(windows)]
fn egui_key_to_code(key: egui::Key) -> Option<u16> {
    use egui::Key;
    let vk: u32 = match key {
        Key::F1 => 0x70,
        Key::F2 => 0x71,
        Key::F3 => 0x72,
        Key::F4 => 0x73,
        Key::F5 => 0x74,
        Key::F6 => 0x75,
        Key::F7 => 0x76,
        Key::F8 => 0x77,
        Key::F9 => 0x78,
        Key::F10 => 0x79,
        Key::F11 => 0x7A,
        Key::F12 => 0x7B,
        Key::F13 => 0x7C,
        Key::F14 => 0x7D,
        Key::F15 => 0x7E,
        Key::Tab => 0x09,
        Key::Space => 0x20,
        Key::Enter => 0x0D,
        Key::Backspace => 0x08,
        Key::Insert => 0x2D,
        Key::Delete => 0x2E,
        Key::Home => 0x24,
        Key::End => 0x23,
        Key::PageUp => 0x21,
        Key::PageDown => 0x22,
        Key::ArrowUp => 0x26,
        Key::ArrowDown => 0x28,
        Key::ArrowLeft => 0x25,
        Key::ArrowRight => 0x27,
        Key::A => 0x41,
        Key::B => 0x42,
        Key::C => 0x43,
        Key::D => 0x44,
        Key::E => 0x45,
        Key::F => 0x46,
        Key::G => 0x47,
        Key::H => 0x48,
        Key::I => 0x49,
        Key::J => 0x4A,
        Key::K => 0x4B,
        Key::L => 0x4C,
        Key::M => 0x4D,
        Key::N => 0x4E,
        Key::O => 0x4F,
        Key::P => 0x50,
        Key::Q => 0x51,
        Key::R => 0x52,
        Key::S => 0x53,
        Key::T => 0x54,
        Key::U => 0x55,
        Key::V => 0x56,
        Key::W => 0x57,
        Key::X => 0x58,
        Key::Y => 0x59,
        Key::Z => 0x5A,
        Key::Num0 => 0x30,
        Key::Num1 => 0x31,
        Key::Num2 => 0x32,
        Key::Num3 => 0x33,
        Key::Num4 => 0x34,
        Key::Num5 => 0x35,
        Key::Num6 => 0x36,
        Key::Num7 => 0x37,
        Key::Num8 => 0x38,
        Key::Num9 => 0x39,
        Key::Backtick => 0xC0,
        Key::Minus => 0xBD,
        Key::Equals => 0xBB,
        Key::OpenBracket => 0xDB,
        Key::CloseBracket => 0xDD,
        Key::Backslash => 0xDC,
        Key::Semicolon => 0xBA,
        Key::Quote => 0xDE,
        Key::Comma => 0xBC,
        Key::Period => 0xBE,
        Key::Slash => 0xBF,
        _ => return None,
    };
    Some(vk as u16)
}

/// Map an egui Key to the Linux evdev keycode (from
/// <linux/input-event-codes.h>). The mouse_listener / keyboard_listener
/// modules read evdev codes directly, so this is what Config stores on
/// Linux.
#[cfg(unix)]
fn egui_key_to_code(key: egui::Key) -> Option<u16> {
    use egui::Key;
    let code: u16 = match key {
        // Function keys: KEY_F1 = 59, KEY_F2 = 60, ... KEY_F10 = 68;
        // KEY_F11 = 87, KEY_F12 = 88. F13–F15 are 183–185.
        Key::F1 => 59,
        Key::F2 => 60,
        Key::F3 => 61,
        Key::F4 => 62,
        Key::F5 => 63,
        Key::F6 => 64,
        Key::F7 => 65,
        Key::F8 => 66,
        Key::F9 => 67,
        Key::F10 => 68,
        Key::F11 => 87,
        Key::F12 => 88,
        Key::F13 => 183,
        Key::F14 => 184,
        Key::F15 => 185,
        Key::Tab => 15,
        Key::Space => 57,
        Key::Enter => 28,
        Key::Backspace => 14,
        Key::Insert => 110,
        Key::Delete => 111,
        Key::Home => 102,
        Key::End => 107,
        Key::PageUp => 104,
        Key::PageDown => 109,
        Key::ArrowUp => 103,
        Key::ArrowDown => 108,
        Key::ArrowLeft => 105,
        Key::ArrowRight => 106,
        // KEY_A..KEY_Z are NOT alphabetical: keyboard scancode order
        // (Q W E R T Y U I O P ...). Below tracks <linux/input-event-codes.h>.
        Key::Q => 16,
        Key::W => 17,
        Key::E => 18,
        Key::R => 19,
        Key::T => 20,
        Key::Y => 21,
        Key::U => 22,
        Key::I => 23,
        Key::O => 24,
        Key::P => 25,
        Key::A => 30,
        Key::S => 31,
        Key::D => 32,
        Key::F => 33,
        Key::G => 34,
        Key::H => 35,
        Key::J => 36,
        Key::K => 37,
        Key::L => 38,
        Key::Z => 44,
        Key::X => 45,
        Key::C => 46,
        Key::V => 47,
        Key::B => 48,
        Key::N => 49,
        Key::M => 50,
        // Number row: KEY_1..KEY_0 = 2..11.
        Key::Num1 => 2,
        Key::Num2 => 3,
        Key::Num3 => 4,
        Key::Num4 => 5,
        Key::Num5 => 6,
        Key::Num6 => 7,
        Key::Num7 => 8,
        Key::Num8 => 9,
        Key::Num9 => 10,
        Key::Num0 => 11,
        Key::Backtick => 41,    // KEY_GRAVE
        Key::Minus => 12,       // KEY_MINUS
        Key::Equals => 13,      // KEY_EQUAL
        Key::OpenBracket => 26, // KEY_LEFTBRACE
        Key::CloseBracket => 27,
        Key::Backslash => 43,
        Key::Semicolon => 39,
        Key::Quote => 40,
        Key::Comma => 51,
        Key::Period => 52,
        Key::Slash => 53,
        _ => return None,
    };
    Some(code)
}

/// Human label for a Win32 VK code, used on the bind button.
#[cfg(windows)]
fn code_to_label(vk: u16) -> String {
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
        0x09 => "Tab".into(),
        0x20 => "Space".into(),
        0x0D => "Enter".into(),
        0x08 => "Backspace".into(),
        0x1B => "Escape".into(),
        0x10 | 0xA0 | 0xA1 => "Shift".into(),
        0x11 | 0xA2 | 0xA3 => "Ctrl".into(),
        0x12 | 0xA4 | 0xA5 => "Alt".into(),
        0xC0 => "`".into(),
        0x30..=0x39 => format!("{}", (vk - 0x30) as u8 as char),
        0x41..=0x5A => format!("{}", vk as u8 as char),
        0x26 => "Up".into(),
        0x28 => "Down".into(),
        0x25 => "Left".into(),
        0x27 => "Right".into(),
        other => format!("VK 0x{:02X}", other),
    }
}

/// Human label for a Linux evdev keycode. Used on the bind button.
/// Codes from <linux/input-event-codes.h>.
#[cfg(unix)]
fn code_to_label(code: u16) -> String {
    match code {
        // Function keys.
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
        // Common control keys.
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
        // Modifiers (left + right variants, evdev distinguishes them).
        42 | 54 => "Shift".into(),
        29 | 97 => "Ctrl".into(),
        56 | 100 => "Alt".into(),
        // Letters: not contiguous in evdev (scancode order).
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
        // Number row: KEY_1..KEY_0 = 2..11.
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
        // Punctuation.
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

/// Open the config panel as a top-level window. Blocks until the user
/// closes the window. Takes a shared LiveSettings so slider changes can
/// be applied to the running preview manager instantly.
pub fn run(config: Config, live: Arc<Mutex<LiveSettings>>) -> Result<(), eframe::Error> {
    // Load the Nicotine icon for the window chrome + taskbar + alt-tab.
    // Baked into the binary via include_bytes so there's no external
    // asset to lose on install. from_png_bytes goes through eframe's
    // bundled `image` crate (already pulled in with the png feature).
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../../assets/icon.png"))
        .expect("failed to decode embedded icon.png");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Open at the empty-config size; the per-frame auto-resize
            // grows the window as the user adds characters. Starting at
            // a tall fixed value (e.g. 1000pt) caused huge dead space on
            // first launch on machines where the OS ignores
            // ViewportCommand::InnerSize *shrinks* on a non-resizable
            // window — the window would never shrink back from the
            // initial size to fit the (much shorter) empty content.
            // Growing reliably works everywhere, so we start small.
            .with_inner_size([600.0, 640.0])
            .with_resizable(false)
            .with_title("Nicotine")
            .with_icon(icon),
        ..Default::default()
    };

    // On Linux, force winit's X11 backend. Previews use X11 directly
    // (XComposite + XRender), so the panel needs to live on the same
    // X server. Without this, winit picks Wayland on Wayland sessions
    // and the panel can't share state/focus with our X11 previews. It
    // also dodges the libwayland-client dlopen failure when the binary
    // is run in environments that don't have it on the loader path.
    #[cfg(unix)]
    let options = {
        use winit::platform::x11::EventLoopBuilderExtX11;
        let mut options = options;
        options.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
        options
    };

    eframe::run_native(
        "Nicotine",
        options,
        Box::new(move |cc| Ok(Box::new(ConfigPanel::new(cc, config, live)))),
    )
}
