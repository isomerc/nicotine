use crate::config::{Config, DisplayMode, LiveSettings};
use eframe::egui;
use std::sync::{Arc, Mutex};

/// Curated dropdown of common cycle-hotkey choices. Values are Win32 VK
/// codes on Windows; on Linux they're stored as evdev codes but the panel
/// is currently Windows-facing so we use VK_* labels.
const VK_CHOICES: &[(u16, &str)] = &[
    (0x70, "F1"),
    (0x71, "F2"),
    (0x72, "F3"),
    (0x73, "F4"),
    (0x74, "F5"),
    (0x75, "F6"),
    (0x76, "F7"),
    (0x77, "F8"),
    (0x78, "F9"),
    (0x79, "F10"),
    (0x7A, "F11"),
    (0x7B, "F12"),
    (0x09, "Tab"),
    (0x20, "Space"),
    (0xC0, "` (backtick)"),
];

const MODIFIER_CHOICES: &[(Option<u16>, &str)] = &[
    (None, "None"),
    (Some(0x10), "Shift"),
    (Some(0x11), "Ctrl"),
    (Some(0x12), "Alt"),
];

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

        Self {
            config,
            dirty: false,
            new_character_buffer: String::new(),
            status: None,
            live,
        }
    }
}

impl eframe::App for ConfigPanel {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // ---- Branded header strip ----
        egui::TopBottomPanel::top("nicotine_header")
            .exact_height(72.0)
            .frame(
                egui::Frame::none()
                    .fill(NICOTINE_RED)
                    .inner_margin(egui::Margin::same(0.0)),
            )
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.add_space(8.0);
                    ui.label(
                        egui::RichText::new("Nicotine")
                            .family(egui::FontFamily::Name("logo".into()))
                            .size(48.0)
                            .color(NICOTINE_CREAM),
                    );
                });
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
                self.dirty |= vk_dropdown(ui, "forward_key", &mut self.config.forward_key);
            });
            ui.horizontal(|ui| {
                ui.label("Backward:");
                self.dirty |= vk_dropdown(ui, "backward_key", &mut self.config.backward_key);
            });
            ui.horizontal(|ui| {
                ui.label("Modifier:");
                self.dirty |= modifier_dropdown(ui, "modifier_key", &mut self.config.modifier_key);
            });
            ui.label(
                egui::RichText::new(
                    "Set both keys to the same value with a modifier to cycle backward via \
                     modifier+key (e.g. Tab + Shift+Tab).",
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
                    "Windows mice expose XBUTTON1 (back) and XBUTTON2 (forward). To swap \
                     directions, exchange the two values below.",
                )
                .size(10.0)
                .color(NICOTINE_BLACK),
            );
            let prev_fwd = self.config.forward_button;
            let prev_back = self.config.backward_button;
            ui.horizontal(|ui| {
                ui.label("Forward button:");
                ui.add(egui::DragValue::new(&mut self.config.forward_button).range(1..=2));
            });
            ui.horizontal(|ui| {
                ui.label("Backward button:");
                ui.add(egui::DragValue::new(&mut self.config.backward_button).range(1..=2));
            });
            if self.config.forward_button != prev_fwd || self.config.backward_button != prev_back {
                self.dirty = true;
            }
        });
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

/// Dropdown for selecting a Win32 VK code from the curated list. Returns
/// true if the user changed the selection.
fn vk_dropdown(ui: &mut egui::Ui, id: &str, value: &mut u16) -> bool {
    let label = VK_CHOICES
        .iter()
        .find(|(code, _)| *code == *value)
        .map(|(_, name)| *name)
        .unwrap_or("Custom");
    let mut changed = false;
    egui::ComboBox::from_id_source(id)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for (code, name) in VK_CHOICES {
                if ui.selectable_label(*value == *code, *name).clicked() {
                    *value = *code;
                    changed = true;
                }
            }
        });
    changed
}

fn modifier_dropdown(ui: &mut egui::Ui, id: &str, value: &mut Option<u16>) -> bool {
    let label = MODIFIER_CHOICES
        .iter()
        .find(|(code, _)| *code == *value)
        .map(|(_, name)| *name)
        .unwrap_or("Custom");
    let mut changed = false;
    egui::ComboBox::from_id_source(id)
        .selected_text(label)
        .show_ui(ui, |ui| {
            for (code, name) in MODIFIER_CHOICES {
                if ui.selectable_label(*value == *code, *name).clicked() {
                    *value = *code;
                    changed = true;
                }
            }
        });
    changed
}

/// Open the config panel as a top-level window. Blocks until the user
/// closes the window. Takes a shared LiveSettings so slider changes can
/// be applied to the running preview manager instantly.
pub fn run(config: Config, live: Arc<Mutex<LiveSettings>>) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Sized to show every section end-to-end at launch without a
            // scrollbar for a typical ~6 character setup. The scroll area
            // still kicks in if the user has many more characters or
            // shrinks the window manually.
            .with_inner_size([540.0, 900.0])
            .with_min_inner_size([420.0, 500.0])
            .with_title("Nicotine"),
        ..Default::default()
    };

    eframe::run_native(
        "Nicotine",
        options,
        Box::new(move |cc| Ok(Box::new(ConfigPanel::new(cc, config, live)))),
    )
}
