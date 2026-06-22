//! List-view display mode: a single window with the cycle's character
//! roster drawn as rows, instead of one thumbnail per client. Owns its
//! own X11 resources and reconcile path; mirrors the Windows manager's
//! list-mode pixel layout (cream body, red title, gold border, active
//! row in red).

use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;

use x11rb::connection::Connection;
use x11rb::protocol::render::{
    ConnectionExt as _, CreatePictureAux as RenderCreatePictureAux, PictOp,
};
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    Rectangle as XRectangle, SubwindowMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

use super::render::{create_text_pixmap, jetbrains_mono, marlboro, xcolor};
use super::{
    list_window_height, DragState, MutexExt as _, PreviewManager, LIST_BORDER, LIST_ROW_FONT_PX,
    LIST_ROW_HEIGHT, LIST_ROW_LEFT_PAD, LIST_ROW_RIGHT_PAD, LIST_ROW_TOP_PAD, LIST_TEXT_BLACK,
    LIST_TITLE_FONT_PX, LIST_TITLE_STRIP_HEIGHT, LIST_WIDTH, NICOTINE_CREAM, NICOTINE_GOLD,
    NICOTINE_RED,
};

/// Owned per-row text resources for the list-view window. Active and
/// inactive variants are pre-rasterized so the per-frame paint is just
/// "pick one and composite." Drop is on the parent OwnedListWindow.
pub(super) struct RowResources {
    pub(super) red_pixmap: u32,
    pub(super) red_picture: u32,
    pub(super) black_pixmap: u32,
    pub(super) black_picture: u32,
    pub(super) text_w: u16,
    pub(super) text_h: u16,
}

/// Owns the list-view window's resources. Drop releases the row
/// pictures, the title pixmap/picture, the dst picture, and destroys
/// the X11 window.
pub(super) struct OwnedListWindow {
    pub(super) conn: Arc<RustConnection>,
    pub(super) window: u32,
    pub(super) dst_picture: u32,
    pub(super) title_pixmap: u32,
    pub(super) title_picture: u32,
    pub(super) title_text_w: u16,
    pub(super) title_text_h: u16,
    /// Per-character row text. Two pre-rasterized ARGB pictures per
    /// row (red active variant, black inactive variant).
    pub(super) rows: HashMap<String, RowResources>,
    /// Names list as of the most recent rebuild. Diffed against the
    /// current cycle order in `update_list_rows` to decide whether to
    /// re-rasterize the rows + resize the window.
    pub(super) last_names: Vec<String>,
    pub(super) width: u16,
    pub(super) height: u16,
    pub(super) x: i16,
    pub(super) y: i16,
    pub(super) drag: DragState,
    pub(super) grabbed: bool,
}

impl Drop for OwnedListWindow {
    fn drop(&mut self) {
        if self.grabbed {
            let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        }
        for row in self.rows.drain().map(|(_, v)| v) {
            let _ = self.conn.render_free_picture(row.red_picture);
            let _ = self.conn.render_free_picture(row.black_picture);
            let _ = self.conn.free_pixmap(row.red_pixmap);
            let _ = self.conn.free_pixmap(row.black_pixmap);
        }
        let _ = self.conn.render_free_picture(self.dst_picture);
        let _ = self.conn.render_free_picture(self.title_picture);
        let _ = self.conn.free_pixmap(self.title_pixmap);
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

impl PreviewManager {
    pub(super) fn reconcile_list(&mut self) -> Result<()> {
        // Smart-hide gates the list window too. Refresh here (it only runs
        // from reconcile_previews otherwise, so it'd go stale in List mode)
        // and unmap when no EVE window is visible enough. `smart_hide_visible`
        // is always true when the feature is off, so this is a no-op then.
        self.refresh_smart_hide();
        if !self.smart_hide_visible {
            if let Some(list) = &self.list_window {
                let _ = self.conn.unmap_window(list.window);
            }
            return Ok(());
        }
        if self.list_window.is_none() {
            self.create_list_window()?;
        }
        // Re-map in case a prior smart-hide unmapped it.
        if let Some(list) = &self.list_window {
            let _ = self.conn.map_window(list.window);
        }
        self.update_list_rows()?;
        self.paint_list()?;
        Ok(())
    }

    /// Build the list window: solid cream body, red title strip with
    /// "Nicotine" in Marlboro, one row per character. Position pulled
    /// from `preview_positions.toml`'s `list_position` (cross-shared
    /// with the Windows manager) or defaults if missing.
    pub(super) fn create_list_window(&mut self) -> Result<()> {
        let conn = &self.conn;
        let screen = &conn.setup().roots[self.screen_num];
        let our_window = conn.generate_id()?;

        let initial_count = self.state.lock_recover().get_windows().len();
        let height = list_window_height(initial_count);

        let (init_x, init_y) = self
            .positions
            .lock_recover()
            .list_position
            .filter(|(x, y)| self.position_on_screen(*x, *y))
            .unwrap_or((100, 100));

        // No background_pixel: see the matching note in
        // `preview.rs::create_preview`. The list window paints itself
        // fully via XRender on every reconcile and on Expose, so X-server
        // auto-clear is unwanted (it would flash CREAM and hide the red
        // active-row highlight).
        let win_aux = CreateWindowAux::new()
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE,
            )
            .border_pixel(0)
            .override_redirect(1);

        conn.create_window(
            COPY_DEPTH_FROM_PARENT,
            our_window,
            screen.root,
            init_x as i16,
            init_y as i16,
            LIST_WIDTH,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &win_aux,
        )?;

        // Same _NET_WM_STATE_ABOVE + _NET_WM_WINDOW_TYPE_UTILITY hints
        // we use for previews — backed up by ConfigureWindow ABOVE in
        // reconcile().
        conn.change_property32(
            PropMode::REPLACE,
            our_window,
            self.atoms.net_wm_state,
            AtomEnum::ATOM,
            &[self.atoms.net_wm_state_above],
        )?;
        conn.change_property32(
            PropMode::REPLACE,
            our_window,
            self.atoms.net_wm_window_type,
            AtomEnum::ATOM,
            &[self.atoms.net_wm_window_type_utility],
        )?;
        let wm_name_atom: u32 = AtomEnum::WM_NAME.into();
        let string_atom: u32 = AtomEnum::STRING.into();
        let title = b"Nicotine";
        conn.change_property8(
            PropMode::REPLACE,
            our_window,
            wm_name_atom,
            string_atom,
            title,
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            our_window,
            self.atoms.net_wm_name,
            self.atoms.utf8_string,
            title,
        )?;

        conn.map_window(our_window)?;
        conn.sync()?;

        // Pre-rasterize the "Nicotine" header (Marlboro, cream).
        let (title_pixmap, title_picture, title_text_w, title_text_h) = create_text_pixmap(
            conn,
            our_window,
            "Nicotine",
            NICOTINE_CREAM,
            marlboro(),
            LIST_TITLE_FONT_PX,
            self.argb32_format,
        )?;

        let pic_aux = RenderCreatePictureAux::new().subwindowmode(SubwindowMode::INCLUDE_INFERIORS);
        let dst_picture = conn.generate_id()?;
        conn.render_create_picture(dst_picture, our_window, self.visual_format, &pic_aux)?;

        self.list_window = Some(OwnedListWindow {
            conn: Arc::clone(&self.conn),
            window: our_window,
            dst_picture,
            title_pixmap,
            title_picture,
            title_text_w,
            title_text_h,
            rows: HashMap::new(),
            last_names: Vec::new(),
            width: LIST_WIDTH,
            height,
            x: init_x as i16,
            y: init_y as i16,
            drag: DragState::default(),
            grabbed: false,
        });
        Ok(())
    }

    /// Rebuild the list window's per-character row resources whenever
    /// the character set changes (add / remove / reorder). Active vs
    /// inactive is just a swap between two pre-rasterized pictures, so
    /// the per-frame paint stays cheap.
    pub(super) fn update_list_rows(&mut self) -> Result<()> {
        let names: Vec<String> = self
            .state
            .lock_recover()
            .get_ordered_windows()
            .into_iter()
            .map(|w| w.title)
            .collect();

        let needs_rebuild = match &self.list_window {
            Some(list) => list.last_names != names,
            None => return Ok(()),
        };
        if !needs_rebuild {
            return Ok(());
        }

        // Free old row pixmaps + pictures before allocating new ones.
        // Drain so the borrow on list_window ends before we call back
        // into self.conn methods.
        let old_rows: Vec<RowResources> = self
            .list_window
            .as_mut()
            .unwrap()
            .rows
            .drain()
            .map(|(_, v)| v)
            .collect();
        for row in old_rows {
            let _ = self.conn.render_free_picture(row.red_picture);
            let _ = self.conn.render_free_picture(row.black_picture);
            let _ = self.conn.free_pixmap(row.red_pixmap);
            let _ = self.conn.free_pixmap(row.black_pixmap);
        }

        let parent = self.list_window.as_ref().unwrap().window;
        let mut new_rows = HashMap::new();
        for name in &names {
            // Active marker: black-right-pointing-small-triangle. The
            // Windows manager uses 🚬 (cigarette emoji) which JetBrains
            // Mono doesn't ship a glyph for; ▸ keeps a single bundled
            // font working.
            let active_text = format!("▸ {}", name);
            let inactive_text = format!("  {}", name);
            let (red_pixmap, red_picture, text_w, text_h) = create_text_pixmap(
                &self.conn,
                parent,
                &active_text,
                NICOTINE_RED,
                jetbrains_mono(),
                LIST_ROW_FONT_PX,
                self.argb32_format,
            )?;
            let (black_pixmap, black_picture, _, _) = create_text_pixmap(
                &self.conn,
                parent,
                &inactive_text,
                LIST_TEXT_BLACK,
                jetbrains_mono(),
                LIST_ROW_FONT_PX,
                self.argb32_format,
            )?;
            new_rows.insert(
                name.clone(),
                RowResources {
                    red_pixmap,
                    red_picture,
                    black_pixmap,
                    black_picture,
                    text_w,
                    text_h,
                },
            );
        }

        let new_height = list_window_height(names.len());
        let resize = if let Some(list) = self.list_window.as_mut() {
            list.rows = new_rows;
            list.last_names = names;
            let needs = list.height != new_height;
            if needs {
                list.height = new_height;
            }
            needs
        } else {
            false
        };
        if resize {
            let window_id = self.list_window.as_ref().unwrap().window;
            let _ = self.conn.configure_window(
                window_id,
                &ConfigureWindowAux::new().height(new_height as u32),
            );
        }
        Ok(())
    }

    /// Paint the list window: gold perimeter border, cream body, red
    /// title strip with the "Nicotine" header centered, one row per
    /// character with the active row in red. Cheap (a handful of
    /// XRender calls); called from reconcile_list every 100ms and from
    /// update_active on focus changes for instant highlight switching.
    pub(super) fn paint_list(&self) -> Result<()> {
        let Some(list) = &self.list_window else {
            return Ok(());
        };
        let conn = &self.conn;

        let cream = xcolor(NICOTINE_CREAM, 0xffff);
        let red = xcolor(NICOTINE_RED, 0xffff);
        let gold = xcolor(NICOTINE_GOLD, 0xffff);

        // Gold full-window fill provides the outer border in a single
        // call — cream + red rects below cover everything except the
        // 2-px ring around the edge.
        let outer = XRectangle {
            x: 0,
            y: 0,
            width: list.width,
            height: list.height,
        };
        conn.render_fill_rectangles(PictOp::SRC, list.dst_picture, gold, &[outer])?;
        // Cream body, inset by the border.
        let body = XRectangle {
            x: LIST_BORDER as i16,
            y: LIST_BORDER as i16,
            width: list.width - LIST_BORDER * 2,
            height: list.height - LIST_BORDER * 2,
        };
        conn.render_fill_rectangles(PictOp::SRC, list.dst_picture, cream, &[body])?;
        // Red title strip (inside the gold border).
        let strip = XRectangle {
            x: LIST_BORDER as i16,
            y: LIST_BORDER as i16,
            width: list.width - LIST_BORDER * 2,
            height: LIST_TITLE_STRIP_HEIGHT,
        };
        conn.render_fill_rectangles(PictOp::SRC, list.dst_picture, red, &[strip])?;

        // Center "Nicotine" in the strip. Marlboro's glyph box has more
        // descent than ascent so geometric center reads as "logo too
        // high" — bump it down a few pixels to match the visual
        // center, same offset the Windows config panel header uses.
        let title_x = (list.width as i16 - list.title_text_w as i16) / 2;
        let title_y = LIST_BORDER as i16
            + (LIST_TITLE_STRIP_HEIGHT as i16 - list.title_text_h as i16) / 2
            + 3;
        conn.render_composite(
            PictOp::OVER,
            list.title_picture,
            x11rb::NONE,
            list.dst_picture,
            0,
            0,
            0,
            0,
            title_x.max(0),
            title_y.max(0),
            list.title_text_w,
            list.title_text_h,
        )?;

        // Per-row text. Each row occupies LIST_ROW_HEIGHT vertical
        // pixels; the text sits at the top of that slot (ab_glyph
        // bakes leading into the rasterized image height) which gives
        // even visual spacing between rows. Truncate composite width
        // so long character names don't bleed past the right edge.
        let max_text_w =
            (list.width as i16 - LIST_BORDER as i16 * 2 - LIST_ROW_LEFT_PAD - LIST_ROW_RIGHT_PAD)
                .max(0) as u16;
        let mut row_top = LIST_BORDER as i16 + LIST_TITLE_STRIP_HEIGHT as i16 + LIST_ROW_TOP_PAD;
        for name in &list.last_names {
            if let Some(row) = list.rows.get(name) {
                let is_active = self
                    .active_character
                    .as_ref()
                    .is_some_and(|active| active == name);
                let pic = if is_active {
                    row.red_picture
                } else {
                    row.black_picture
                };
                let composite_w = row.text_w.min(max_text_w);
                conn.render_composite(
                    PictOp::OVER,
                    pic,
                    x11rb::NONE,
                    list.dst_picture,
                    0,
                    0,
                    0,
                    0,
                    LIST_BORDER as i16 + LIST_ROW_LEFT_PAD,
                    row_top,
                    composite_w,
                    row.text_h,
                )?;
            }
            row_top += LIST_ROW_HEIGHT as i16;
        }

        Ok(())
    }
}
