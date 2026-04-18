use crate::config::{Config, DisplayMode, LiveSettings};
use eframe::egui;
use std::sync::{Arc, Mutex};

/// Which config field is currently capturing a live keypress/click from
/// the panel. Only one can capture at a time; `None` means no capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureTarget {
    ForwardKey,
    BackwardKey,
    ModifierKey,
    ForwardButton,
    BackwardButton,
}

/// Brand palette matching the existing Linux overlay.
const NICOTINE_RED: egui::Color32 = egui::Color32::from_rgb(196, 30, 58);
const NICOTINE_GOLD: egui::Color32 = egui::Color32::from_rgb(180, 155, 105);
const NICOTINE_CREAM: egui::Color32 = egui::Color32::from_rgb(252, 250, 242);
const NICOTINE_BLACK: egui::Color32 = egui::Color32::from_rgb(30, 30, 30);

pub struct ConfigPanel {
    config: Config,
    /// Tracks whether the working copy differs from the on-disk version.
    dirty: bool,
    /// Buffer for "add character" text input.
    new_character_buffer: String,
    /// Toast-style status message shown briefly after save/reload.
    status: Option<String>,
    /// Shared settings watched by the preview manager for live updates
    /// (resize windows while sliders are being dragged).
    live: Arc<Mutex<LiveSettings>>,
    /// When Some(...), the panel is listening for the next keypress /
    /// side-mouse click to bind it to the given field.
    capturing: Option<CaptureTarget>,
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
                "../assets/fonts/JetBrainsMono-Regular.ttf"
            )),
        );
        fonts.font_data.insert(
            "logo_font".to_owned(),
            egui::FontData::from_static(include_bytes!("../assets/fonts/Marlboro.ttf")),
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
            dirty: false,
            new_character_buffer: String::new(),
            status: None,
            live,
            capturing: None,
        }
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
        // ---- Capture mode: listen for the next keypress / x-button ----
        // Runs before any widget draw so the event stream we inspect
        // reflects what the user just did.
        if let Some(target) = self.capturing {
            if let Some(vk) = captured_binding(ctx, target) {
                match target {
                    CaptureTarget::ForwardKey => self.config.forward_key = vk,
                    CaptureTarget::BackwardKey => self.config.backward_key = vk,
                    CaptureTarget::ModifierKey => self.config.modifier_key = Some(vk),
                    CaptureTarget::ForwardButton => self.config.forward_button = vk,
                    CaptureTarget::BackwardButton => self.config.backward_button = vk,
                }
                self.capturing = None;
                self.dirty = true;
            } else if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
                // Escape cancels capture without binding.
                self.capturing = None;
            }
            // Keep requesting frames so a key press lands even when the
            // user isn't hovering over the panel.
            ctx.request_repaint();
        }

        // ---- Branded header strip ----
        egui::TopBottomPanel::top("nicotine_header")
            .exact_height(72.0)
            .frame(
                egui::Frame::none()
                    .fill(NICOTINE_RED)
                    .inner_margin(egui::Margin::same(0.0)),
            )
            .show(ctx, |ui| {
                // Center the logo both horizontally AND vertically in the
                // red strip. `centered_and_justified` handles the vertical
                // centering so we don't have to hand-tune add_space based
                // on the Marlboro font's internal glyph padding.
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

        // ---- Status toast / footer ----
        egui::TopBottomPanel::bottom("nicotine_footer")
            .exact_height(48.0)
            .frame(
                egui::Frame::none()
                    .fill(NICOTINE_CREAM)
                    .inner_margin(egui::Margin::symmetric(16.0, 8.0))
                    .stroke(egui::Stroke::new(1.0, NICOTINE_GOLD)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let save = egui::Button::new(
                        egui::RichText::new("SAVE")
                            .color(NICOTINE_CREAM)
                            .size(13.0)
                            .strong(),
                    )
                    .fill(NICOTINE_RED)
                    .rounding(2.0);
                    if ui.add_sized([100.0, 28.0], save).clicked() {
                        match self.config.save() {
                            Ok(_) => {
                                self.dirty = false;
                                self.status =
                                    Some("✓ Saved — daemon will pick up changes".to_string());
                            }
                            Err(e) => self.status = Some(format!("✗ Save failed: {}", e)),
                        }
                    }

                    ui.add_space(8.0);
                    let reload =
                        egui::Button::new(egui::RichText::new("RELOAD").color(NICOTINE_BLACK))
                            .fill(NICOTINE_GOLD)
                            .rounding(2.0);
                    if ui.add_sized([100.0, 28.0], reload).clicked() {
                        match Config::load() {
                            Ok(cfg) => {
                                self.config = cfg;
                                self.dirty = false;
                                self.status = Some("✓ Reloaded from disk".to_string());
                            }
                            Err(e) => self.status = Some(format!("✗ Reload failed: {}", e)),
                        }
                    }

                    ui.add_space(16.0);
                    if let Some(msg) = &self.status {
                        ui.colored_label(NICOTINE_BLACK, msg);
                    } else if self.dirty {
                        ui.colored_label(NICOTINE_RED, "Unsaved changes");
                    }
                });
            });

        // ---- Body ----
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(NICOTINE_CREAM)
                    .inner_margin(egui::Margin::symmetric(16.0, 12.0)),
            )
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.draw_display_mode_section(ui);
                    ui.add_space(20.0);
                    self.draw_characters_section(ui);
                    ui.add_space(20.0);
                    self.draw_hotkeys_section(ui);
                    ui.add_space(20.0);
                    self.draw_mouse_section(ui);
                    ui.add_space(20.0);
                    self.draw_previews_section(ui);
                });
            });
    }
}

impl ConfigPanel {
    fn draw_section_header(ui: &mut egui::Ui, label: &str) {
        ui.label(
            egui::RichText::new(label)
                .size(16.0)
                .strong()
                .color(NICOTINE_RED),
        );
        ui.separator();
    }

    fn draw_display_mode_section(&mut self, ui: &mut egui::Ui) {
        Self::draw_section_header(ui, "Display Mode");
        ui.label(
            egui::RichText::new(
                "How Nicotine shows your running clients on screen. \
                 Preview windows mirror each client live; the list view is \
                 a compact always-on-top window of names.",
            )
            .size(11.0)
            .color(NICOTINE_BLACK),
        );
        ui.add_space(4.0);

        let prev = self.config.display_mode;
        ui.horizontal(|ui| {
            ui.radio_value(
                &mut self.config.display_mode,
                DisplayMode::Previews,
                "Preview windows",
            );
            ui.add_space(12.0);
            ui.radio_value(
                &mut self.config.display_mode,
                DisplayMode::List,
                "Client list",
            );
        });
        if self.config.display_mode != prev {
            self.dirty = true;
            // Push immediately to the shared LiveSettings so the preview
            // manager swaps modes within its next reconcile tick.
            self.live.lock().unwrap().display_mode = self.config.display_mode;
        }
    }

    fn draw_characters_section(&mut self, ui: &mut egui::Ui) {
        Self::draw_section_header(ui, "Cycle Order");
        ui.label(
            egui::RichText::new(
                "Characters cycle in the order shown. Names must match EVE's window title \
                 exactly (the part after \"EVE - \").",
            )
            .size(11.0)
            .color(NICOTINE_BLACK),
        );
        ui.add_space(6.0);

        let mut swap: Option<(usize, usize)> = None;
        let mut remove: Option<usize> = None;

        for (idx, name) in self.config.characters.iter_mut().enumerate() {
            ui.horizontal(|ui| {
                ui.label(format!("{}.", idx + 1));
                if ui.text_edit_singleline(name).changed() {
                    self.dirty = true;
                }
                if ui.button("↑").clicked() && idx > 0 {
                    swap = Some((idx, idx - 1));
                }
                if ui.button("↓").clicked() {
                    swap = Some((idx, idx + 1));
                }
                if ui.button("✕").clicked() {
                    remove = Some(idx);
                }
            });
        }

        if let Some((a, b)) = swap {
            if b < self.config.characters.len() {
                self.config.characters.swap(a, b);
                self.dirty = true;
            }
        }
        if let Some(idx) = remove {
            self.config.characters.remove(idx);
            self.dirty = true;
        }

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.label("Add:");
            let response = ui.text_edit_singleline(&mut self.new_character_buffer);
            let add_clicked = ui.button("+").clicked();
            let enter_pressed =
                response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (add_clicked || enter_pressed) && !self.new_character_buffer.trim().is_empty() {
                self.config
                    .characters
                    .push(self.new_character_buffer.trim().to_string());
                self.new_character_buffer.clear();
                self.dirty = true;
            }
        });
    }

    fn draw_hotkeys_section(&mut self, ui: &mut egui::Ui) {
        Self::draw_section_header(ui, "Keyboard Hotkeys");

        let prev_enable = self.config.enable_keyboard_buttons;
        ui.checkbox(
            &mut self.config.enable_keyboard_buttons,
            "Enable keyboard cycling",
        );
        if self.config.enable_keyboard_buttons != prev_enable {
            self.dirty = true;
        }

        ui.add_enabled_ui(self.config.enable_keyboard_buttons, |ui| {
            ui.horizontal(|ui| {
                ui.label("Forward:");
                self.draw_bind_button(
                    ui,
                    CaptureTarget::ForwardKey,
                    vk_to_label(self.config.forward_key),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Backward:");
                self.draw_bind_button(
                    ui,
                    CaptureTarget::BackwardKey,
                    vk_to_label(self.config.backward_key),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Modifier:");
                let label = match self.config.modifier_key {
                    Some(vk) => vk_to_label(vk),
                    None => "None".to_string(),
                };
                self.draw_bind_button(ui, CaptureTarget::ModifierKey, label);
                if self.config.modifier_key.is_some() && ui.button("Clear").clicked() {
                    self.config.modifier_key = None;
                    self.dirty = true;
                }
            });
            ui.label(
                egui::RichText::new(
                    "Click a binding to record the next key you press. Esc cancels. \
                     Set both keys to the same value with a modifier to cycle backward \
                     via modifier+key (e.g. Tab + Shift+Tab).",
                )
                .size(10.0)
                .color(NICOTINE_BLACK),
            );
        });
    }

    fn draw_mouse_section(&mut self, ui: &mut egui::Ui) {
        Self::draw_section_header(ui, "Mouse Side Buttons");
        let prev = self.config.enable_mouse_buttons;
        ui.checkbox(
            &mut self.config.enable_mouse_buttons,
            "Enable mouse cycling",
        );
        if self.config.enable_mouse_buttons != prev {
            self.dirty = true;
        }
        ui.add_enabled_ui(self.config.enable_mouse_buttons, |ui| {
            ui.label(
                egui::RichText::new(
                    "Click a binding then press the mouse side button you want to use. \
                     Esc cancels. Most mice expose XBUTTON1 (back) and XBUTTON2 (forward).",
                )
                .size(10.0)
                .color(NICOTINE_BLACK),
            );
            ui.horizontal(|ui| {
                ui.label("Forward button:");
                self.draw_bind_button(
                    ui,
                    CaptureTarget::ForwardButton,
                    xbutton_to_label(self.config.forward_button),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Backward button:");
                self.draw_bind_button(
                    ui,
                    CaptureTarget::BackwardButton,
                    xbutton_to_label(self.config.backward_button),
                );
            });
        });
    }

    /// Button that toggles capture for a given config field. When
    /// capturing, shows a hint; otherwise shows the current binding's
    /// label. Click while already capturing to cancel.
    fn draw_bind_button(&mut self, ui: &mut egui::Ui, target: CaptureTarget, label: String) {
        let is_capturing = self.capturing == Some(target);
        let text = if is_capturing {
            "[press key / button — Esc to cancel]".to_string()
        } else {
            label
        };
        // When idle, let the theme drive hover colors. When capturing,
        // lock the fill to gold + red stroke as a loud "waiting for
        // input" indicator.
        let mut button = egui::Button::new(text).min_size(egui::vec2(200.0, 22.0));
        if is_capturing {
            button = button
                .fill(NICOTINE_GOLD)
                .stroke(egui::Stroke::new(1.5, NICOTINE_RED));
        }
        if ui.add(button).clicked() {
            self.capturing = if is_capturing { None } else { Some(target) };
        }
    }

    fn draw_previews_section(&mut self, ui: &mut egui::Ui) {
        Self::draw_section_header(ui, "Preview Windows");
        let prev_show = self.config.show_previews;
        ui.checkbox(&mut self.config.show_previews, "Show preview windows");
        if self.config.show_previews != prev_show {
            self.dirty = true;
        }
        ui.add_enabled_ui(self.config.show_previews, |ui| {
            let prev_w = self.config.preview_width;
            let prev_h = self.config.preview_height;
            ui.horizontal(|ui| {
                ui.label("Width:");
                ui.add(
                    egui::Slider::new(&mut self.config.preview_width, 120..=800)
                        .suffix(" px")
                        .smart_aim(false)
                        .step_by(1.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Height:");
                ui.add(
                    egui::Slider::new(&mut self.config.preview_height, 80..=600)
                        .suffix(" px")
                        .smart_aim(false)
                        .step_by(1.0),
                );
            });
            if self.config.preview_width != prev_w || self.config.preview_height != prev_h {
                self.dirty = true;
                // Push the new size to the shared LiveSettings so the
                // preview manager resizes its windows on the next tick —
                // no need to wait for Save + hot-reload.
                let mut live = self.live.lock().unwrap();
                live.preview_width = self.config.preview_width;
                live.preview_height = self.config.preview_height;
            }
        });
    }
}

/// Given a target, poll egui's input events for the right kind of
/// press and return the Win32 VK or XBUTTON code to bind. Returns None
/// when nothing relevant happened this frame.
fn captured_binding(ctx: &egui::Context, target: CaptureTarget) -> Option<u16> {
    ctx.input(|i| {
        for event in &i.events {
            match (target, event) {
                // Keyboard targets: first non-Escape keypress wins.
                (
                    CaptureTarget::ForwardKey
                    | CaptureTarget::BackwardKey
                    | CaptureTarget::ModifierKey,
                    egui::Event::Key {
                        key,
                        pressed: true,
                        repeat: false,
                        ..
                    },
                ) if *key != egui::Key::Escape => {
                    return egui_key_to_vk(*key);
                }
                // Mouse-button targets: only XBUTTON1/XBUTTON2 count.
                (
                    CaptureTarget::ForwardButton | CaptureTarget::BackwardButton,
                    egui::Event::PointerButton {
                        button,
                        pressed: true,
                        ..
                    },
                ) => {
                    return match button {
                        egui::PointerButton::Extra1 => Some(1),
                        egui::PointerButton::Extra2 => Some(2),
                        _ => None,
                    };
                }
                _ => {}
            }
        }
        None
    })
}

/// Map an egui Key to the Windows Virtual-Key code. Returns None for
/// keys that don't have a standard VK_ (mostly exotic IME / media keys
/// we don't care about binding for cycling).
fn egui_key_to_vk(key: egui::Key) -> Option<u16> {
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

/// Human label for a Win32 VK code, used on the bind button.
fn vk_to_label(vk: u16) -> String {
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

/// Human label for the two Windows X-buttons.
fn xbutton_to_label(b: u16) -> String {
    match b {
        1 => "XBUTTON1 (back)".into(),
        2 => "XBUTTON2 (forward)".into(),
        other => format!("XBUTTON {}", other),
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
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/icon.png"))
        .expect("failed to decode embedded icon.png");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Fixed-size window sized to show every section end-to-end
            // without a scrollbar. Not resizable — this is a config
            // panel, not a document viewer, and dialogs feel more
            // intentional when they don't wiggle.
            .with_inner_size([600.0, 1000.0])
            .with_resizable(false)
            .with_title("Nicotine")
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        "Nicotine",
        options,
        Box::new(move |cc| Ok(Box::new(ConfigPanel::new(cc, config, live)))),
    )
}
