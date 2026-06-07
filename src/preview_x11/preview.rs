//! Per-EVE-client preview window: an always-on-top thumbnail surface
//! that mirrors the source via XComposite redirect + XRender composite
//! at vsync via Present. Owns the X11 resources (redirect pixmap, two
//! pictures, render target pixmap/picture, title text, damage handle)
//! and frees them in reverse construction order on Drop.

use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::composite::{ConnectionExt as _, Redirect};
use x11rb::protocol::damage::{ConnectionExt as _, ReportLevel};
use x11rb::protocol::present::{ConnectionExt as _, EventMask as PresentEventMask};
use x11rb::protocol::render::{
    ConnectionExt as _, CreatePictureAux as RenderCreatePictureAux, PictOp,
};
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, PropMode, Rectangle as XRectangle,
    SubwindowMode, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::COPY_DEPTH_FROM_PARENT;

use super::render::{
    apply_scale_transform, create_text_pixmap, jetbrains_mono, pick_visual_format, thumbnail_size,
    xcolor,
};
use super::{
    opacity_to_cardinal, DragState, MutexExt as _, PreviewManager, BORDER_WIDTH, CHROME_DARK,
    NICOTINE_CREAM, NICOTINE_RED, PREVIEW_HEIGHT, PREVIEW_WIDTH, TITLE_FONT_PX, TITLE_STRIP_HEIGHT,
    TITLE_TEXT_LEFT_PAD,
};

/// If a present's CompleteNotify hasn't arrived within this long, assume it
/// was dropped and allow a new present rather than freezing the thumbnail.
/// Far longer than a vblank (~16ms at 60Hz) so it only fires on genuinely
/// lost notifications, not normal pacing.
const STALE_PRESENT: Duration = Duration::from_millis(200);

/// Owns one preview window's resources. Drop releases everything in
/// reverse construction order. Best-effort — if the X11 connection has
/// died we just skip and let the OS clean up.
pub(super) struct OwnedPreview {
    pub(super) conn: Arc<RustConnection>,
    pub(super) window: u32,
    pub(super) source_id: u32,
    /// Character name (the EVE window title minus "EVE - " prefix).
    /// Used as the key for position persistence and for the chrome's
    /// title strip text.
    pub(super) character_name: String,
    /// XDamage resource on the source EVE window. The X server fires
    /// DamageNotify whenever the source draws; we mark the preview
    /// dirty and composite on the next paint pass, then damage_subtract
    /// to re-arm. Lifecycle is tied to the preview (created in
    /// create_preview, destroyed in Drop).
    pub(super) damage_id: u32,
    /// Set when the source has drawn since our last composite, cleared
    /// after we composite. Starts true so a freshly-created preview
    /// paints its first frame even before EVE redraws.
    pub(super) dirty: bool,
    pub(super) src_pixmap: u32,
    pub(super) src_picture: u32,
    /// Offscreen render target sized to match the preview window. Each
    /// frame we paint chrome + the scaled thumbnail into this pixmap,
    /// then present_pixmap flips it onto the window. Going through
    /// Present (instead of rendering straight to the window) gives us
    /// vsync-paced presentation under KWin/XWayland — the previous
    /// "render directly to the window" path used to land at the
    /// compositor's slow scheduling rate for override_redirect surfaces.
    pub(super) render_pixmap: u32,
    pub(super) render_picture: u32,
    /// Premultiplied-ARGB32 pixmap with the rasterized character name
    /// baked in. Drop frees this; chrome paint composites it directly.
    pub(super) title_pixmap: u32,
    pub(super) title_picture: u32,
    pub(super) title_text_w: u16,
    pub(super) title_text_h: u16,
    /// Current window dimensions. Updated on live-size apply (panel
    /// slider drags) and reflected in the next chrome paint + thumbnail
    /// transform.
    pub(super) width: u16,
    pub(super) height: u16,
    /// Source-window dimensions captured at create time. The downscale
    /// transform on `src_picture` is `(source / thumbnail)`; if either
    /// changes (live resize, EVE relaunched at a different size) we
    /// recompute the transform.
    pub(super) source_w: u16,
    pub(super) source_h: u16,
    /// Cached top-left in root-window coordinates. Tracked locally so
    /// drag-handling math doesn't have to round-trip the X server with
    /// `get_geometry` every motion event.
    pub(super) x: i16,
    pub(super) y: i16,
    pub(super) is_active: bool,
    /// True while this preview is unmapped because the "hide active
    /// client's preview" setting is on and this preview's source is the
    /// foreground client. Tracked so the reconcile/active-change passes
    /// only issue map/unmap when the desired state actually flips.
    pub(super) hidden: bool,
    /// Press / motion / release bookkeeping. See `DragState` in
    /// `preview_common` for field semantics.
    pub(super) drag: DragState,
    /// True iff we held an active pointer grab for the current drag.
    /// On lock toggle (positions_locked = true), we skip the grab and
    /// treat the gesture as click-only — but we still need to know
    /// whether to ungrab on release.
    pub(super) grabbed: bool,
    /// Present event-context id for `present_select_input` (CompleteNotify).
    /// Lets us pace repaints to the compositor's vblank instead of EVE's
    /// frame rate. Cleaned up when the window is destroyed.
    pub(super) present_eid: u32,
    /// True between issuing a present and receiving its CompleteNotify. While
    /// set we don't issue another present (we just mark `dirty`), so we
    /// present at most once per vblank. The previous unbounded one-present-
    /// per-EVE-frame flooded KWin's commit queue and degraded the whole
    /// session over time.
    pub(super) present_in_flight: bool,
    /// When the in-flight present was issued — lets us recover if a
    /// CompleteNotify is ever dropped (treat the present as done after a
    /// generous timeout instead of freezing the thumbnail forever).
    pub(super) present_at: Option<Instant>,
}

impl OwnedPreview {
    /// Whether we may issue a present right now: nothing in flight, or the
    /// last present's CompleteNotify never arrived and we've waited long
    /// enough to assume it was lost.
    pub(super) fn can_present(&self) -> bool {
        !self.present_in_flight || self.present_at.is_none_or(|t| t.elapsed() > STALE_PRESENT)
    }
}

impl Drop for OwnedPreview {
    fn drop(&mut self) {
        // Ungrab if we were mid-drag when the preview got dropped
        // (e.g., source EVE window closed while the user was holding
        // the mouse button). X auto-releases grabs on window
        // destruction, but destroy_window below is `let _ =`'d, so
        // belt-and-suspenders this against the rare case where the
        // destroy doesn't make it to the server.
        if self.grabbed {
            let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        }
        // Damage first — once we release the source pixmap and
        // unredirect the source window below, the damage handle would
        // be referencing a possibly-recycled X resource.
        let _ = self.conn.damage_destroy(self.damage_id);
        let _ = self.conn.render_free_picture(self.src_picture);
        let _ = self.conn.render_free_picture(self.render_picture);
        let _ = self.conn.free_pixmap(self.render_pixmap);
        let _ = self.conn.render_free_picture(self.title_picture);
        let _ = self.conn.free_pixmap(self.title_pixmap);
        let _ = self.conn.free_pixmap(self.src_pixmap);
        let _ = self
            .conn
            .composite_unredirect_window(self.source_id, Redirect::AUTOMATIC);
        // Delete the present event context (empty mask) before tearing the
        // window down, so the server stops tracking CompleteNotify for it.
        let _ = self.conn.present_select_input(
            self.present_eid,
            self.window,
            PresentEventMask::NO_EVENT,
        );
        let _ = self.conn.destroy_window(self.window);
        let _ = self.conn.flush();
    }
}

impl PreviewManager {
    pub(super) fn create_preview(&mut self, source_id: Window, title: &str) -> Result<()> {
        let conn = &self.conn;

        let geom = conn.get_geometry(source_id)?.reply()?;
        if geom.width == 0 || geom.height == 0 {
            anyhow::bail!("source window has zero geometry");
        }
        let src_attrs = conn.get_window_attributes(source_id)?.reply()?;

        conn.composite_redirect_window(source_id, Redirect::AUTOMATIC)?;
        let src_pixmap = conn.generate_id()?;
        conn.composite_name_window_pixmap(source_id, src_pixmap)?;

        // Subscribe to damage on the source. NON_EMPTY = "tell me when
        // the damage region transitions from empty to non-empty" — we
        // call damage_subtract in the handler to re-arm it. Each fire
        // means EVE drew (presented) a new frame.
        let damage_id = conn.generate_id()?;
        conn.damage_create(damage_id, source_id, ReportLevel::NON_EMPTY)?;

        conn.sync()?;

        // Source PictFormat must match the source visual; using a
        // mismatched standard format produces BadMatch.
        let formats_reply = conn.render_query_pict_formats()?.reply()?;
        let src_format = pick_visual_format(&formats_reply, src_attrs.visual, geom.depth)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no PictFormat for source visual 0x{:x} depth {}",
                    src_attrs.visual,
                    geom.depth
                )
            })?;

        let screen = &conn.setup().roots[self.screen_num];
        let our_window = conn.generate_id()?;

        // Persisted position takes priority. Drop saved coords if they
        // land off-screen (e.g., user disconnected a monitor); fall
        // through to the diagonal-offset default which guarantees a
        // visible spawn near the top-left of the primary display.
        let saved = self.positions.lock_recover().get(title);
        let (init_x, init_y) = saved
            .filter(|(x, y)| self.position_on_screen(*x, *y))
            .unwrap_or_else(|| {
                let off = self.next_offset;
                self.next_offset = (self.next_offset + 32) % 320;
                (64 + off, 64 + off)
            });

        // Initial dimensions come from LiveSettings (so the panel
        // sliders' current value is honored on first spawn) falling
        // back to config and finally to the compile-time defaults.
        let (init_w, init_h, init_opacity) = {
            let live = self.live.lock_recover();
            (
                live.preview_width as u16,
                live.preview_height as u16,
                live.preview_opacity,
            )
        };
        let init_w = if init_w == 0 { PREVIEW_WIDTH } else { init_w };
        let init_h = if init_h == 0 { PREVIEW_HEIGHT } else { init_h };

        // override_redirect = true: WM doesn't manage this window. No
        // titlebar, no decorations, no entry in alt-tab / taskbar. We
        // own positioning + z-order from here on. BUTTON_PRESS +
        // BUTTON_RELEASE on the window mask cover click + drag start
        // even when we don't grab the pointer (positions-locked path);
        // motion events come via the grab when we do drag.
        //
        // background_pixmap = None (the default when neither it nor
        // background_pixel is set) means X never auto-clears the window
        // on Expose. With a `background_pixel`, X fills the window with
        // that solid color *before* our Expose handler runs — and
        // ConfigureWindow STACK_MODE ABOVE during focus changes can
        // trigger Exposes. That fill produced a one-frame flash of
        // CHROME_DARK on the newly-active preview (red→dark→red) every
        // time the user switched clients. We keep the surface
        // present-driven instead: the render_pixmap is always fully
        // painted before we hand it to Present.
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
            init_w,
            init_h,
            0,
            WindowClass::INPUT_OUTPUT,
            screen.root_visual,
            &win_aux,
        )?;

        // _NET_WM_STATE_ABOVE + _NET_WM_WINDOW_TYPE_UTILITY: most
        // compositors honor these even on override_redirect windows
        // (KWin definitely does), giving us belt-and-suspenders topmost.
        // The reconcile loop also issues ConfigureWindow ABOVE every
        // 100ms as a third backstop.
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
        // Honor the panel's opacity slider on first spawn; reconcile's
        // apply_live_opacity re-pushes this when the slider moves later.
        conn.change_property32(
            PropMode::REPLACE,
            our_window,
            self.atoms.net_wm_window_opacity,
            AtomEnum::CARDINAL,
            &[opacity_to_cardinal(init_opacity)],
        )?;
        // Title isn't shown by override_redirect (no titlebar), but set
        // it anyway so window-list tools and our own xprop debug see it.
        let wm_name_atom: u32 = AtomEnum::WM_NAME.into();
        let string_atom: u32 = AtomEnum::STRING.into();
        conn.change_property8(
            PropMode::REPLACE,
            our_window,
            wm_name_atom,
            string_atom,
            title.as_bytes(),
        )?;
        conn.change_property8(
            PropMode::REPLACE,
            our_window,
            self.atoms.net_wm_name,
            self.atoms.utf8_string,
            title.as_bytes(),
        )?;

        conn.map_window(our_window)?;
        conn.sync()?;

        // Ask the X server for a CompleteNotify after each present, so the
        // repaint can be paced to the compositor's vblank instead of EVE's
        // frame rate (see `OwnedPreview::present_in_flight`).
        let present_eid = conn.generate_id()?;
        conn.present_select_input(present_eid, our_window, PresentEventMask::COMPLETE_NOTIFY)?;

        // Pre-rasterize the character name into a premultiplied-ARGB32
        // pixmap. The cream text color and per-pixel alpha are baked
        // into the pixmap itself, so chrome's text composite is just one
        // PictOp::OVER call with the title picture as the source — no
        // separate mask, no solid-fill picture, nothing for the X server
        // to interpret differently across vendors.
        let (title_pixmap, title_picture, title_text_w, title_text_h) = create_text_pixmap(
            conn,
            our_window,
            title,
            NICOTINE_CREAM,
            jetbrains_mono(),
            TITLE_FONT_PX,
            self.argb32_format,
        )?;

        // Build XRender pictures. Source wraps the redirected pixmap
        // (with a downscale transform set below). Render target is an
        // offscreen pixmap sized to match the preview window — each
        // frame paints chrome + thumbnail into it, then present_pixmap
        // flips it onto the window for vsync-paced display.
        let pic_aux = RenderCreatePictureAux::new().subwindowmode(SubwindowMode::INCLUDE_INFERIORS);

        let src_picture = conn.generate_id()?;
        conn.render_create_picture(src_picture, src_pixmap, src_format, &pic_aux)?;

        let render_pixmap = conn.generate_id()?;
        conn.create_pixmap(screen.root_depth, render_pixmap, our_window, init_w, init_h)?;
        let render_picture = conn.generate_id()?;
        conn.render_create_picture(render_picture, render_pixmap, self.visual_format, &pic_aux)?;

        // Transform: dest coords (within the *thumbnail* area, not the
        // full window) → source coords. Scale factors are >1 because the
        // source is larger than the thumbnail.
        let (thumb_w, thumb_h) = thumbnail_size(init_w, init_h);
        apply_scale_transform(conn, src_picture, geom.width, geom.height, thumb_w, thumb_h)?;

        let is_active = source_id == self.active_id;

        let preview = OwnedPreview {
            conn: Arc::clone(&self.conn),
            window: our_window,
            source_id,
            character_name: title.to_string(),
            damage_id,
            // Force an initial composite even if EVE doesn't redraw for
            // a moment — otherwise a paused/idle source would show a
            // blank thumbnail until the next frame.
            dirty: true,
            src_pixmap,
            src_picture,
            render_pixmap,
            render_picture,
            title_pixmap,
            title_picture,
            title_text_w,
            title_text_h,
            width: init_w,
            height: init_h,
            source_w: geom.width,
            source_h: geom.height,
            x: init_x as i16,
            y: init_y as i16,
            is_active,
            // Created mapped; the reconcile pass right after this unmaps
            // it if the hide-active setting says it should start hidden.
            hidden: false,
            drag: DragState::default(),
            grabbed: false,
            present_eid,
            present_in_flight: false,
            present_at: None,
        };
        self.previews.insert(title.to_string(), preview);

        // Push the first complete frame eagerly. paint_preview_now bundles
        // chrome + source composite + present, so this both fills the
        // newly-allocated render_pixmap and gets it onscreen before any
        // Expose can race against undefined content.
        let _ = self.paint_preview_now(title);
        Ok(())
    }

    /// Paint chrome on a preview: fill the title strip + borders with
    /// the chrome color (Nicotine red when source is the foreground
    /// window, dark otherwise), then composite the pre-rasterized
    /// character-name text on top of the strip.
    pub(super) fn paint_chrome(&self, preview: &OwnedPreview) -> Result<()> {
        let conn = &self.conn;
        let chrome_color = xcolor(
            if preview.is_active {
                NICOTINE_RED
            } else {
                CHROME_DARK
            },
            0xffff,
        );

        // Title strip (full width, top of window) + 3 borders around
        // the thumbnail area. One round-trip.
        let rects = [
            XRectangle {
                x: 0,
                y: 0,
                width: preview.width,
                height: TITLE_STRIP_HEIGHT,
            },
            XRectangle {
                x: 0,
                y: TITLE_STRIP_HEIGHT as i16,
                width: BORDER_WIDTH,
                height: preview.height - TITLE_STRIP_HEIGHT,
            },
            XRectangle {
                x: (preview.width - BORDER_WIDTH) as i16,
                y: TITLE_STRIP_HEIGHT as i16,
                width: BORDER_WIDTH,
                height: preview.height - TITLE_STRIP_HEIGHT,
            },
            XRectangle {
                x: 0,
                y: (preview.height - BORDER_WIDTH) as i16,
                width: preview.width,
                height: BORDER_WIDTH,
            },
        ];
        conn.render_fill_rectangles(PictOp::SRC, preview.render_picture, chrome_color, &rects)?;

        // Composite the pre-rasterized text on top. Source is ARGB32
        // (cream RGB premultiplied with the rasterized alpha); PictOp
        // OVER blends it onto the chrome strip we just filled.
        let baseline_y = (TITLE_STRIP_HEIGHT as i16 - preview.title_text_h as i16) / 2;
        let baseline_y = baseline_y.max(0);
        conn.render_composite(
            PictOp::OVER,
            preview.title_picture,
            x11rb::NONE,
            preview.render_picture,
            0,
            0,
            0,
            0,
            TITLE_TEXT_LEFT_PAD,
            baseline_y,
            preview.title_text_w,
            preview.title_text_h,
        )?;

        Ok(())
    }

    /// Per-frame: composite the source picture into the thumbnail area.
    /// Replace this preview's src_pixmap/src_picture with fresh
    /// references to the EVE window's *current* redirect storage.
    /// Under XWayland + DXVK (EVE's Wine/Vulkan stack), each DRI3
    /// present allocates a fresh backing buffer; the original pixmap
    /// we got at create-time keeps pointing at whatever buffer was
    /// front when redirect was first established, which looks like the
    /// preview is throttled to a small fraction of EVE's actual rate.
    /// Calling composite_name_window_pixmap on each damage gets us a
    /// reference to the current storage.
    pub(super) fn refresh_source_pixmap(&mut self, key: &str) -> Result<()> {
        let preview = match self.previews.get(key) {
            Some(p) => p,
            None => return Ok(()),
        };
        let (source_id, source_w, source_h, thumb_w, thumb_h, old_picture, old_pixmap) = (
            preview.source_id,
            preview.source_w,
            preview.source_h,
            thumbnail_size(preview.width, preview.height).0,
            thumbnail_size(preview.width, preview.height).1,
            preview.src_picture,
            preview.src_pixmap,
        );
        let src_format = self.visual_format;

        let new_pixmap = self.conn.generate_id()?;
        // .check() forces a server round-trip so we surface BadWindow
        // synchronously when the source EVE window has closed between
        // the DamageNotify and this rebind. Without it the request
        // looks successful locally; the error arrives asynchronously
        // through the event channel only after we've already mutated
        // preview.src_pixmap / src_picture to point at IDs whose
        // server-side creation failed, cascading BadPixmap/BadPicture
        // through every subsequent composite and the final Drop.
        // On failure: leave the preview's existing src_picture / src_pixmap
        // alone (they're still valid X resources), kick reconcile to
        // drop this preview on its next tick, and return Ok so the
        // caller doesn't log a spurious error during the brief window
        // between source closure and reconcile-driven cleanup.
        if self
            .conn
            .composite_name_window_pixmap(source_id, new_pixmap)?
            .check()
            .is_err()
        {
            self.needs_window_scan = true;
            return Ok(());
        }
        let new_picture = self.conn.generate_id()?;
        let pic_aux = RenderCreatePictureAux::new().subwindowmode(SubwindowMode::INCLUDE_INFERIORS);
        self.conn
            .render_create_picture(new_picture, new_pixmap, src_format, &pic_aux)?;
        apply_scale_transform(
            &self.conn,
            new_picture,
            source_w,
            source_h,
            thumb_w,
            thumb_h,
        )?;

        // Free the previous handle and pixmap. The actual server-side
        // buffer they referenced is kept alive by any pending composite
        // requests that still hold it.
        let _ = self.conn.render_free_picture(old_picture);
        let _ = self.conn.free_pixmap(old_pixmap);

        if let Some(preview) = self.previews.get_mut(key) {
            preview.src_pixmap = new_pixmap;
            preview.src_picture = new_picture;
        }
        Ok(())
    }

    /// Refresh + composite + present one preview right now, in response
    /// to a DamageNotify. Doing this synchronously on the event arrival
    /// (vs. deferring to the loop tick) minimizes the time between EVE's
    /// buffer swap and our sample of it, which is the difference between
    /// "fps tracks EVE's rate" and "jittery sub-rate cadence".
    ///
    /// Repaints chrome at the top of every call so each presented pixmap
    /// is a complete, self-consistent frame (chrome + thumbnail). The
    /// X Present extension references the source pixmap until the
    /// presentation completes; if chrome were modified out-of-band by
    /// `update_active` between a present and its vblank, the displayed
    /// frame could capture the new chrome on top of the old thumbnail
    /// (or vice versa) — the flicker that was previously visible when
    /// switching clients.
    pub(super) fn paint_preview_now(&mut self, key: &str) -> Result<()> {
        self.refresh_source_pixmap(key)?;
        let preview_snapshot = match self.previews.get(key) {
            Some(p) => p,
            None => return Ok(()),
        };
        // Borrow-juggling: paint_chrome takes &self, but the second half
        // of this function takes &mut self via previews.get_mut. We do
        // the chrome paint here with the immutable borrow, then reacquire
        // mutably below for the composite/present.
        self.paint_chrome(preview_snapshot)?;
        let preview = match self.previews.get_mut(key) {
            Some(p) => p,
            None => return Ok(()),
        };
        let (thumb_w, thumb_h) = thumbnail_size(preview.width, preview.height);
        self.conn.render_composite(
            PictOp::SRC,
            preview.src_picture,
            x11rb::NONE,
            preview.render_picture,
            0,
            0,
            0,
            0,
            BORDER_WIDTH as i16,
            TITLE_STRIP_HEIGHT as i16,
            thumb_w,
            thumb_h,
        )?;
        // Hand the finished pixmap to the X server / XWayland for
        // presentation at the next vblank. target_msc=0 means "soonest
        // possible" — XWayland forwards to KWin via the wp_presentation
        // Wayland protocol with correct timing.
        self.conn.present_pixmap(
            preview.window,
            preview.render_pixmap,
            0,
            x11rb::NONE,
            x11rb::NONE,
            0,
            0,
            x11rb::NONE,
            x11rb::NONE,
            x11rb::NONE,
            0,
            0,
            0,
            0,
            &[],
        )?;
        preview.dirty = false;
        // Mark the present in flight: we won't issue another until its
        // CompleteNotify arrives (or STALE_PRESENT elapses), which paces us
        // to the compositor's vblank instead of EVE's frame rate.
        preview.present_in_flight = true;
        preview.present_at = Some(Instant::now());
        // Flush right now so the present hits the X server in time for
        // the next vblank instead of waiting up to a full loop tick to
        // flush — that extra 0-8ms of buffer latency reads as judder.
        self.conn.flush()?;
        Ok(())
    }

    /// Loop-tick backstop: composite + present any preview still marked
    /// dirty. This is mostly a safety net for newly-created previews
    /// whose first paint can't be triggered by a damage event yet, and
    /// for the post-Expose redraw path. Steady-state painting happens
    /// from the DamageNotify handler.
    pub(super) fn repaint_thumbnails(&mut self) -> Result<()> {
        // Only previews that are dirty AND not mid-present (or whose present
        // went stale). The steady-state repaint clock is now
        // PresentCompleteNotify; this backstop just covers the first frame
        // and recovery from a dropped notify.
        let dirty_keys: Vec<String> = self
            .previews
            .iter()
            .filter(|(_, p)| p.dirty && p.can_present())
            .map(|(k, _)| k.clone())
            .collect();
        for key in &dirty_keys {
            if let Err(e) = self.paint_preview_now(key) {
                eprintln!("Preview paint error ({}): {}", key, e);
            }
        }
        Ok(())
    }
}
