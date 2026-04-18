use crate::config::{Config, DisplayMode, LiveSettings};
use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
            self.touch();
            // Push immediately to the shared LiveSettings so the preview
            // manager swaps modes within its next reconcile tick.
            self.live.lock().unwrap().display_mode = self.config.display_mode;
        }

        ui.add_space(6.0);
        let prev_lock = self.config.positions_locked;
        ui.checkbox(
            &mut self.config.positions_locked,
            "Lock positions (drag disabled on previews and list)",
        );
        if self.config.positions_locked != prev_lock {
            self.touch();
            // Live-apply so the running preview manager stops honoring
            // drags immediately — no save + restart needed.
            self.live.lock().unwrap().positions_locked = self.config.positions_locked;
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
        // Track edits locally; `self.touch()` can't be called from
        // inside the closures below because the closures borrow self.
        let mut dirty = false;

        let len = self.config.characters.len();
        for idx in 0..len {
            // Row 1 — name + reorder + delete.
            ui.horizontal(|ui| {
                ui.label(format!("{}.", idx + 1));
                if ui
                    .text_edit_singleline(&mut self.config.characters[idx])
                    .changed()
                {
                    dirty = true;
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

            // Row 2 — per-character jump hotkey.
            let name = self.config.characters[idx].clone();
            ui.horizontal(|ui| {
                ui.add_space(22.0);
                ui.label("Hotkey:");

                // Modifier dropdown.
                let current_mod = self
                    .config
                    .character_hotkeys
                    .get(&name)
                    .and_then(|h| h.modifier);
                let selected_label = MODIFIER_CHOICES
                    .iter()
                    .find(|(m, _)| *m == current_mod)
                    .map(|(_, l)| *l)
                    .unwrap_or("None");
                let mut new_mod = current_mod;
                egui::ComboBox::from_id_salt(format!("char_mod_{}", idx))
                    .selected_text(selected_label)
                    .width(70.0)
                    .show_ui(ui, |ui| {
                        for (code, label) in MODIFIER_CHOICES {
                            if ui.selectable_label(new_mod == *code, *label).clicked() {
                                new_mod = *code;
                            }
                        }
                    });
                if new_mod != current_mod {
                    // Always persist the modifier choice. If no key has
                    // been bound yet, we create a placeholder entry
                    // with vk=0; the daemon's register_hotkeys skips
                    // vk=0 entries, and the next captured keypress
                    // fills in the vk while preserving this modifier.
                    let entry = self.config.character_hotkeys.entry(name.clone()).or_insert(
                        crate::config::CharacterHotkey {
                            vk: 0,
                            modifier: None,
                        },
                    );
                    entry.modifier = new_mod;
                    dirty = true;
                }

                // Bind button — shows current VK or "none." vk == 0
                // means "only the modifier is set so far," so we also
                // display that as "none" until a real key is captured.
                let binding_label = self
                    .config
                    .character_hotkeys
                    .get(&name)
                    .filter(|h| h.vk != 0)
                    .map(|h| vk_to_label(h.vk))
                    .unwrap_or_else(|| "none".into());
                self.draw_bind_button_sized(
                    ui,
                    &CaptureTarget::Character(name.clone()),
                    binding_label,
                    egui::vec2(100.0, 20.0),
                );

                // Clear the binding entirely.
                if self.config.character_hotkeys.contains_key(&name) && ui.button("✕").clicked() {
                    self.config.character_hotkeys.remove(&name);
                    dirty = true;
                }
            });

            ui.add_space(2.0);
        }

        if dirty {
            self.touch();
        }
        if let Some((a, b)) = swap {
            if b < self.config.characters.len() {
                self.config.characters.swap(a, b);
                self.touch();
            }
        }
        if let Some(idx) = remove {
            // Drop the per-character hotkey for the removed name too.
            let removed_name = self.config.characters.remove(idx);
            self.config.character_hotkeys.remove(&removed_name);
            self.touch();
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
                self.touch();
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
            self.touch();
        }

        ui.add_enabled_ui(self.config.enable_keyboard_buttons, |ui| {
            ui.horizontal(|ui| {
                ui.label("Forward:");
                self.draw_bind_button(
                    ui,
                    &CaptureTarget::ForwardKey,
                    vk_to_label(self.config.forward_key),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Backward:");
                self.draw_bind_button(
                    ui,
                    &CaptureTarget::BackwardKey,
                    vk_to_label(self.config.backward_key),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Modifier:");
                let label = match self.config.modifier_key {
                    Some(vk) => vk_to_label(vk),
                    None => "None".to_string(),
                };
                self.draw_bind_button(ui, &CaptureTarget::ModifierKey, label);
                if self.config.modifier_key.is_some() && ui.button("Clear").clicked() {
                    self.config.modifier_key = None;
                    self.touch();
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

    /// Button that toggles capture for a given config field. When
    /// capturing, shows a hint; otherwise shows the current binding's
    /// label. Click while already capturing to cancel.
    fn draw_bind_button(&mut self, ui: &mut egui::Ui, target: &CaptureTarget, label: String) {
        self.draw_bind_button_sized(ui, target, label, egui::vec2(200.0, 22.0));
    }

    fn draw_bind_button_sized(
        &mut self,
        ui: &mut egui::Ui,
        target: &CaptureTarget,
        label: String,
        size: egui::Vec2,
    ) {
        let is_capturing = self.capturing.as_ref() == Some(target);
        let text = if is_capturing {
            "[press key — Esc]".to_string()
        } else {
            label
        };
        let mut button = egui::Button::new(text).min_size(size);
        if is_capturing {
            button = button
                .fill(NICOTINE_GOLD)
                .stroke(egui::Stroke::new(1.5, NICOTINE_RED));
        }
        if ui.add(button).clicked() {
            self.capturing = if is_capturing {
                None
            } else {
                Some(target.clone())
            };
        }
    }

    fn draw_previews_section(&mut self, ui: &mut egui::Ui) {
        Self::draw_section_header(ui, "Preview Windows");
        let prev_show = self.config.show_previews;
        ui.checkbox(&mut self.config.show_previews, "Show preview windows");
        if self.config.show_previews != prev_show {
            self.touch();
        }
        ui.add_enabled_ui(self.config.show_previews, |ui| {
            // Widen sliders so a 1px step is actually reachable without
            // sub-pixel cursor precision. 3× the egui default width.
            ui.spacing_mut().slider_width = ui.spacing().slider_width * 3.0;

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
                self.touch();
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
/// Win32 VK code to bind, or None if no eligible press happened.
fn captured_binding(ctx: &egui::Context) -> Option<u16> {
    ctx.input(|i| {
        for key in SUPPORTED_KEYS {
            if *key == egui::Key::Escape {
                continue;
            }
            if i.key_pressed(*key) {
                return egui_key_to_vk(*key);
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

    eframe::run_native(
        "Nicotine",
        options,
        Box::new(move |cc| Ok(Box::new(ConfigPanel::new(cc, config, live)))),
    )
}
