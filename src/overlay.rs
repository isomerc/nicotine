use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use eframe::egui;
use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct OverlayApp {
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    config: crate::config::Config,
    drag_start_window_pos: Option<egui::Pos2>,
    drag_accumulated: egui::Vec2,
    overlay_window_id: Option<u32>,
    last_sync: Instant,
    last_index: usize,
}

impl OverlayApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
        config: crate::config::Config,
    ) -> Self {
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
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .insert(0, "jetbrains_mono".to_owned());

        fonts
            .families
            .entry(egui::FontFamily::Name("logo".into()))
            .or_default()
            .push("logo_font".to_owned());

        cc.egui_ctx.set_fonts(fonts);

        Self {
            wm,
            state,
            config,
            drag_start_window_pos: None,
            drag_accumulated: egui::Vec2::ZERO,
            overlay_window_id: None,
            last_sync: Instant::now(),
            last_index: 0,
        }
    }
}

impl eframe::App for OverlayApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Request repaint for smooth updates
        ctx.request_repaint();

        // Read current index from file (instant, no process spawning)
        if let Some(index) = CycleState::read_index_from_file() {
            if index != self.last_index {
                self.last_index = index;
                let mut state = self.state.lock().unwrap();
                state.set_current_index(index);
            }
        }

        // Periodic full sync for window list updates (new clients, etc)
        let now = Instant::now();
        if now.duration_since(self.last_sync).as_millis() >= 500 {
            self.last_sync = now;

            if let Ok(windows) = self.wm.get_eve_windows() {
                let mut state = self.state.lock().unwrap();
                state.update_windows(windows);

                // Resize window based on client count
                let client_count = state.get_windows().len();
                let base_height = 320.0_f32;
                let per_client = 20.0_f32;
                let min_clients = 10;
                let extra_clients = client_count.saturating_sub(min_clients);
                let target_height = base_height + (extra_clients as f32 * per_client);

                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    220.0,
                    target_height,
                )));
            }
        }

        let red = egui::Color32::from_rgb(196, 30, 58);
        let gold = egui::Color32::from_rgb(180, 155, 105);
        let cream = egui::Color32::from_rgb(252, 250, 242);
        let black = egui::Color32::from_rgb(30, 30, 30);

        let _panel_response = egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(cream)
                    .rounding(0.0)
                    .inner_margin(0.0)
                    .stroke(egui::Stroke::new(2.0, gold)),
            )
            .show(ctx, |ui| {
                // Red top bar
                let rect = ui.available_rect_before_wrap();
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(rect.min, egui::vec2(rect.width(), 44.0)),
                    0.0,
                    red,
                );

                // NICOTINE text in red bar
                ui.add_space(10.0);
                ui.vertical_centered(|ui| {
                    ui.label(
                        egui::RichText::new("Nicotine")
                            .family(egui::FontFamily::Name("logo".into()))
                            .size(32.0)
                            .color(cream),
                    );
                });

                ui.add_space(16.0);

                // Client list
                egui::Frame::none()
                    .inner_margin(egui::Margin::symmetric(16.0, 0.0))
                    .show(ui, |ui| {
                        let state = self.state.lock().unwrap();
                        let windows = state.get_windows();
                        let current_index = state.get_current_index();

                        for (i, window) in windows.iter().enumerate() {
                            let is_active = i == current_index;
                            let display_title = &window.title[..window.title.len().min(20)];

                            let text_color = if is_active { red } else { black };
                            let prefix = if is_active { "▸ " } else { "  " };

                            ui.colored_label(
                                text_color,
                                egui::RichText::new(format!("{}{}", prefix, display_title))
                                    .size(13.0)
                                    .strong(),
                            );
                            ui.add_space(2.0);
                        }

                        if windows.is_empty() {
                            ui.add_space(10.0);
                            ui.vertical_centered(|ui| {
                                ui.colored_label(gold, "No clients");
                            });
                        }
                    });

                // Bottom button
                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    ui.add_space(10.0);

                    let button =
                        egui::Button::new(egui::RichText::new("RESTACK").color(cream).size(12.0))
                            .fill(red)
                            .rounding(2.0);

                    if ui.add(button).clicked() {
                        let wm_clone = Arc::clone(&self.wm);
                        let config = self.config.clone();
                        std::thread::spawn(move || {
                            if let Ok(windows) = wm_clone.get_eve_windows() {
                                let _ = wm_clone.stack_windows(&windows, &config);
                            }
                        });
                    }

                    ui.add_space(6.0);
                });
            });

        // Handle dragging with middle mouse button
        // Note: Overlay dragging is X11-only. On Wayland, use your compositor's window
        // management features to position the overlay window.
        let middle_down = ctx.input(|i| i.pointer.button_down(egui::PointerButton::Middle));

        if middle_down {
            // Initialize drag if just started
            if self.drag_start_window_pos.is_none() {
                if let Some(window_pos) = ctx.input(|i| i.viewport().outer_rect).map(|r| r.min) {
                    self.drag_start_window_pos = Some(window_pos);
                    self.drag_accumulated = egui::Vec2::ZERO;

                    // Cache the window ID once at the start
                    if self.overlay_window_id.is_none() {
                        if let Ok(Some(id)) = self.wm.find_window_by_title("Nicotine") {
                            self.overlay_window_id = Some(id);
                        }
                    }
                }
            }

            // Accumulate mouse delta
            let delta = ctx.input(|i| i.pointer.delta());
            if delta.length() > 0.0 {
                self.drag_accumulated += delta;

                // Use cached window ID for instant movement
                if let (Some(start_window), Some(window_id)) =
                    (self.drag_start_window_pos, self.overlay_window_id)
                {
                    let new_x = (start_window.x + self.drag_accumulated.x) as i32;
                    let new_y = (start_window.y + self.drag_accumulated.y) as i32;

                    let _ = self.wm.move_window(window_id, new_x, new_y);
                }
            }

            ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
        } else {
            // Reset drag state when button is released
            self.drag_start_window_pos = None;
            self.drag_accumulated = egui::Vec2::ZERO;

            if ctx.input(|i| i.pointer.hover_pos()).is_some() {
                ctx.set_cursor_icon(egui::CursorIcon::Grab);
            }
        }
    }
}

pub fn run_overlay(
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    overlay_x: f32,
    overlay_y: f32,
    config: crate::config::Config,
) -> Result<(), eframe::Error> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([220.0, 320.0])
            .with_min_inner_size([220.0, 320.0])
            .with_position([overlay_x, overlay_y])
            .with_decorations(false)
            .with_always_on_top()
            .with_transparent(true)
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Nicotine",
        options,
        Box::new(move |cc| {
            // Set window properties after window is created
            std::thread::spawn(|| {
                // Try multiple times with increasing delays (window might not be ready immediately)
                for delay in [300, 500, 1000] {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    if std::process::Command::new("wmctrl")
                        .args(["-F", "-r", "Nicotine", "-b", "add,above,sticky"])
                        .output()
                        .is_ok()
                    {
                        break;
                    }
                }
            });
            Ok(Box::new(OverlayApp::new(cc, wm, state, config)))
        }),
    )
}
