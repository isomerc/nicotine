use crate::config::Config;
use crate::eve_match::pid_is_eve_client;
use crate::window_manager::{EveWindow, WindowManager};
use crate::windows_helpers::should_attach_thread_input;
use anyhow::{Context, Result};
use std::ffi::c_void;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, TRUE, WPARAM};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::{
    BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsIconic, IsWindowVisible, SendMessageW, SetForegroundWindow,
    SetWindowPos, ShowWindow, HWND_TOP, SC_MINIMIZE, SC_RESTORE, SWP_NOZORDER, SW_MINIMIZE,
    SW_RESTORE, WM_SYSCOMMAND,
};

pub struct WindowsManager;

impl WindowsManager {
    pub fn new() -> Result<Self> {
        Ok(Self)
    }
}

pub(crate) fn hwnd_to_id(hwnd: HWND) -> u32 {
    hwnd.0 as usize as u32
}

pub(crate) fn id_to_hwnd(id: u32) -> HWND {
    HWND(id as usize as *mut c_void)
}

fn read_window_title(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    // +1 for the null terminator that GetWindowTextW writes
    let mut buf: Vec<u16> = vec![0; len as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..copied as usize])
}

unsafe extern "system" fn enum_collect_eve(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let windows = &mut *(lparam.0 as *mut Vec<EveWindow>);

    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }

    let title = read_window_title(hwnd);
    if !title.starts_with("EVE - ") {
        return TRUE;
    }
    // Process gate: reject anything titled "EVE - …" whose owning
    // process isn't the actual game (`exefile.exe`). Closes the bug
    // where preview windows got created for non-EVE apps (browser
    // tabs etc.) that happened to start with the EVE title prefix.
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == 0 || !pid_is_eve_client(pid) {
        return TRUE;
    }
    windows.push(EveWindow {
        id: hwnd_to_id(hwnd),
        title: title.trim_start_matches("EVE - ").to_string(),
    });

    TRUE
}

/// SetForegroundWindow is restricted on modern Windows — for reliable
/// focus stealing from another process, the standard pattern is to attach
/// our input queue to the target's. But that's slow. The fast path:
/// SetForegroundWindow works directly when our process "received the last
/// input event" — and a `RegisterHotKey` WM_HOTKEY counts. So we try the
/// cheap call first and only fall back to the AttachThreadInput dance if
/// Windows rejects it (typical when activation is triggered from the
/// passive low-level mouse hook, not a hotkey).
fn force_activate(target: HWND) {
    unsafe {
        let foreground = GetForegroundWindow();
        if foreground == target {
            // Already focused — skip everything. Common case for repeated
            // hotkey presses against the active window.
            return;
        }

        // SW_RESTORE triggers the show animation, so only fire it when we
        // genuinely need to un-minimize.
        if IsIconic(target).as_bool() {
            let _ = ShowWindow(target, SW_RESTORE);
        }

        // Fast path: try direct SetForegroundWindow first.
        if SetForegroundWindow(target).as_bool() {
            return;
        }

        // Fallback path: Windows refused the foreground change. Briefly
        // attach our thread's input queue to the target and current
        // foreground's queues so we look like the same input session.
        let target_thread = GetWindowThreadProcessId(target, None);
        let current_thread = GetCurrentThreadId();
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };

        // Gate the AttachThreadInput calls on the pure attach-eligibility
        // rule (nonzero, not self, not already attached). See
        // `windows_helpers::should_attach_thread_input` for the rule and
        // its tests.
        let attached_target = should_attach_thread_input(target_thread, current_thread, &[])
            && AttachThreadInput(current_thread, target_thread, true).as_bool();
        let attached_foreground =
            should_attach_thread_input(foreground_thread, current_thread, &[target_thread])
                && AttachThreadInput(current_thread, foreground_thread, true).as_bool();

        let _ = SetForegroundWindow(target);
        let _ = BringWindowToTop(target);
        let _ = SetFocus(Some(target));

        if attached_target {
            let _ = AttachThreadInput(current_thread, target_thread, false);
        }
        if attached_foreground {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
    }
}

impl WindowManager for WindowsManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        // Use a Mutex<Vec<...>> to satisfy unwind safety even though the
        // callback is single-threaded — EnumWindows is synchronous.
        let mut windows: Vec<EveWindow> = Vec::new();
        unsafe {
            EnumWindows(
                Some(enum_collect_eve),
                LPARAM(&mut windows as *mut _ as isize),
            )
            .context("EnumWindows failed")?;
        }
        Ok(windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        force_activate(id_to_hwnd(window_id));
        Ok(())
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width as i32;
        let height = (config.display_height - config.panel_height) as i32;

        for window in windows {
            unsafe {
                SetWindowPos(
                    id_to_hwnd(window.id),
                    Some(HWND_TOP),
                    x,
                    y,
                    width,
                    height,
                    SWP_NOZORDER,
                )
                .ok();
            }
        }
        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        let hwnd = unsafe { GetForegroundWindow() };
        Ok(hwnd_to_id(hwnd))
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        let hwnd = id_to_hwnd(window_id);
        unsafe {
            // SC_MINIMIZE via WM_SYSCOMMAND is friendlier to applications
            // than ShowWindow(SW_MINIMIZE) — it goes through the normal
            // window state machine.
            SendMessageW(
                hwnd,
                WM_SYSCOMMAND,
                Some(WPARAM(SC_MINIMIZE as usize)),
                Some(LPARAM(0)),
            );
        }
        let _ = unsafe { ShowWindow(hwnd, SW_MINIMIZE) };
        Ok(())
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        let hwnd = id_to_hwnd(window_id);
        unsafe {
            SendMessageW(
                hwnd,
                WM_SYSCOMMAND,
                Some(WPARAM(SC_RESTORE as usize)),
                Some(LPARAM(0)),
            );
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        Ok(())
    }
}

// SAFETY: WindowsManager has no state and Win32 window APIs are thread-safe
// for the operations we perform.
unsafe impl Send for WindowsManager {}
unsafe impl Sync for WindowsManager {}
