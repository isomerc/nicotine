use crate::config::{Config, LiveSettings};
use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use crate::windows_manager::{hwnd_to_id, id_to_hwnd};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Dwm::{
    DwmRegisterThumbnail, DwmUnregisterThumbnail, DwmUpdateThumbnailProperties,
    DWM_THUMBNAIL_PROPERTIES, DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_VISIBLE,
};
use windows::Win32::Graphics::Gdi::{
    AddFontMemResourceEx, BeginPaint, ClientToScreen, CreateFontIndirectW, CreateSolidBrush,
    DeleteObject, DrawTextW, EndPaint, FillRect, InvalidateRect, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, FW_NORMAL, HFONT, LOGFONTW, OUT_TT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::GetDpiForSystem;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, GetWindowRect, KillTimer, LoadCursorW, RegisterClassExW,
    SetTimer, SetWindowLongPtrW, SetWindowPos, TranslateMessage, EVENT_SYSTEM_FOREGROUND,
    GWLP_USERDATA, HCURSOR, HICON, HMENU, HWND_TOPMOST, IDC_ARROW, MSG, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, WINEVENT_OUTOFCONTEXT, WM_DESTROY, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_VISIBLE,
};

/// Type alias for DWM thumbnail handles. windows-rs 0.59 doesn't expose a
/// named Hthumbnail type — DwmRegisterThumbnail returns isize directly and
/// DwmUnregisterThumbnail takes isize.
type Hthumbnail = isize;

/// Extract (x, y) from a Win32 LPARAM that packs them as low/high words.
/// The cast chain `as u16 as i16 as i32` performs the necessary
/// sign-extension so coordinates left/above the window during a captured
/// drag come back as small negatives instead of huge positives.
fn unpack_xy(lparam: LPARAM) -> (i32, i32) {
    let raw = lparam.0 as u32;
    let x = (raw as u16) as i16 as i32;
    let y = ((raw >> 16) as u16) as i16 as i32;
    (x, y)
}

/// Pointer to the live PreviewManager, stored as a usize so the WinEvent
/// callback (which can't capture environment) can find it. Set when the
/// manager thread starts and cleared on shutdown. SAFETY: only written by
/// the manager thread; the WinEvent callback runs on the same thread.
static MANAGER_PTR: AtomicUsize = AtomicUsize::new(0);

/// Read the shared `positions_locked` flag. Used by preview + list
/// window drag handlers to ignore mouse drags when the user has locked
/// the layout in the config panel.
fn positions_locked() -> bool {
    let ptr = MANAGER_PTR.load(Ordering::Acquire) as *const PreviewManager;
    if ptr.is_null() {
        return false;
    }
    unsafe { (*ptr).live.lock().unwrap().positions_locked }
}

/// WinEvent hook callback for EVENT_SYSTEM_FOREGROUND. Fires synchronously
/// on every system-wide foreground change. Routes the new HWND into the
/// manager so the active-client outline updates immediately instead of
/// waiting up to 500ms for the next reconcile tick.
unsafe extern "system" fn foreground_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    if event != EVENT_SYSTEM_FOREGROUND {
        return;
    }
    let ptr = MANAGER_PTR.load(Ordering::Acquire) as *mut PreviewManager;
    if ptr.is_null() {
        return;
    }
    (*ptr).update_active(hwnd_to_id(hwnd));
}

/// Adjust a proposed window position so it docks to nearby preview windows.
/// Each axis snaps independently so you can edge-touch on one side while
/// also aligning a perpendicular edge (e.g., dock to the right of A AND
/// align tops). The first match per axis wins — once snapped, further
/// candidates on that axis are ignored to avoid thrash when previews are
/// stacked tightly.
fn snap_position(
    proposed_x: i32,
    proposed_y: i32,
    width: i32,
    height: i32,
    others: &[RECT],
) -> (i32, i32) {
    let mut x = proposed_x;
    let mut y = proposed_y;
    let mut x_snapped = false;
    let mut y_snapped = false;
    let drag_right = proposed_x + width;
    let drag_bottom = proposed_y + height;
    let snap = px(SNAP_THRESHOLD);

    for other in others {
        // Edge-to-edge docking on the X axis.
        if !x_snapped && (drag_right - other.left).abs() <= snap {
            x = other.left - width;
            x_snapped = true;
        }
        if !x_snapped && (proposed_x - other.right).abs() <= snap {
            x = other.right;
            x_snapped = true;
        }
        // Edge-to-edge docking on the Y axis.
        if !y_snapped && (drag_bottom - other.top).abs() <= snap {
            y = other.top - height;
            y_snapped = true;
        }
        if !y_snapped && (proposed_y - other.bottom).abs() <= snap {
            y = other.bottom;
            y_snapped = true;
        }
        // Parallel-edge alignment so docked previews share a baseline.
        if !y_snapped && (proposed_y - other.top).abs() <= snap {
            y = other.top;
            y_snapped = true;
        }
        if !y_snapped && (drag_bottom - other.bottom).abs() <= snap {
            y = other.bottom - height;
            y_snapped = true;
        }
        if !x_snapped && (proposed_x - other.left).abs() <= snap {
            x = other.left;
            x_snapped = true;
        }
        if !x_snapped && (drag_right - other.right).abs() <= snap {
            x = other.right - width;
            x_snapped = true;
        }
    }

    (x, y)
}

/// True if (x, y) lies somewhere inside the multi-monitor virtual screen.
/// Used to reject saved positions from a previous build that wrote
/// off-screen coordinates due to a sign-extension bug — without this,
/// affected previews spawn invisible at coords like (65000, 1000).
fn position_on_screen(x: i32, y: i32) -> bool {
    unsafe {
        let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
        let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
        x >= vx && y >= vy && x < vx + vw && y < vy + vh
    }
}

/// System DPI scale factor (1.0 at 96 DPI, 1.5 at 150%, 2.0 at 200%).
/// Cached once per process — we're SYSTEM_AWARE, so the value is fixed
/// at process start and doesn't change as windows move between monitors.
fn dpi_scale() -> f32 {
    static CACHED: OnceLock<f32> = OnceLock::new();
    *CACHED.get_or_init(|| unsafe { GetDpiForSystem() as f32 / 96.0 })
}

/// Scale a reference pixel value (authored at 96 DPI) to actual physical
/// pixels on the current display. Preserves sign so negative font
/// heights round correctly.
fn px(n: i32) -> i32 {
    (n as f32 * dpi_scale()).round() as i32
}

const PREVIEW_CLASS: &str = "NicotinePreviewWnd\0";
const CONTROL_CLASS: &str = "NicotinePreviewCtrl\0";
const LIST_CLASS: &str = "NicotineListWnd\0";

// Chrome dimensions below are "reference pixels" at 96 DPI. Every use
// site wraps them in `px(...)` so they render at the correct physical
// size on high-DPI displays (e.g. 4K @ 150% scaling).

/// Reference dimensions for the client-list window (96 DPI).
const LIST_WIDTH: i32 = 260;
const LIST_ROW_HEIGHT: i32 = 24;
const LIST_PADDING: i32 = 6;
const RECONCILE_TIMER_ID: usize = 1;
/// Reconcile tick interval in ms. Needs to be snappy enough that slider
/// drags in the config panel feel live. 100ms = 10fps — enough for size
/// changes to track a dragging slider without a visible lag.
const RECONCILE_INTERVAL_MS: u32 = 100;
const TITLE_HEIGHT: i32 = 24;
const BORDER_WIDTH: i32 = 3;
const DRAG_THRESHOLD_PX: i32 = 4;
/// Reference-pixel grace band within which a dragged preview snaps to
/// align with another preview's edge. Generous enough to make docking
/// feel deliberate but tight enough that you can place windows freely
/// between previews.
const SNAP_THRESHOLD: i32 = 12;

/// Win32 COLORREF is 0x00BBGGRR. Nicotine red is RGB(196, 30, 58).
const NICOTINE_RED: COLORREF = COLORREF(0x003A_1EC4);
/// Dark chrome color used for inactive previews — same as the title strip
/// so the border blends in until a client becomes active.
const CHROME_DARK: COLORREF = COLORREF(0x0000_0000);
/// Cream background for the list window body (RGB 252, 250, 242).
const NICOTINE_CREAM: COLORREF = COLORREF(0x00F2_FAFC);
/// Text color for inactive rows in the list window.
const LIST_TEXT_BLACK: COLORREF = COLORREF(0x0000_0000);

/// Per-character preview window position. Persisted to disk so previews come
/// back at the same place across daemon restarts.
#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct PreviewPositions {
    #[serde(default)]
    pub positions: HashMap<String, (i32, i32)>,
    /// Last-known position of the single list-view window (Display Mode =
    /// List). Saved on drag-end, restored on window (re)create. Without
    /// this, toggling display mode would reset the list window back to
    /// its default spawn position every time.
    #[serde(default)]
    pub list_position: Option<(i32, i32)>,
}

impl PreviewPositions {
    fn config_path() -> PathBuf {
        let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        p.push("nicotine");
        p.push("preview_positions.toml");
        p
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(p) = toml::from_str::<Self>(&s) {
                return p;
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&path, s);
        }
    }

    fn get(&self, name: &str) -> Option<(i32, i32)> {
        self.positions.get(name).copied()
    }

    fn set(&mut self, name: String, x: i32, y: i32) {
        self.positions.insert(name, (x, y));
    }
}

/// State for a single preview window. The pointer is stored in the
/// window's GWLP_USERDATA so the wnd_proc can recover it.
struct PreviewWindowState {
    source_id: u32,
    character_name: String,
    thumbnail: Hthumbnail,
    wm: Arc<dyn WindowManager>,
    positions: Arc<Mutex<PreviewPositions>>,
    drag_active: bool,
    dragged: bool,
    drag_origin_screen: (i32, i32),
    drag_origin_window: (i32, i32),
    /// True when this preview's source EVE client is the system foreground
    /// window. Read from WM_PAINT to choose border color. Updated by
    /// reconcile via the GWLP_USERDATA pointer.
    is_active: bool,
}

/// One owned preview window. Drop unregisters the DWM thumbnail.
struct OwnedPreview {
    hwnd: HWND,
    source_id: u32,
    /// Mirror of `PreviewWindowState.is_active` kept here so reconcile
    /// can detect changes without dereferencing the GWLP_USERDATA pointer
    /// on every tick.
    is_active: bool,
}

impl Drop for OwnedPreview {
    fn drop(&mut self) {
        unsafe {
            // Box-drop the per-window state stored in GWLP_USERDATA.
            let ptr = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA);
            if ptr != 0 {
                let state = Box::from_raw(ptr as *mut PreviewWindowState);
                let _ = DwmUnregisterThumbnail(state.thumbnail);
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

struct PreviewManager {
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    config: Config,
    positions: Arc<Mutex<PreviewPositions>>,
    previews: HashMap<String, OwnedPreview>,
    /// Where to drop the next never-seen-before preview. Increments
    /// diagonally so multiple new clients don't stack on top of each other.
    next_default_offset: i32,
    /// Shared settings watched for live updates (e.g. slider drags in the
    /// config panel). Checked on every reconcile tick; any difference from
    /// the cached config values triggers a resize pass over all previews.
    live: Arc<Mutex<LiveSettings>>,
    /// Mirror of `live.display_mode` from the last reconcile. Used to
    /// detect transitions so we can tear down the outgoing mode's windows
    /// before spawning the incoming mode's.
    current_mode: crate::config::DisplayMode,
    /// Optional single-window list view; populated when `display_mode` is
    /// `List`. Drop destroys the window.
    list: Option<OwnedListWindow>,
    /// Most recent foreground EVE window id. Used by the list window's
    /// paint callback to decide which row to highlight.
    active_id: u32,
    /// Names rendered in the list window on the previous reconcile, in
    /// order. Used to detect changes (add/remove/reorder) so we only
    /// invalidate when something actually shifted — calling
    /// InvalidateRect every 100ms otherwise produces a visible flicker
    /// and feels sluggish.
    list_last_names: Vec<String>,
}

/// Drop-guard for the list window — destroys the Win32 window and the
/// heap-allocated drag state stored in its GWLP_USERDATA.
struct OwnedListWindow {
    hwnd: HWND,
}

impl Drop for OwnedListWindow {
    fn drop(&mut self) {
        unsafe {
            let ptr = GetWindowLongPtrW(self.hwnd, GWLP_USERDATA);
            if ptr != 0 {
                drop(Box::from_raw(ptr as *mut ListWindowState));
                SetWindowLongPtrW(self.hwnd, GWLP_USERDATA, 0);
            }
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Per-window mutable state for the list window — mostly drag tracking.
/// A Box<ListWindowState> is stashed in the window's GWLP_USERDATA and
/// reclaimed when `OwnedListWindow` drops.
struct ListWindowState {
    drag_active: bool,
    dragged: bool,
    drag_origin_screen: (i32, i32),
    drag_origin_window: (i32, i32),
}

impl PreviewManager {
    fn reconcile(&mut self) {
        // Check whether the user toggled display mode since the last
        // reconcile; if so, tear down the outgoing mode's windows.
        let target_mode = self.live.lock().unwrap().display_mode;
        if target_mode != self.current_mode {
            match target_mode {
                crate::config::DisplayMode::Previews => self.list = None,
                crate::config::DisplayMode::List => self.previews.clear(),
            }
            self.current_mode = target_mode;
        }

        match self.current_mode {
            crate::config::DisplayMode::Previews => self.reconcile_previews(),
            crate::config::DisplayMode::List => self.reconcile_list(),
        }

        // Polling fallback in case the WinEvent hook missed something
        // (rare). The hook is the primary path and updates instantly.
        let active_id = self.wm.get_active_window().unwrap_or(0);
        self.update_active(active_id);
    }

    fn reconcile_previews(&mut self) {
        // Apply any pending live-settings changes first — this lets the
        // user drag the size sliders in the config panel and see preview
        // windows resize in real time.
        self.apply_live_size();

        let windows = {
            let s = self.state.lock().unwrap();
            s.get_windows().to_vec()
        };

        // Drop previews whose source EVE client is no longer present.
        let live_names: std::collections::HashSet<String> =
            windows.iter().map(|w| w.title.clone()).collect();
        self.previews.retain(|name, _| live_names.contains(name));

        // Spawn previews for new EVE clients; rebind thumbnails if HWNDs
        // changed (EVE relaunched into the same character slot).
        for window in &windows {
            if let Some(existing) = self.previews.get(&window.title) {
                if existing.source_id != window.id {
                    let _ = self.rebind_preview(window);
                }
            } else if let Err(e) = self.create_preview(window) {
                eprintln!("Failed to create preview for {}: {}", window.title, e);
            }
        }

        // Re-assert topmost Z-order every tick.
        for preview in self.previews.values() {
            unsafe {
                let _ = SetWindowPos(
                    preview.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn reconcile_list(&mut self) {
        // Spawn the list window on first entry into this mode, or if it
        // was torn down somehow.
        if self.list.is_none() {
            if let Err(e) = self.create_list_window() {
                eprintln!("Failed to create list window: {}", e);
                return;
            }
        }

        // Pull the current ordered character list (stable, keyed on
        // characters.txt) so we can compare against what we last painted.
        let ordered_names: Vec<String> = {
            let s = self.state.lock().unwrap();
            s.get_ordered_windows()
                .into_iter()
                .map(|w| w.title)
                .collect()
        };
        let names_changed = ordered_names != self.list_last_names;
        let count = ordered_names.len();

        if let Some(list) = &self.list {
            // Resize only when the count actually changed — SetWindowPos
            // with NOSIZE/NOMOVE is cheap, but SetWindowPos with size
            // triggers a repaint.
            if names_changed {
                let target_h = list_window_height(count);
                let mut rect = RECT::default();
                unsafe {
                    let _ = GetWindowRect(list.hwnd, &mut rect);
                }
                let current_h = rect.bottom - rect.top;
                if current_h != target_h {
                    unsafe {
                        let _ = SetWindowPos(
                            list.hwnd,
                            Some(HWND_TOPMOST),
                            0,
                            0,
                            px(LIST_WIDTH),
                            target_h,
                            SWP_NOMOVE | SWP_NOACTIVATE,
                        );
                    }
                }
                // Repaint once. berase=false because paint_list fills
                // the full window; no need to clear first.
                unsafe {
                    let _ = InvalidateRect(Some(list.hwnd), None, false);
                }
            }

            // Re-assert topmost Z-order every tick; NOMOVE/NOSIZE/NOACTIVATE
            // is a Z-only change, no repaint, no flicker.
            unsafe {
                let _ = SetWindowPos(
                    list.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }

        self.list_last_names = ordered_names;
    }

    fn create_list_window(&mut self) -> Result<()> {
        let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
        let class_name: Vec<u16> = LIST_CLASS.encode_utf16().collect();
        let title: Vec<u16> = "Nicotine\0".encode_utf16().collect();

        let windows_len = self.state.lock().unwrap().get_windows().len();
        let height = list_window_height(windows_len);

        // Restore the last-known list-view position if we have one and
        // it's still on screen (handles saved coords from a prior setup
        // with a now-disconnected monitor).
        let (x, y) = self
            .positions
            .lock()
            .unwrap()
            .list_position
            .filter(|(x, y)| position_on_screen(*x, *y))
            .unwrap_or((20, 20));

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                px(LIST_WIDTH),
                height,
                None,
                None,
                Some(HINSTANCE(module.0)),
                None,
            )
        }
        .context("CreateWindowExW failed for list window")?;

        let state = Box::new(ListWindowState {
            drag_active: false,
            dragged: false,
            drag_origin_screen: (0, 0),
            drag_origin_window: (0, 0),
        });
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        self.list = Some(OwnedListWindow { hwnd });
        Ok(())
    }

    /// Read the shared LiveSettings and, if the user has adjusted preview
    /// size, resize every preview window and update its DWM thumbnail
    /// rect. No-op when nothing has changed.
    fn apply_live_size(&mut self) {
        let (want_w, want_h) = {
            let live = self.live.lock().unwrap();
            (live.preview_width, live.preview_height)
        };
        if want_w == self.config.preview_width && want_h == self.config.preview_height {
            return;
        }
        self.config.preview_width = want_w;
        self.config.preview_height = want_h;
        let w = want_w as i32;
        let h = want_h as i32;
        for preview in self.previews.values() {
            unsafe {
                // Resize the window without touching its position or z-order.
                let _ = SetWindowPos(
                    preview.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    w,
                    h,
                    SWP_NOMOVE | SWP_NOACTIVATE,
                );
                // Recompute the thumbnail destination rect against the
                // new window size so the mirror fills the new area.
                let ptr =
                    GetWindowLongPtrW(preview.hwnd, GWLP_USERDATA) as *const PreviewWindowState;
                if !ptr.is_null() {
                    update_thumbnail_rect((*ptr).thumbnail, w, h);
                }
                // Repaint title strip + border at the new dimensions.
                let _ = InvalidateRect(Some(preview.hwnd), None, true);
            }
        }
    }

    /// Snapshot of all preview window rects in screen coordinates,
    /// excluding the one identified by `exclude`. Used by the drag handler
    /// for snap-to-dock calculations.
    fn collect_other_rects(&self, exclude: HWND) -> Vec<RECT> {
        let mut rects = Vec::with_capacity(self.previews.len());
        for preview in self.previews.values() {
            if preview.hwnd == exclude {
                continue;
            }
            let mut rect = RECT::default();
            if unsafe { GetWindowRect(preview.hwnd, &mut rect) }.is_ok() {
                rects.push(rect);
            }
        }
        rects
    }

    /// Set the active-client highlight to whichever preview matches
    /// `active_id`. Cheap to call repeatedly — only invalidates and
    /// repaints when a preview's state actually flips.
    ///
    /// Also keeps the cycle's `current_index` in sync with whatever EVE
    /// window the user has manually focused (via mouse click, Alt-Tab,
    /// etc.). Without this, `current_index` only updates when our own
    /// cycle commands run — so if the user activates B by hand, then
    /// focuses a non-EVE app, then presses F11, the cycle would step
    /// from wherever we last cycled to (say A) instead of from B,
    /// looking like "cycle skipped a client."
    fn update_active(&mut self, active_id: u32) {
        self.state.lock().unwrap().sync_with_active(active_id);

        for preview in self.previews.values_mut() {
            let now_active = preview.source_id == active_id;
            if preview.is_active == now_active {
                continue;
            }
            preview.is_active = now_active;
            unsafe {
                let ptr = GetWindowLongPtrW(preview.hwnd, GWLP_USERDATA) as *mut PreviewWindowState;
                if !ptr.is_null() {
                    (*ptr).is_active = now_active;
                }
                let _ = InvalidateRect(Some(preview.hwnd), None, true);
            }
        }

        // Repaint the list window whenever the active client changes so
        // the red + cigarette row follows the real foreground window.
        // berase=false because paint_list fills the full window — no
        // need to erase first (and erase + paint flickers).
        if self.active_id != active_id {
            self.active_id = active_id;
            if let Some(list) = &self.list {
                unsafe {
                    let _ = InvalidateRect(Some(list.hwnd), None, false);
                }
            }
        }
    }

    fn create_preview(&mut self, window: &crate::window_manager::EveWindow) -> Result<()> {
        let (x, y) = self
            .positions
            .lock()
            .unwrap()
            .get(&window.title)
            .filter(|(x, y)| position_on_screen(*x, *y))
            .unwrap_or_else(|| {
                let off = self.next_default_offset;
                self.next_default_offset = (self.next_default_offset + 32) % 320;
                (px(10 + off), px(10 + off))
            });

        let width = self.config.preview_width as i32;
        let height = self.config.preview_height as i32;

        let class_name: Vec<u16> = PREVIEW_CLASS.encode_utf16().collect();
        let title_w: Vec<u16> = window
            .title
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;

        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                width,
                height,
                None,
                None,
                Some(HINSTANCE(module.0)),
                None,
            )
        }
        .context("CreateWindowExW failed for preview window")?;

        // Register a DWM thumbnail mirroring the EVE source HWND into our
        // window's client area below the title strip.
        let thumbnail: Hthumbnail = unsafe {
            DwmRegisterThumbnail(hwnd, id_to_hwnd(window.id))
                .context("DwmRegisterThumbnail failed")?
        };
        update_thumbnail_rect(thumbnail, width, height);

        let per_window = Box::new(PreviewWindowState {
            source_id: window.id,
            character_name: window.title.clone(),
            thumbnail,
            wm: Arc::clone(&self.wm),
            positions: Arc::clone(&self.positions),
            drag_active: false,
            dragged: false,
            drag_origin_screen: (0, 0),
            drag_origin_window: (0, 0),
            is_active: false,
        });
        unsafe {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(per_window) as isize);
        }

        // Belt-and-suspenders topmost — WS_EX_TOPMOST should already do it,
        // but DWM compositing sometimes drops the order on EVE startup races.
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }

        self.previews.insert(
            window.title.clone(),
            OwnedPreview {
                hwnd,
                source_id: window.id,
                is_active: false,
            },
        );
        Ok(())
    }

    fn rebind_preview(&mut self, window: &crate::window_manager::EveWindow) -> Result<()> {
        // The simplest correct rebind: drop the old preview and create a new
        // one. Position is preserved because it's keyed on character name.
        self.previews.remove(&window.title);
        self.create_preview(window)
    }
}

/// Recompute the DWM thumbnail destination rect. The thumbnail occupies
/// everything below the title strip, inset by BORDER_WIDTH on the sides
/// and bottom so we have margin to paint the active-client outline. We
/// mirror the whole source window (including any title bar/border) — EVE's
/// client area definition reportedly hides the actual game render surface,
/// so SOURCECLIENTAREAONLY gives a blank preview.
fn update_thumbnail_rect(thumbnail: Hthumbnail, width: i32, height: i32) {
    let border = px(BORDER_WIDTH);
    let title = px(TITLE_HEIGHT);
    let props = DWM_THUMBNAIL_PROPERTIES {
        dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
        rcDestination: RECT {
            left: border,
            top: title,
            right: width - border,
            bottom: height - border,
        },
        rcSource: RECT::default(),
        opacity: 255,
        fVisible: true.into(),
        fSourceClientAreaOnly: false.into(),
    };
    unsafe {
        let _ = DwmUpdateThumbnailProperties(thumbnail, &props);
    }
}

/// Window procedure for preview windows. Pulls per-window state from
/// GWLP_USERDATA. WM_DESTROY does not free the state — that happens in
/// `OwnedPreview::drop` so the manager owns the lifetime.
unsafe extern "system" fn preview_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PreviewWindowState;
    let state = if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    } else {
        &mut *state_ptr
    };

    match msg {
        WM_PAINT => {
            paint_chrome(hwnd, &state.character_name, state.is_active);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (client_x, client_y) = unpack_xy(lparam);
            let mut pt = POINT {
                x: client_x,
                y: client_y,
            };
            let _ = ClientToScreen(hwnd, &mut pt);

            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);

            state.drag_active = true;
            state.dragged = false;
            state.drag_origin_screen = (pt.x, pt.y);
            state.drag_origin_window = (rect.left, rect.top);
            let _ = SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // Positions locked: don't track motion. Keeping drag_active
            // true means the subsequent WM_LBUTTONUP's `!dragged` path
            // still fires, so click-to-activate keeps working.
            if state.drag_active && !positions_locked() {
                let (client_x, client_y) = unpack_xy(lparam);
                let mut pt = POINT {
                    x: client_x,
                    y: client_y,
                };
                let _ = ClientToScreen(hwnd, &mut pt);

                let dx = pt.x - state.drag_origin_screen.0;
                let dy = pt.y - state.drag_origin_screen.1;
                let threshold = px(DRAG_THRESHOLD_PX);
                if dx.abs() > threshold || dy.abs() > threshold {
                    state.dragged = true;
                }
                if state.dragged {
                    let mut new_x = state.drag_origin_window.0 + dx;
                    let mut new_y = state.drag_origin_window.1 + dy;

                    // Snap to dock with other previews if any of our edges
                    // come within SNAP_THRESHOLD of theirs.
                    let mut self_rect = RECT::default();
                    if GetWindowRect(hwnd, &mut self_rect).is_ok() {
                        let width = self_rect.right - self_rect.left;
                        let height = self_rect.bottom - self_rect.top;
                        let mgr_ptr = MANAGER_PTR.load(Ordering::Acquire) as *const PreviewManager;
                        if !mgr_ptr.is_null() {
                            let others = (*mgr_ptr).collect_other_rects(hwnd);
                            let (sx, sy) = snap_position(new_x, new_y, width, height, &others);
                            new_x = sx;
                            new_y = sy;
                        }
                    }

                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        new_x,
                        new_y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if state.drag_active {
                state.drag_active = false;
                let _ = ReleaseCapture();
                if state.dragged {
                    // Persist the new position keyed by character name.
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    let mut positions = state.positions.lock().unwrap();
                    positions.set(state.character_name.clone(), rect.left, rect.top);
                    positions.save();
                } else {
                    // No drag — treat as click-to-activate.
                    let _ = state.wm.activate_window(state.source_id);
                }
                state.dragged = false;
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            // State cleanup happens in OwnedPreview::drop. Don't double-free.
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Register JetBrains Mono + Marlboro with GDI from bytes embedded in the
/// binary. After this runs, CreateFontIndirectW can locate both fonts by
/// family name. Idempotent — GDI refcounts the underlying data on repeat
/// calls, and the OnceLock ensures we only do it once per process.
fn register_embedded_fonts() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        const FONTS: &[&[u8]] = &[
            include_bytes!("../assets/fonts/JetBrainsMono-Regular.ttf"),
            include_bytes!("../assets/fonts/Marlboro.ttf"),
        ];
        for bytes in FONTS {
            let count: u32 = 0;
            unsafe {
                AddFontMemResourceEx(bytes.as_ptr() as *const _, bytes.len() as u32, None, &count);
            }
        }
    });
}

/// Build a LOGFONTW + create an HFONT for `face` at the given negative
/// lfHeight (pixels). Caller owns the returned HFONT; in practice we stash
/// them in OnceLocks and never delete them — they live for the process.
unsafe fn create_font(face: &str, height: i32) -> HFONT {
    let mut logfont = LOGFONTW {
        lfHeight: height,
        lfWeight: FW_NORMAL.0 as i32,
        lfCharSet: DEFAULT_CHARSET,
        lfOutPrecision: OUT_TT_PRECIS,
        lfClipPrecision: CLIP_DEFAULT_PRECIS,
        lfQuality: CLEARTYPE_QUALITY,
        ..Default::default()
    };
    let face_u16: Vec<u16> = face.encode_utf16().collect();
    let max = logfont.lfFaceName.len() - 1;
    for (i, c) in face_u16.iter().take(max).enumerate() {
        logfont.lfFaceName[i] = *c;
    }
    CreateFontIndirectW(&logfont)
}

/// JetBrains Mono for body text — character names on preview titles and
/// on list rows. Matches the config panel's body font.
fn nicotine_body_font() -> HFONT {
    static SLOT: OnceLock<isize> = OnceLock::new();
    let raw = *SLOT.get_or_init(|| {
        register_embedded_fonts();
        unsafe { create_font("JetBrains Mono", px(-14)).0 as isize }
    });
    HFONT(raw as *mut _)
}

/// Marlboro for the list window's "NICOTINE" title strip — matches the
/// config panel's header.
fn nicotine_logo_font() -> HFONT {
    static SLOT: OnceLock<isize> = OnceLock::new();
    let raw = *SLOT.get_or_init(|| {
        register_embedded_fonts();
        unsafe { create_font("Marlboro", px(-20)).0 as isize }
    });
    HFONT(raw as *mut _)
}

/// Paint the preview's chrome: a title strip at the top with the
/// character name, plus a left/right/bottom border around the thumbnail
/// area. Border color is Nicotine red when this client is the system
/// foreground window, otherwise the same dark color as the title strip
/// (so it blends seamlessly).
unsafe fn paint_chrome(hwnd: HWND, character_name: &str, is_active: bool) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    let chrome_color = if is_active { NICOTINE_RED } else { CHROME_DARK };
    let chrome_brush = CreateSolidBrush(chrome_color);
    let title_h = px(TITLE_HEIGHT);
    let border_w = px(BORDER_WIDTH);

    // Top strip (full-width title bar).
    let title_strip = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: title_h,
    };
    FillRect(hdc, &title_strip, chrome_brush);

    // Left, right, and bottom borders around the thumbnail area.
    let left_border = RECT {
        left: 0,
        top: title_h,
        right: border_w,
        bottom: height,
    };
    let right_border = RECT {
        left: width - border_w,
        top: title_h,
        right: width,
        bottom: height,
    };
    let bottom_border = RECT {
        left: 0,
        top: height - border_w,
        right: width,
        bottom: height,
    };
    FillRect(hdc, &left_border, chrome_brush);
    FillRect(hdc, &right_border, chrome_brush);
    FillRect(hdc, &bottom_border, chrome_brush);

    let _ = DeleteObject(chrome_brush.into());

    // White centered character name in the title strip.
    let _ = SetBkMode(hdc, TRANSPARENT);
    let _ = SetTextColor(hdc, COLORREF(0x00FF_FFFF));
    let body_font = nicotine_body_font();
    let prev_font = SelectObject(hdc, body_font.into());
    let mut text: Vec<u16> = character_name.encode_utf16().collect();
    let mut text_rect = title_strip;
    let _ = DrawTextW(
        hdc,
        &mut text,
        &mut text_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, prev_font);

    let _ = EndPaint(hwnd, &ps);
}

/// Height of the list window given a current client count.
fn list_window_height(num_clients: usize) -> i32 {
    let rows = num_clients.max(1) as i32;
    px(TITLE_HEIGHT) + rows * px(LIST_ROW_HEIGHT) + px(LIST_PADDING)
}

/// Window procedure for the single list-view window. Paints the title
/// strip + one row per character, with the active character drawn in
/// Nicotine red prefixed with a 🚬. Left-click drags the window.
unsafe extern "system" fn list_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ListWindowState;
    let state = if state_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    } else {
        &mut *state_ptr
    };

    match msg {
        WM_PAINT => {
            paint_list(hwnd);
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (cx, cy) = unpack_xy(lparam);
            let mut pt = POINT { x: cx, y: cy };
            let _ = ClientToScreen(hwnd, &mut pt);
            let mut rect = RECT::default();
            let _ = GetWindowRect(hwnd, &mut rect);
            state.drag_active = true;
            state.dragged = false;
            state.drag_origin_screen = (pt.x, pt.y);
            state.drag_origin_window = (rect.left, rect.top);
            let _ = SetCapture(hwnd);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            // Positions locked: drag the list window is a no-op.
            if state.drag_active && !positions_locked() {
                let (cx, cy) = unpack_xy(lparam);
                let mut pt = POINT { x: cx, y: cy };
                let _ = ClientToScreen(hwnd, &mut pt);
                let dx = pt.x - state.drag_origin_screen.0;
                let dy = pt.y - state.drag_origin_screen.1;
                let threshold = px(DRAG_THRESHOLD_PX);
                if dx.abs() > threshold || dy.abs() > threshold {
                    state.dragged = true;
                }
                if state.dragged {
                    let new_x = state.drag_origin_window.0 + dx;
                    let new_y = state.drag_origin_window.1 + dy;
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOPMOST),
                        new_x,
                        new_y,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if state.drag_active {
                state.drag_active = false;
                let _ = ReleaseCapture();
                if state.dragged {
                    // Persist the new list-view position so it survives
                    // a display-mode toggle (which destroys/recreates
                    // this window) or a daemon restart.
                    let mut rect = RECT::default();
                    let _ = GetWindowRect(hwnd, &mut rect);
                    let mgr_ptr = MANAGER_PTR.load(Ordering::Acquire) as *const PreviewManager;
                    if !mgr_ptr.is_null() {
                        let mut positions = (*mgr_ptr).positions.lock().unwrap();
                        positions.list_position = Some((rect.left, rect.top));
                        positions.save();
                    }
                }
                state.dragged = false;
            }
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Paint the list window: red title strip at the top, then one row per
/// character on a cream background. Active row is rendered in Nicotine
/// red with a cigarette emoji prefix.
unsafe fn paint_list(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    let mut rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut rect);
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;

    let title_h = px(TITLE_HEIGHT);
    let row_h = px(LIST_ROW_HEIGHT);

    // Cream body
    let body_brush = CreateSolidBrush(NICOTINE_CREAM);
    let body_rect = RECT {
        left: 0,
        top: title_h,
        right: width,
        bottom: height,
    };
    FillRect(hdc, &body_rect, body_brush);
    let _ = DeleteObject(body_brush.into());

    // Red title strip
    let title_brush = CreateSolidBrush(NICOTINE_RED);
    let title_rect = RECT {
        left: 0,
        top: 0,
        right: width,
        bottom: title_h,
    };
    FillRect(hdc, &title_rect, title_brush);
    let _ = DeleteObject(title_brush.into());

    let _ = SetBkMode(hdc, TRANSPARENT);

    // Title text — Marlboro logo font, matching the config panel header.
    let _ = SetTextColor(hdc, COLORREF(0x00FF_FFFF));
    let logo_font = nicotine_logo_font();
    let prev_title_font = SelectObject(hdc, logo_font.into());
    let mut title_text: Vec<u16> = "Nicotine".encode_utf16().collect();
    let mut title_draw_rect = title_rect;
    let _ = DrawTextW(
        hdc,
        &mut title_text,
        &mut title_draw_rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, prev_title_font);

    // Pull the latest window list + active id via MANAGER_PTR. Safe
    // because the control thread (us) owns the manager memory.
    let mgr_ptr = MANAGER_PTR.load(Ordering::Acquire) as *const PreviewManager;
    if mgr_ptr.is_null() {
        let _ = EndPaint(hwnd, &ps);
        return;
    }
    let mgr = &*mgr_ptr;
    let active_id = mgr.active_id;
    // Use the stable character-order view so rows don't reorder as the
    // user cycles (get_windows() is z-order from EnumWindows).
    let windows = {
        let s = mgr.state.lock().unwrap();
        s.get_ordered_windows()
    };

    // Per-row text — JetBrains Mono, same as config panel body text.
    let body_font = nicotine_body_font();
    let prev_row_font = SelectObject(hdc, body_font.into());
    let left_pad = px(10);
    let right_pad = px(6);
    let mut y = title_h + px(2);
    for window in &windows {
        let is_active = window.id == active_id;
        let text = if is_active {
            format!("🚬 {}", window.title)
        } else {
            format!("     {}", window.title)
        };
        let color = if is_active {
            NICOTINE_RED
        } else {
            LIST_TEXT_BLACK
        };
        let _ = SetTextColor(hdc, color);
        let mut row_buf: Vec<u16> = text.encode_utf16().collect();
        let mut row_rect = RECT {
            left: left_pad,
            top: y,
            right: width - right_pad,
            bottom: y + row_h,
        };
        let _ = DrawTextW(hdc, &mut row_buf, &mut row_rect, DT_SINGLELINE | DT_VCENTER);
        y += row_h;
    }
    SelectObject(hdc, prev_row_font);

    let _ = EndPaint(hwnd, &ps);
}

/// Control-window procedure: the only message it cares about is WM_TIMER,
/// which fires the reconcile pass against CycleState.
unsafe extern "system" fn control_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_TIMER && wparam.0 == RECONCILE_TIMER_ID {
        let mgr_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PreviewManager;
        if !mgr_ptr.is_null() {
            (*mgr_ptr).reconcile();
        }
        return LRESULT(0);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn register_classes(module: HMODULE) -> Result<()> {
    unsafe {
        let cursor = LoadCursorW(None, IDC_ARROW).context("LoadCursorW failed")?;

        // Background brush for preview windows. Without this, the OS skips
        // WM_ERASEBKGND and any pixel that DWM doesn't fully cover (e.g. a
        // sub-pixel anti-aliased edge of the thumbnail) shows whatever was
        // last in the buffer — typically white. Erasing to chrome dark
        // first means those edges blend invisibly with our chrome.
        let preview_bg = CreateSolidBrush(CHROME_DARK);

        let preview_class: Vec<u16> = PREVIEW_CLASS.encode_utf16().collect();
        let preview_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: Default::default(),
            lpfnWndProc: Some(preview_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module.into(),
            hIcon: HICON::default(),
            hCursor: cursor,
            hbrBackground: preview_bg,
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(preview_class.as_ptr()),
            hIconSm: HICON::default(),
        };
        // RegisterClassExW returns 0 on failure but also fails harmlessly if
        // already registered (e.g. if the daemon is restarted in-process for
        // testing). Ignore the result.
        let _ = RegisterClassExW(&preview_wc);

        let control_class: Vec<u16> = CONTROL_CLASS.encode_utf16().collect();
        let control_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: Default::default(),
            lpfnWndProc: Some(control_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module.into(),
            hIcon: HICON::default(),
            hCursor: HCURSOR::default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(control_class.as_ptr()),
            hIconSm: HICON::default(),
        };
        let _ = RegisterClassExW(&control_wc);

        // List window class — opaque cream background erased via
        // hbrBackground so flicker-free when text rows change.
        let list_bg = CreateSolidBrush(NICOTINE_CREAM);
        let list_class: Vec<u16> = LIST_CLASS.encode_utf16().collect();
        let list_wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: Default::default(),
            lpfnWndProc: Some(list_wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: module.into(),
            hIcon: HICON::default(),
            hCursor: cursor,
            hbrBackground: list_bg,
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(list_class.as_ptr()),
            hIconSm: HICON::default(),
        };
        let _ = RegisterClassExW(&list_wc);
    }
    Ok(())
}

/// Spawn the preview-window manager thread. Runs forever (until the
/// daemon process exits). Each EVE client gets a borderless top-most
/// preview window with a 24px title strip and a DWM thumbnail mirror.
pub fn spawn(
    config: Config,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    live: Arc<Mutex<LiveSettings>>,
) -> Result<JoinHandle<()>> {
    let handle = std::thread::spawn(move || {
        if let Err(e) = run_manager(config, wm, state, live) {
            eprintln!("Preview window manager exited with error: {}", e);
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
    // Touch the thread ID so message routing works (and so any future
    // PostThreadMessage senders have a stable ID to target).
    let _tid = unsafe { GetCurrentThreadId() };

    let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
    register_classes(module)?;

    // Create the hidden message-only control window that owns the
    // reconcile timer.
    let control_class: Vec<u16> = CONTROL_CLASS.encode_utf16().collect();
    let control_title: Vec<u16> = "NicotineControl\0".encode_utf16().collect();
    let control_hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR(control_class.as_ptr()),
            PCWSTR(control_title.as_ptr()),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(HINSTANCE(module.0)),
            None,
        )
    }
    .context("CreateWindowExW failed for control window")?;

    let positions = Arc::new(Mutex::new(PreviewPositions::load()));

    // Allocate the manager on the heap so we can stash a pointer in the
    // control window's GWLP_USERDATA. The control wnd_proc reads this
    // back to dispatch reconcile().
    let initial_mode = live.lock().unwrap().display_mode;
    let manager = Box::new(PreviewManager {
        wm,
        state,
        config,
        positions,
        previews: HashMap::new(),
        next_default_offset: 0,
        live,
        current_mode: initial_mode,
        list: None,
        active_id: 0,
        list_last_names: Vec::new(),
    });
    let manager_ptr = Box::into_raw(manager);
    MANAGER_PTR.store(manager_ptr as usize, Ordering::Release);
    unsafe {
        SetWindowLongPtrW(control_hwnd, GWLP_USERDATA, manager_ptr as isize);
        let _ = SetTimer(
            Some(control_hwnd),
            RECONCILE_TIMER_ID,
            RECONCILE_INTERVAL_MS,
            None,
        );
    }

    // Subscribe to system-wide foreground changes so the active-client
    // outline updates instantly on focus change instead of waiting up to
    // 500ms for the next reconcile tick. WINEVENT_OUTOFCONTEXT delivers
    // the callback on this thread via the message queue, which is exactly
    // what we want — no cross-thread synchronization needed.
    let win_event_hook = unsafe {
        SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(foreground_event_proc),
            0, // any process
            0, // any thread
            WINEVENT_OUTOFCONTEXT,
        )
    };

    // Main message pump for the manager + all preview windows on this
    // thread.
    let mut msg = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !got.as_bool() {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // Cleanup on shutdown.
    unsafe {
        let _ = UnhookWinEvent(win_event_hook);
        MANAGER_PTR.store(0, Ordering::Release);
        let _ = KillTimer(Some(control_hwnd), RECONCILE_TIMER_ID);
        if !manager_ptr.is_null() {
            // Drop the manager last — that drops the OwnedPreview map, which
            // unregisters DWM thumbnails and destroys the preview windows.
            drop(Box::from_raw(manager_ptr));
        }
        let _ = DestroyWindow(control_hwnd);
    }
    Ok(())
}

// Suppress unused warnings for symbols only meaningful in some build modes.
#[allow(dead_code)]
fn _unused() {
    let _ = HMENU::default();
}
