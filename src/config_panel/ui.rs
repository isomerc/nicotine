//! The big `impl ConfigPanel` block that draws the panel: section
//! headers, display-mode picker, character list, hotkey bindings, and
//! preview-window sliders. Lives in its own file because at ~350 lines
//! it dominates `mod.rs` if kept inline.

use eframe::egui;

use crate::config::DisplayMode;

use super::{
    code_to_label, CaptureTarget, ConfigPanel, MODIFIER_CHOICES, NICOTINE_BLACK, NICOTINE_GOLD,
    NICOTINE_RED,
};

impl ConfigPanel {
    pub(super) fn draw_section_header(ui: &mut egui::Ui, label: &str) {
        ui.label(
            egui::RichText::new(label)
                .size(16.0)
                .strong()
                .color(NICOTINE_RED),
        );
        ui.separator();
    }

    pub(super) fn draw_display_mode_section(&mut self, ui: &mut egui::Ui) {
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

        ui.add_space(6.0);
        // Restack EVE clients to the configured stack geometry. Sends
        // the request over the IPC socket so the daemon (which owns the
        // WindowManager) does the work — same path as `nicotine stack`
        // on the CLI. Errors are silent: if the daemon isn't running,
        // there's nothing to restack anyway.
        if ui.button("Restack EVE Windows").clicked() {
            let _ = crate::daemon::send_command("stack");
        }
    }

    pub(super) fn draw_characters_section(&mut self, ui: &mut egui::Ui) {
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
                    .map(|h| code_to_label(h.vk))
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

    pub(super) fn draw_hotkeys_section(&mut self, ui: &mut egui::Ui) {
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
                    code_to_label(self.config.forward_key),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Backward:");
                self.draw_bind_button(
                    ui,
                    &CaptureTarget::BackwardKey,
                    code_to_label(self.config.backward_key),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Modifier:");
                let label = match self.config.modifier_key {
                    Some(vk) => code_to_label(vk),
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

        ui.add_space(8.0);
        let prev_mouse = self.config.enable_mouse_buttons;
        ui.checkbox(
            &mut self.config.enable_mouse_buttons,
            "Cycle on mouse side buttons (XBUTTON1/XBUTTON2)",
        );
        if self.config.enable_mouse_buttons != prev_mouse {
            self.touch();
        }
        ui.label(
            egui::RichText::new(
                "Off by default. Turn on only if you don't already remap your mouse \
                 side buttons via driver software (Logi Options+, Razer Synapse, etc.) \
                 — otherwise this will hijack the buttons in browsers/games too.",
            )
            .size(10.0)
            .color(NICOTINE_BLACK),
        );
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

    pub(super) fn draw_previews_section(&mut self, ui: &mut egui::Ui) {
        Self::draw_section_header(ui, "Preview Windows");
        let prev_show = self.config.show_previews;
        ui.checkbox(&mut self.config.show_previews, "Show preview windows");
        if self.config.show_previews != prev_show {
            self.touch();
            // Push to LiveSettings so the preview manager spawns or tears
            // down its windows on its next reconcile tick instead of
            // waiting for a daemon restart.
            self.live.lock().unwrap().show_previews = self.config.show_previews;
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
