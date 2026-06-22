// Linux preview window manager (Phases 1+3).
//
// Per-EVE-client always-on-top, undecorated preview windows. Live source
// mirror via XComposite redirect + XRender server-side composite. Chrome:
//   - Nicotine-red title strip with the character name (white text,
//     JetBrains Mono rasterized via ab_glyph)
//   - 3px border around the thumbnail; flips to Nicotine red when the
//     source is the system foreground window, dark otherwise
//
// override_redirect = true lifts WM management (no titlebar, no taskbar
// entry, no alt-tab inclusion) so we own the entire surface. We re-assert
// topmost via _NET_WM_STATE_ABOVE plus a configure-window stack-above on
// every reconcile tick — the same belt-and-suspenders pattern the Windows
// manager uses against DWM topmost drift.
//
// Active-state tracking is event-driven via PropertyNotify on the root
// window's _NET_ACTIVE_WINDOW atom. Same instant-update behavior as the
// Windows manager's WinEventHook(EVENT_SYSTEM_FOREGROUND) — focus changes
// repaint the affected previews' chrome immediately, no 100ms wait.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as _;
use x11rb::protocol::damage::ConnectionExt as _;
use x11rb::protocol::render::{ConnectionExt as _, CreatePictureAux as RenderCreatePictureAux};
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux,
    ConnectionExt as _, EventMask, InputFocus, PropMode, StackMode, SubwindowMode, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::config::{Config, DisplayMode, LiveSettings};
use crate::cycle_state::CycleState;
use crate::preview_common::{preview_should_hide, DragRect, DragState};
use crate::preview_positions::PreviewPositions;
use crate::window_manager::WindowManager;

/// `Mutex::lock().unwrap()` panics if the mutex was poisoned by a
/// thread that died while holding it. `live` and `positions` are
/// shared with the config-panel (egui) thread; a panic over there
/// would otherwise propagate here and silently kill the preview
/// pipeline until the daemon is restarted. The protected state is
/// plain data — `LiveSettings`, `PreviewPositions`, `CycleState` —
/// where the last-written value is always self-consistent, so the
/// recovery is just "take the inner guard and carry on."
trait MutexExt<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Light-maintenance tick: re-asserts topmost stacking and applies any
/// pending live-size changes. All-async X requests, so 100ms is cheap.
/// The expensive bit — sync get_property round-trips to enumerate EVE
/// windows — is event-driven off _NET_CLIENT_LIST PropertyNotify (see
/// handle_event) so it only runs when EVE windows actually appear or
/// disappear. Running enumeration on every tick caused ~10Hz judder.
const RECONCILE_INTERVAL: Duration = Duration::from_millis(100);
/// Backstop for window enumeration. The event-driven triggers
/// (_NET_CLIENT_LIST + _NET_ACTIVE_WINDOW PropertyNotify) cover the
/// common cases, but neither fires if a new EVE client maps with a
/// generic title ("EVE Online") and the user never focuses it — they'd
/// see the client in their cycle list (the daemon re-enumerates every
/// 500ms separately) but no preview window. Re-scanning every 2s as a
/// safety net catches those late title resolutions. 2s is slow enough
/// to be invisible — the scan is ~3 sync round-trips, perceptible at
/// 10Hz but not at 0.5Hz.
const WINDOW_SCAN_BACKSTOP: Duration = Duration::from_secs(2);
/// 8ms ≈ 125Hz polling, slightly above a 120Hz display's vblank rate.
/// Composites + presents are gated on damage events, so an idle loop
/// iteration is just one poll_for_event call — cheap. Going faster than
/// display rate kills the "present 1 vblank too late" cadence that an
/// exactly-60fps loop hits on a 120Hz panel.
const FRAME_INTERVAL: Duration = Duration::from_millis(8);

// Phase 3 still hardcodes preview dimensions; Phase 4 (live size sliders)
// will pull these from config / LiveSettings. Includes chrome — a 480×270
// preview is 480×270 total, with 24px title + 3px border eating into the
// thumbnail area.
const PREVIEW_WIDTH: u16 = 480;
const PREVIEW_HEIGHT: u16 = 270;

const TITLE_STRIP_HEIGHT: u16 = 24;
const BORDER_WIDTH: u16 = 3;
const TITLE_FONT_PX: f32 = 14.0;
const TITLE_TEXT_LEFT_PAD: i16 = 10;

// Borderless mode: the name is drawn over the thumbnail (no title strip),
// so it gets a translucent black backdrop to stay legible over bright
// content. ~60% alpha (0x9999 of 0xffff); top padding above the glyphs.
const NAME_BACKDROP_ALPHA: u16 = 0x9999;
const NAME_TOP_PAD: i16 = 6;

// Drag / snap thresholds (DRAG_THRESHOLD_PX, SNAP_THRESHOLD_PX) live
// in `preview_common` since both the X11 and Windows managers use
// them. Imported above.

// Brand palette. Same RGB triples as the Windows preview chrome and the
// (now-removed) Linux egui overlay so the look stays consistent across
// platforms.
const NICOTINE_RED: (u8, u8, u8) = (196, 30, 58);
const CHROME_DARK: (u8, u8, u8) = (30, 30, 30);
const NICOTINE_CREAM: (u8, u8, u8) = (252, 250, 242);
const LIST_TEXT_BLACK: (u8, u8, u8) = (30, 30, 30);
/// Brand gold used for the list-view window's perimeter border. Same
/// hue as the (now-removed) egui overlay's frame so the list-view
/// keeps the recognizable Nicotine look.
const NICOTINE_GOLD: (u8, u8, u8) = (180, 155, 105);

// List-view (Display Mode = List) constants. Mirror the (now-removed)
// egui overlay's look: 2px gold border around the whole window, red
// title strip with "Nicotine" in Marlboro, cream body, one row per
// character with the active row in Nicotine red.
const LIST_WIDTH: u16 = 220;
const LIST_BORDER: u16 = 2;
const LIST_TITLE_STRIP_HEIGHT: u16 = 44;
const LIST_TITLE_FONT_PX: f32 = 32.0;
const LIST_ROW_HEIGHT: u16 = 22;
const LIST_ROW_FONT_PX: f32 = 13.0;
const LIST_ROW_LEFT_PAD: i16 = 16;
const LIST_ROW_RIGHT_PAD: i16 = 16;
const LIST_ROW_TOP_PAD: i16 = 14;
const LIST_BOTTOM_PAD: u16 = 12;

mod events;
mod list;
mod preview;
mod render;
use list::OwnedListWindow;
use preview::OwnedPreview;
use render::*;

/// Total height of the list window for `n` character rows. Includes
/// the 2-px gold border top and bottom, the red title strip, the
/// top-pad gap below it, the rows themselves, and a bottom pad above
/// the bottom border.
fn list_window_height(num_clients: usize) -> u16 {
    let rows = num_clients.max(1) as u16;
    LIST_BORDER * 2
        + LIST_TITLE_STRIP_HEIGHT
        + LIST_ROW_TOP_PAD as u16
        + rows * LIST_ROW_HEIGHT
        + LIST_BOTTOM_PAD
}

/// Spawn the Linux preview manager. Returns immediately; the manager
/// thread runs forever (until the daemon process exits).
pub fn spawn(
    config: Config,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    live: Arc<Mutex<LiveSettings>>,
) -> Result<JoinHandle<()>> {
    let handle = std::thread::spawn(move || {
        if let Err(e) = run_manager(config, wm, state, live) {
            eprintln!("Linux preview manager exited with error: {}", e);
        }
    });
    Ok(handle)
}

fn run_manager(
    config: Config,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    live: Arc<Mutex<LiveSettings>>,
) -> Result<()> {
    let (conn, screen_num) = x11rb::connect(None).context("X11 connect failed (DISPLAY set?)")?;
    let conn = Arc::new(conn);

    // XFixes is a hard prerequisite for Composite — query it first to
    // surface a meaningful error if either isn't supported.
    conn.xfixes_query_version(4, 0)
        .context("XFixes extension query")?
        .reply()
        .context("XFixes extension reply — XFixes not supported by X server?")?;
    conn.composite_query_version(0, 4)
        .context("Composite extension query")?
        .reply()
        .context("Composite extension reply — Composite not supported by X server?")?;
    conn.render_query_version(0, 11)
        .context("Render extension query")?
        .reply()
        .context("Render extension reply — Render not supported by X server?")?;
    // Damage drives per-source repaint: we composite each preview only
    // when its source EVE window actually drew, instead of running a
    // free-running 60fps loop and burning the same identical pixmap.
    conn.damage_query_version(1, 1)
        .context("Damage extension query")?
        .reply()
        .context("Damage extension reply — Damage not supported by X server?")?;

    let formats_reply = conn
        .render_query_pict_formats()?
        .reply()
        .context("Render QueryPictFormats")?;

    let screen = &conn.setup().roots[screen_num];
    let visual_format = pick_visual_format(&formats_reply, screen.root_visual, screen.root_depth)
        .ok_or_else(|| anyhow::anyhow!("no PictFormat for default visual"))?;
    // ARGB32 is the standard depth-32 alpha-premultiplied format. Used
    // for the rasterized character-name text — we bake the cream color
    // and per-pixel alpha into one source picture so chrome paint is a
    // single composite call with no separate alpha mask, side-stepping
    // a class of A8 mask issues that vary across X servers.
    let argb32_format = pick_argb32_format(&formats_reply)
        .ok_or_else(|| anyhow::anyhow!("no ARGB32 PictFormat (depth-32)"))?;

    let atoms = Atoms {
        net_wm_name: atom(&conn, b"_NET_WM_NAME")?,
        utf8_string: atom(&conn, b"UTF8_STRING")?,
        net_wm_state: atom(&conn, b"_NET_WM_STATE")?,
        net_wm_state_above: atom(&conn, b"_NET_WM_STATE_ABOVE")?,
        net_wm_window_type: atom(&conn, b"_NET_WM_WINDOW_TYPE")?,
        net_wm_window_type_utility: atom(&conn, b"_NET_WM_WINDOW_TYPE_UTILITY")?,
        net_client_list: atom(&conn, b"_NET_CLIENT_LIST")?,
        net_active_window: atom(&conn, b"_NET_ACTIVE_WINDOW")?,
        net_wm_pid: atom(&conn, b"_NET_WM_PID")?,
        net_wm_window_opacity: atom(&conn, b"_NET_WM_WINDOW_OPACITY")?,
    };

    // Subscribe to PropertyNotify on the root window so changes to
    // _NET_ACTIVE_WINDOW (focus shift) wake us up immediately. Mirror of
    // the Windows manager's WinEventHook(EVENT_SYSTEM_FOREGROUND).
    let root = screen.root;
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
    )?;

    let active_id = current_active_window(&conn, root, atoms.net_active_window).unwrap_or(0);
    let positions = Arc::new(Mutex::new(PreviewPositions::load()));

    // Track the size we last applied to all previews so we don't issue
    // a no-op resize burst every reconcile tick. Initialized to (0, 0)
    // so the first reconcile applies whatever LiveSettings says.
    let last_applied_size = (0u16, 0u16);

    // Same idea for opacity. 0 is never a valid slider value (range is
    // 10..=100), so the first reconcile always pushes the real value.
    let last_applied_opacity = 0u32;

    // Initial display mode + borderless come from LiveSettings (the
    // panel's last saved choice). Previews are created reading the same
    // live value, so we seed `last_applied_borderless` to match and the
    // first reconcile doesn't do spurious relayout work.
    let (initial_mode, initial_borderless) = {
        let l = live.lock_recover();
        (l.display_mode, l.borderless)
    };

    let mut manager = PreviewManager {
        conn: Arc::clone(&conn),
        screen_num,
        config,
        wm,
        state,
        live,
        previews: HashMap::new(),
        visual_format,
        argb32_format,
        next_offset: 0,
        atoms,
        active_id,
        active_character: None,
        positions,
        last_applied_size,
        last_applied_opacity,
        last_applied_borderless: initial_borderless,
        current_mode: initial_mode,
        list_window: None,
        needs_window_scan: true,
        last_window_scan: Instant::now(),
    };
    manager.run()
}

/// Cached atoms so we don't re-intern on every tick.
struct Atoms {
    net_wm_name: u32,
    utf8_string: u32,
    net_wm_state: u32,
    net_wm_state_above: u32,
    net_wm_window_type: u32,
    net_wm_window_type_utility: u32,
    net_client_list: u32,
    net_active_window: u32,
    net_wm_pid: u32,
    /// `_NET_WM_WINDOW_OPACITY` — CARDINAL the compositor reads to blend
    /// the whole window. Set per preview from the panel's opacity slider.
    net_wm_window_opacity: u32,
}

struct PreviewManager {
    conn: Arc<RustConnection>,
    screen_num: usize,
    /// Reserved for Phase 5+ (display mode toggle reads `config.display_mode`,
    /// `config.show_previews` for per-tick cycle integration).
    #[allow(dead_code)]
    config: Config,
    /// Reserved for Phase 5 (cycle integration: active-state via
    /// CycleState.current_index in addition to _NET_ACTIVE_WINDOW).
    #[allow(dead_code)]
    wm: Arc<dyn WindowManager>,
    /// Reserved for Phase 5 (cycle integration).
    #[allow(dead_code)]
    state: Arc<Mutex<CycleState>>,
    /// Shared with the config panel. The reconcile loop reads it each
    /// tick to apply slider-driven preview-size changes and the
    /// positions-locked toggle without waiting for save-to-disk.
    live: Arc<Mutex<LiveSettings>>,
    previews: HashMap<String, OwnedPreview>,
    visual_format: u32,
    argb32_format: u32,
    next_offset: i32,
    atoms: Atoms,
    /// X11 id of the current foreground window (any X11 window, not just
    /// EVE). Compared against each preview's source_id to set is_active.
    active_id: u32,
    /// Character name of the currently-active EVE client, or None if
    /// no EVE window is foregrounded. Tracked alongside `active_id`
    /// because list-view's active-row highlight matches on name (the
    /// Sway/Hyprland WindowManager impls use compositor-native ids
    /// that don't match X11 ids; comparing names dodges that).
    active_character: Option<String>,
    /// Per-character preview positions, persisted to
    /// `~/.config/nicotine/preview_positions.toml`. Shared with the
    /// Windows preview manager via the same TOML schema. We `Arc<Mutex>`
    /// it so background save calls from drag-end handlers can run
    /// without blocking the event loop.
    positions: Arc<Mutex<PreviewPositions>>,
    /// Last size we applied to all preview windows. Diffed against
    /// LiveSettings on each reconcile to skip the resize work when the
    /// user hasn't touched the sliders.
    last_applied_size: (u16, u16),
    /// Last opacity percent we pushed to all preview windows. Diffed
    /// against LiveSettings each reconcile so we only re-set the
    /// `_NET_WM_WINDOW_OPACITY` property when the slider actually moved.
    last_applied_opacity: u32,
    /// Last borderless state we laid previews out for. Diffed against
    /// LiveSettings each reconcile; on a flip we rebuild every preview's
    /// downscale transform (the thumbnail rect changes) and repaint.
    last_applied_borderless: bool,
    /// Whether the panel currently wants per-client previews or a
    /// single client-list window. Reconcile detects transitions and
    /// tears down the outgoing mode's surfaces before spawning the
    /// incoming mode's.
    current_mode: DisplayMode,
    /// The single list-view window (Display Mode = List). `None` when
    /// the user is in Previews mode.
    list_window: Option<OwnedListWindow>,
    /// Set when an _NET_CLIENT_LIST PropertyNotify fires (or on first
    /// tick) so the next reconcile knows it needs the sync round-trip
    /// window enumeration. Without this we'd re-enumerate every 100ms,
    /// and the get_property round-trips cause visible ~10Hz judder.
    needs_window_scan: bool,
    /// Wall time of the last successful enumeration. Drives the
    /// `WINDOW_SCAN_BACKSTOP` periodic re-scan that catches EVE clients
    /// whose title only becomes "EVE - <name>" after the user logs in.
    last_window_scan: Instant,
}

impl PreviewManager {
    fn run(&mut self) -> Result<()> {
        // Resolve the initial active state. PropertyNotify only fires
        // on _NET_ACTIVE_WINDOW *changes*, and the common case is the
        // user already has EVE focused when nicotine starts — without
        // an explicit kickoff here the list view's active highlight
        // would stay blank until the user focused some other window
        // and came back. Errors are non-fatal; the next focus change
        // will populate state correctly.
        let _ = self.update_active();

        let mut last_reconcile = Instant::now()
            .checked_sub(RECONCILE_INTERVAL)
            .unwrap_or_else(Instant::now);
        let mut next_frame = Instant::now();

        loop {
            while let Some(event) = self.conn.poll_for_event()? {
                if let Err(e) = self.handle_event(event) {
                    eprintln!("Preview event handling error: {}", e);
                }
            }

            if last_reconcile.elapsed() >= RECONCILE_INTERVAL {
                if let Err(e) = self.reconcile() {
                    eprintln!("Preview reconcile error: {}", e);
                }
                last_reconcile = Instant::now();
            }

            if let Err(e) = self.repaint_thumbnails() {
                eprintln!("Preview repaint error: {}", e);
            }
            self.conn.flush()?;

            next_frame += FRAME_INTERVAL;
            let now = Instant::now();
            if let Some(wait) = next_frame.checked_duration_since(now) {
                std::thread::sleep(wait);
            } else {
                next_frame = now + FRAME_INTERVAL;
            }
        }
    }

    fn reconcile(&mut self) -> Result<()> {
        // Panel show/hide toggle. Drop all surfaces when the user
        // unchecks "Show preview windows" so the manager goes truly
        // dark (no override_redirect windows lingering, no XComposite
        // redirect on EVE surfaces). When the user re-checks it, set
        // needs_window_scan so reconcile_previews actually rebuilds —
        // its normal gate only fires on _NET_CLIENT_LIST changes,
        // which won't fire just because we want to repopulate after
        // being hidden.
        let show_previews = self.live.lock_recover().show_previews;
        if !show_previews {
            if !self.previews.is_empty() {
                self.previews.clear();
            }
            if self.list_window.is_some() {
                self.list_window = None;
            }
            // Stay quiet — don't run the rest of reconcile.
            return Ok(());
        }
        if self.previews.is_empty() && self.list_window.is_none() {
            // Either we just toggled back on or we started clean;
            // either way, re-enumerate EVE windows on the next
            // reconcile_previews pass.
            self.needs_window_scan = true;
        }

        // Display-mode transition (panel toggled Previews ↔ List).
        // Tear down the outgoing mode's surfaces before the matching
        // mode-specific reconcile spawns the new one.
        let target_mode = self.live.lock_recover().display_mode;
        if target_mode != self.current_mode {
            match target_mode {
                DisplayMode::Previews => {
                    // OwnedListWindow::drop frees its row pictures,
                    // title pixmap, dst picture, and destroys the window.
                    self.list_window = None;
                }
                DisplayMode::List => {
                    // OwnedPreview::drop unredirects each source EVE
                    // window and tears down per-preview resources.
                    self.previews.clear();
                }
            }
            self.current_mode = target_mode;
        }

        match self.current_mode {
            DisplayMode::Previews => self.reconcile_previews()?,
            DisplayMode::List => self.reconcile_list()?,
        }

        // Topmost is re-asserted from the focus-change PropertyNotify
        // handler, which is enough for the common case (clicking an EVE
        // client raises it, we react and restack). A periodic backstop
        // here used to fire every ~1s, but KWin re-blits the preview
        // backing buffer on every ConfigureWindow STACK_MODE — visible
        // as a one-frame "squash" on the thumbnail every second. The
        // edge case (something covers a preview without a focus event)
        // is rare and self-recovers on the next click.
        Ok(())
    }

    /// Push all our surfaces above other windows. ConfigureWindow with
    /// STACK_MODE ABOVE is async (no reply), so the cost is server-side
    /// not per-us. Called from the tick backstop and immediately on
    /// focus changes.
    fn restack_topmost(&self) {
        for preview in self.previews.values() {
            let _ = self.conn.configure_window(
                preview.window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            );
        }
        if let Some(list) = &self.list_window {
            let _ = self.conn.configure_window(
                list.window,
                &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
            );
        }
    }

    fn reconcile_previews(&mut self) -> Result<()> {
        // Only re-enumerate EVE windows when something actually changed.
        // find_eve_windows is ~3 sync get_property round-trips on a
        // typical setup; doing it every 100ms made the preview judder
        // at ~10Hz. Triggers: startup (initial scan), _NET_CLIENT_LIST
        // PropertyNotify, _NET_ACTIVE_WINDOW PropertyNotify, and the
        // WINDOW_SCAN_BACKSTOP timer below (catches new clients whose
        // title only resolves after login and that the user never
        // explicitly focuses).
        if self.last_window_scan.elapsed() >= WINDOW_SCAN_BACKSTOP {
            self.needs_window_scan = true;
        }
        if self.needs_window_scan {
            let eve_windows = self.find_eve_windows()?;
            let live: HashSet<String> = eve_windows.iter().map(|(_, t)| t.clone()).collect();

            // Drop previews whose source disappeared. OwnedPreview::drop
            // handles all the X11 cleanup (pixmaps, pictures, unredirect).
            self.previews.retain(|name, _| live.contains(name));

            for (window_id, title) in &eve_windows {
                let needs_rebind = matches!(
                    self.previews.get(title),
                    Some(existing) if existing.source_id != *window_id
                );
                if needs_rebind {
                    self.previews.remove(title);
                }
                if !self.previews.contains_key(title) {
                    if let Err(e) = self.create_preview(*window_id, title) {
                        eprintln!("Create preview '{}' failed: {}", title, e);
                    }
                }
            }
            self.needs_window_scan = false;
            self.last_window_scan = Instant::now();
        }

        // Apply panel slider changes (preview width/height). No sync
        // round-trips here, so safe on every 100ms tick.
        self.apply_live_size();
        self.apply_live_opacity();
        self.apply_live_borderless();
        // Catches the setting being toggled and previews created since
        // the last tick; the instant per-cycle response comes from the
        // same call at the end of update_active.
        self.apply_active_visibility();

        Ok(())
    }

    /// Hide the active client's preview (and reveal everything else) when
    /// the "hide active client's preview" setting is on. Maps/unmaps only
    /// on a state flip so it's cheap to call every tick and on every
    /// focus change. On a cycle, update_active flips `is_active` first,
    /// so re-running this hides the newly-active preview and remaps the
    /// one cycled away from.
    fn apply_active_visibility(&mut self) {
        let hide_active = self.live.lock_recover().hide_active_preview;
        for preview in self.previews.values_mut() {
            let want_hidden = preview_should_hide(hide_active, preview.is_active);
            if want_hidden == preview.hidden {
                continue;
            }
            preview.hidden = want_hidden;
            if want_hidden {
                let _ = self.conn.unmap_window(preview.window);
            } else {
                let _ = self.conn.map_window(preview.window);
                // Backing contents are undefined after a remap; force the
                // loop-tick repaint to re-present a complete frame.
                preview.dirty = true;
            }
        }
    }

    /// Read LiveSettings.preview_opacity; if it changed since the last
    /// reconcile, push `_NET_WM_WINDOW_OPACITY` to every preview window
    /// so the compositor reblends them. New previews get their opacity
    /// at creation (see create_preview), so this only handles slider
    /// moves. change_property is async (no reply), cheap on every tick.
    fn apply_live_opacity(&mut self) {
        let want = self.live.lock_recover().preview_opacity;
        if want == self.last_applied_opacity {
            return;
        }
        self.last_applied_opacity = want;
        let cardinal = opacity_to_cardinal(want);
        for preview in self.previews.values() {
            let _ = self.conn.change_property32(
                PropMode::REPLACE,
                preview.window,
                self.atoms.net_wm_window_opacity,
                AtomEnum::CARDINAL,
                &[cardinal],
            );
        }
    }

    /// Read LiveSettings.borderless; if it flipped since the last
    /// reconcile, rebuild each preview's downscale transform for the new
    /// thumbnail rect (full-window vs. inset) and force a full repaint so
    /// the chrome appears/disappears live without a daemon restart.
    fn apply_live_borderless(&mut self) {
        let want = self.live.lock_recover().borderless;
        if want == self.last_applied_borderless {
            return;
        }
        self.last_applied_borderless = want;

        let keys: Vec<String> = self.previews.keys().cloned().collect();
        for name in keys {
            let (src_picture, source_w, source_h, w, h) = {
                let Some(p) = self.previews.get(&name) else {
                    continue;
                };
                (p.src_picture, p.source_w, p.source_h, p.width, p.height)
            };
            let (_, _, thumb_w, thumb_h) = thumbnail_rect(w, h, want);
            let _ =
                apply_scale_transform(&self.conn, src_picture, source_w, source_h, thumb_w, thumb_h);
            if let Some(p) = self.previews.get_mut(&name) {
                p.dirty = true;
            }
            let _ = self.paint_preview_now(&name);
        }
    }

    /// Read LiveSettings.preview_width / preview_height; if either has
    /// changed since the last reconcile, resize every preview window,
    /// rebuild the downscale transform against the new thumbnail area,
    /// and repaint chrome. No-op when nothing changed.
    fn apply_live_size(&mut self) {
        let (want_w, want_h) = {
            let live = self.live.lock_recover();
            (live.preview_width as u16, live.preview_height as u16)
        };
        if want_w == 0 || want_h == 0 {
            return;
        }
        if (want_w, want_h) == self.last_applied_size {
            return;
        }
        self.last_applied_size = (want_w, want_h);

        let screen_depth = self.conn.setup().roots[self.screen_num].root_depth;
        let pic_aux = RenderCreatePictureAux::new().subwindowmode(SubwindowMode::INCLUDE_INFERIORS);

        // Snapshot the keys + per-preview source dims so the mutation
        // pass below doesn't fight the borrow checker over `self`.
        let preview_keys: Vec<String> = self.previews.keys().cloned().collect();
        for name in preview_keys {
            let (src_picture, source_w, source_h, window, old_render_pixmap, old_render_picture) = {
                let Some(preview) = self.previews.get_mut(&name) else {
                    continue;
                };
                if preview.width == want_w && preview.height == want_h {
                    continue;
                }
                preview.width = want_w;
                preview.height = want_h;
                (
                    preview.src_picture,
                    preview.source_w,
                    preview.source_h,
                    preview.window,
                    preview.render_pixmap,
                    preview.render_picture,
                )
            };

            let _ = self.conn.configure_window(
                window,
                &ConfigureWindowAux::new()
                    .width(want_w as u32)
                    .height(want_h as u32),
            );
            // Render pixmap is fixed-size in X11; allocate a fresh one
            // for the new dimensions. The old pixmap stays alive for
            // any in-flight present that referenced it.
            let Ok(new_pixmap) = self.conn.generate_id() else {
                continue;
            };
            if self
                .conn
                .create_pixmap(screen_depth, new_pixmap, window, want_w, want_h)
                .is_err()
            {
                continue;
            }
            let Ok(new_picture) = self.conn.generate_id() else {
                continue;
            };
            if self
                .conn
                .render_create_picture(new_picture, new_pixmap, self.visual_format, &pic_aux)
                .is_err()
            {
                continue;
            }
            let _ = self.conn.render_free_picture(old_render_picture);
            let _ = self.conn.free_pixmap(old_render_pixmap);
            if let Some(preview) = self.previews.get_mut(&name) {
                preview.render_pixmap = new_pixmap;
                preview.render_picture = new_picture;
                preview.dirty = true;
            }

            let (thumb_w, thumb_h) = thumbnail_size(want_w, want_h);
            let _ = apply_scale_transform(
                &self.conn,
                src_picture,
                source_w,
                source_h,
                thumb_w,
                thumb_h,
            );
            // Push a fresh frame on the new pixmap (chrome + composite +
            // present). The render_pixmap was just allocated and its
            // contents are undefined until we paint.
            let _ = self.paint_preview_now(&name);
        }
    }

    /// Enumerate EVE client windows from `_NET_CLIENT_LIST`. Same X11
    /// path on all sessions — Wayland users get them via XWayland.
    fn find_eve_windows(&self) -> Result<Vec<(Window, String)>> {
        let screen = &self.conn.setup().roots[self.screen_num];
        let root = screen.root;

        let reply = self
            .conn
            .get_property(
                false,
                root,
                self.atoms.net_client_list,
                AtomEnum::WINDOW,
                0,
                u32::MAX,
            )?
            .reply()?;
        let ids: Vec<u32> = reply
            .value32()
            .ok_or_else(|| anyhow::anyhow!("_NET_CLIENT_LIST not 32-bit"))?
            .collect();

        let mut out = Vec::new();
        for id in ids {
            if let Ok(title) = self.window_title(id) {
                // Title prefix is the quick gate — most windows on the
                // root client list don't start with "EVE - " so we
                // skip the /proc read for them.
                if !title.starts_with("EVE - ") {
                    continue;
                }
                // The reliable gate: confirm the underlying process is
                // the actual EVE client (`exefile.exe`). This filters
                // out browser tabs, Discord channels, and other "EVE -"
                // titled windows that match the title heuristic but
                // aren't the game.
                let Some(pid) = self.read_net_wm_pid(id) else {
                    continue;
                };
                if !crate::eve_match::pid_is_eve_client(pid) {
                    continue;
                }
                let stripped = title.trim_start_matches("EVE - ").to_string();
                out.push((id, stripped));
            }
        }
        Ok(out)
    }

    /// Read `_NET_WM_PID` from a window. Returns None if the property
    /// is missing, malformed, or zero — we treat any of those as "we
    /// don't know which process owns this", which in find_eve_windows
    /// means we conservatively reject the candidate rather than risk
    /// creating a preview for a non-EVE app.
    fn read_net_wm_pid(&self, window: u32) -> Option<u32> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.net_wm_pid,
                AtomEnum::CARDINAL,
                0,
                1,
            )
            .ok()?
            .reply()
            .ok()?;
        let pid = reply.value32()?.next()?;
        if pid == 0 {
            None
        } else {
            Some(pid)
        }
    }

    fn window_title(&self, window: u32) -> Result<String> {
        let reply = self
            .conn
            .get_property(
                false,
                window,
                self.atoms.net_wm_name,
                self.atoms.utf8_string,
                0,
                1024,
            )?
            .reply()?;
        if !reply.value.is_empty() {
            if let Ok(s) = String::from_utf8(reply.value.clone()) {
                return Ok(s);
            }
        }
        let reply = self
            .conn
            .get_property(false, window, AtomEnum::WM_NAME, AtomEnum::STRING, 0, 1024)?
            .reply()?;
        Ok(String::from_utf8_lossy(&reply.value).to_string())
    }

    /// Send an EWMH `_NET_ACTIVE_WINDOW` ClientMessage so the WM
    /// foregrounds the source EVE client. Mirrors the same call X11
    /// WindowManager.activate_window() makes; staying inline here lets
    /// click-to-activate work even when the daemon's WindowManager is
    /// a Wayland-native impl (Sway / Hyprland) whose internal ids
    /// don't match X11 window ids.
    fn activate_source(&self, target: u32) -> Result<()> {
        let screen = &self.conn.setup().roots[self.screen_num];
        let root = screen.root;
        let event = ClientMessageEvent::new(
            32,
            target,
            self.atoms.net_active_window,
            // EWMH source: 2 = pager (so the WM doesn't apply the
            // anti-focus-stealing penalty it might apply to source 1).
            // CURRENT_TIME (0) is fine for synthetic events; some WMs
            // require a real timestamp to honor the request, but KWin
            // / Mutter / Sway accept CURRENT_TIME.
            [2, x11rb::CURRENT_TIME, 0, 0, 0],
        );
        self.conn.send_event(
            false,
            root,
            EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
            event,
        )?;
        // SetInputFocus as a fallback for compositors that ignore the
        // EWMH message under tight focus-stealing-prevention rules.
        let _ = self
            .conn
            .set_input_focus(InputFocus::PARENT, target, x11rb::CURRENT_TIME);
        self.conn.flush()?;
        Ok(())
    }

    /// Snapshot rectangles of every preview / list window EXCEPT the
    /// surface currently being dragged. Used by the drag motion handler
    /// to feed snap-to-dock. Without the exclude, a dragged window
    /// would always include its own rect in the snap targets and end
    /// up perpetually trying to snap to its previous position — the
    /// drag would feel like it was on a grid.
    fn collect_other_rects(&self, dragged: &DragTarget) -> Vec<DragRect> {
        let mut rects: Vec<DragRect> = self
            .previews
            .iter()
            .filter(|(n, _)| !matches!(dragged, DragTarget::Preview(name) if name == *n))
            .map(|(_, p)| DragRect {
                left: p.x as i32,
                top: p.y as i32,
                right: p.x as i32 + p.width as i32,
                bottom: p.y as i32 + p.height as i32,
            })
            .collect();
        if let Some(list) = &self.list_window {
            if !matches!(dragged, DragTarget::List) {
                rects.push(DragRect {
                    left: list.x as i32,
                    top: list.y as i32,
                    right: list.x as i32 + list.width as i32,
                    bottom: list.y as i32 + list.height as i32,
                });
            }
        }
        rects
    }

    /// Best-effort screen-bounds check. Used to discard saved positions
    /// that came from a different monitor configuration. Reads only
    /// the root window's geometry — multi-monitor setups present as a
    /// single stitched root in the X server, so this catches the
    /// common "saved to a now-disconnected display" case.
    fn position_on_screen(&self, x: i32, y: i32) -> bool {
        let root = self.conn.setup().roots[self.screen_num].root;
        // get_geometry returns Result<Cookie, ConnectionError> and the
        // cookie's reply() returns Result<_, ReplyError> — different
        // error types. Two `?`-style unwraps via Option are the
        // simplest way to bail to "trust the caller" on either failure.
        let Some(cookie) = self.conn.get_geometry(root).ok() else {
            return true;
        };
        let Some(reply) = cookie.reply().ok() else {
            return true;
        };
        x >= reply.x as i32
            && y >= reply.y as i32
            && x < reply.x as i32 + reply.width as i32
            && y < reply.y as i32 + reply.height as i32
    }

    /// Re-query `_NET_ACTIVE_WINDOW` and flip is_active on whichever
    /// preview's source matches (and unflips the previously-active one).
    /// Triggers a chrome repaint on every preview whose state changed.
    /// In list mode, repaints the whole list instead — the active row's
    /// color flips between red and black.
    fn update_active(&mut self) -> Result<()> {
        let screen = &self.conn.setup().roots[self.screen_num];
        let new_active =
            current_active_window(&self.conn, screen.root, self.atoms.net_active_window)
                .unwrap_or(0);
        // Ignore transients where KWin briefly reports one of our own
        // override_redirect surfaces (a preview or the list window) as
        // _NET_ACTIVE_WINDOW. This happens on Wayland during focus
        // changes — typically right when we restack after a client
        // switch. Letting the value through would flip every preview's
        // is_active to false for one frame before the real new active
        // value lands, producing the red→black→red flash on the
        // newly-active preview.
        let is_our_window = self.previews.values().any(|p| p.window == new_active)
            || self
                .list_window
                .as_ref()
                .is_some_and(|l| l.window == new_active);
        if is_our_window {
            return Ok(());
        }
        // Resolve the new active window's character name (None if the
        // foreground isn't an EVE client). List mode highlights by name
        // since Wayland WindowManager impls don't share id-space with
        // X11.
        let new_active_character = if new_active == 0 {
            None
        } else {
            self.window_title(new_active)
                .ok()
                .filter(|t| t.starts_with("EVE - "))
                .map(|t| t.trim_start_matches("EVE - ").to_string())
        };
        // Compare both id AND character. Comparing id alone misses the
        // startup case where active_id is already set from the initial
        // query but active_character is still None (no PropertyNotify
        // has fired yet to resolve the name).
        if new_active == self.active_id && new_active_character == self.active_character {
            return Ok(());
        }
        self.active_id = new_active;
        self.active_character = new_active_character;

        // Per-preview chrome flip in Previews mode. Collect keys first
        // to avoid mutable / immutable borrow tangles.
        let mut to_repaint: Vec<String> = Vec::new();
        for (name, preview) in self.previews.iter() {
            let now_active = preview.source_id == new_active;
            if now_active != preview.is_active {
                to_repaint.push(name.clone());
            }
        }
        for name in &to_repaint {
            if let Some(preview) = self.previews.get_mut(name) {
                preview.is_active = preview.source_id == new_active;
            }
        }
        // Hide the now-active preview / reveal the one cycled away from,
        // before the paint loop — so a revealed preview is mapped in time
        // for the present below instead of waiting for the next tick.
        self.apply_active_visibility();
        // Trigger a fresh present for each affected preview. Chrome is
        // repainted inside paint_preview_now using the just-updated
        // is_active flag, so we don't paint chrome separately here —
        // doing so would race with any in-flight Present referencing the
        // same render_pixmap.
        for name in &to_repaint {
            let _ = self.paint_preview_now(name);
        }

        // List-mode active-row highlight. Repaint the whole list — the
        // cost is a handful of XRender calls and we don't need
        // per-row dirty tracking at this granularity.
        if matches!(self.current_mode, DisplayMode::List) {
            let _ = self.paint_list();
        }

        Ok(())
    }
}

fn atom(conn: &RustConnection, name: &[u8]) -> Result<u32> {
    Ok(conn.intern_atom(false, name)?.reply()?.atom)
}

/// Map an opacity percent (slider range 10..=100) to the CARDINAL value
/// `_NET_WM_WINDOW_OPACITY` expects: 0 = fully transparent,
/// 0xFFFF_FFFF = fully opaque. 100% maps exactly to u32::MAX.
pub(super) fn opacity_to_cardinal(percent: u32) -> u32 {
    let frac = percent.min(100) as f64 / 100.0;
    (frac * u32::MAX as f64) as u32
}

/// Which surface a drag started on. Previews are keyed by character
/// name; the list window is a singleton.
enum DragTarget {
    Preview(String),
    List,
}

/// Read the X server's notion of the current foreground window.
/// `_NET_ACTIVE_WINDOW` is set by the WM (KWin / Mutter / Sway+xwayland
/// shim) on every focus change. Returns None if the property isn't
/// present (e.g., before any X11 client has been focused this session).
fn current_active_window(
    conn: &RustConnection,
    root: Window,
    net_active_window: u32,
) -> Option<u32> {
    let reply = conn
        .get_property(false, root, net_active_window, AtomEnum::WINDOW, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    reply.value32().and_then(|mut it| it.next())
}

#[cfg(test)]
mod tests;
