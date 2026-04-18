use crate::config::Config;
use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use anyhow::{Context, Result};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, VK_CONTROL, VK_LCONTROL,
    VK_LMENU, VK_LSHIFT, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, HHOOK, MSG, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_HOTKEY, WM_USER, WM_XBUTTONDOWN,
};

const HOTKEY_FORWARD_ID: i32 = 1001;
const HOTKEY_BACKWARD_ID: i32 = 1002;

const WM_USER_FORWARD: u32 = WM_USER + 1;
const WM_USER_BACKWARD: u32 = WM_USER + 2;

/// Static context the low-level mouse hook reads to decide which posted
/// message (if any) to send back to the listener thread on each x-button
/// click. The hook callback is `extern "system" fn` — it can't capture, so
/// state has to live in a global.
struct HookContext {
    forward_button: u16,
    backward_button: u16,
    listener_thread_id: u32,
}

static HOOK_CTX: OnceLock<HookContext> = OnceLock::new();

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && wparam.0 as u32 == WM_XBUTTONDOWN {
        let info = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        // High word of mouseData identifies the X button: 1 = XBUTTON1 (back),
        // 2 = XBUTTON2 (forward).
        let xbutton = ((info.mouseData >> 16) & 0xFFFF) as u16;

        if let Some(ctx) = HOOK_CTX.get() {
            let post = if xbutton == ctx.forward_button {
                Some(WM_USER_FORWARD)
            } else if xbutton == ctx.backward_button {
                Some(WM_USER_BACKWARD)
            } else {
                None
            };

            if let Some(msg) = post {
                // PostThreadMessageW returns false if the thread queue is
                // unavailable (e.g. listener has exited). Best-effort.
                let _ = PostThreadMessageW(ctx.listener_thread_id, msg, WPARAM(0), LPARAM(0));
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

fn vk_to_modifier(vk: u16) -> HOT_KEY_MODIFIERS {
    match vk {
        v if v == VK_SHIFT.0 || v == VK_LSHIFT.0 || v == VK_RSHIFT.0 => MOD_SHIFT,
        v if v == VK_CONTROL.0 || v == VK_LCONTROL.0 || v == VK_RCONTROL.0 => MOD_CONTROL,
        v if v == VK_MENU.0 || v == VK_LMENU.0 || v == VK_RMENU.0 => MOD_ALT,
        _ => HOT_KEY_MODIFIERS(0),
    }
}

/// Spawn the Windows input listener thread. The thread installs a low-level
/// mouse hook for the configured side buttons and (optionally) registers
/// keyboard hotkeys, then runs a message pump that triggers cycle actions.
pub fn spawn(
    config: Config,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
) -> Result<JoinHandle<()>> {
    let handle = std::thread::spawn(move || {
        if let Err(e) = run_listener(config, wm, state) {
            eprintln!("Windows input listener exited with error: {}", e);
        }
    });
    Ok(handle)
}

fn run_listener(
    config: Config,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
) -> Result<()> {
    let listener_thread_id = unsafe { GetCurrentThreadId() };

    // Install mouse hook if enabled. The hook posts WM_USER_* to this thread.
    let _hook: Option<HHOOK> = if config.enable_mouse_buttons {
        let _ = HOOK_CTX.set(HookContext {
            forward_button: config.forward_button,
            backward_button: config.backward_button,
            listener_thread_id,
        });
        let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
        let hook = unsafe {
            SetWindowsHookExW(
                WH_MOUSE_LL,
                Some(mouse_hook_proc),
                Some(HINSTANCE(module.0)),
                0,
            )
        }
        .context("SetWindowsHookExW failed — check that the daemon process has UI access")?;
        println!("Mouse side-button hook installed");
        Some(hook)
    } else {
        None
    };

    // Register keyboard hotkeys if enabled. RegisterHotKey with hWnd=NULL
    // sends WM_HOTKEY to this thread's queue.
    if config.enable_keyboard_buttons {
        let modifier = config.modifier_key.map(vk_to_modifier);

        // Forward: bare key (no modifier)
        unsafe {
            RegisterHotKey(
                None,
                HOTKEY_FORWARD_ID,
                HOT_KEY_MODIFIERS(0),
                config.forward_key as u32,
            )
        }
        .context("RegisterHotKey for forward failed — another app may own this hotkey")?;

        // Backward: same or different key, with modifier if configured
        let backward_mod = if config.forward_key == config.backward_key {
            modifier.unwrap_or(HOT_KEY_MODIFIERS(0))
        } else {
            HOT_KEY_MODIFIERS(0)
        };

        let need_separate_backward =
            config.forward_key != config.backward_key || backward_mod.0 != 0;

        if need_separate_backward {
            unsafe {
                RegisterHotKey(
                    None,
                    HOTKEY_BACKWARD_ID,
                    backward_mod,
                    config.backward_key as u32,
                )
            }
            .context("RegisterHotKey for backward failed")?;
        } else {
            eprintln!(
                "Warning: forward_key == backward_key with no modifier — \
                 backward hotkey not registered"
            );
        }
        println!("Keyboard hotkeys registered");
    }

    let minimize_inactive = config.minimize_inactive;
    let mut msg = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !got.as_bool() {
            // WM_QUIT or error — both terminate the loop.
            break;
        }

        let action: Option<CycleDirection> = match msg.message {
            WM_USER_FORWARD => Some(CycleDirection::Forward),
            WM_USER_BACKWARD => Some(CycleDirection::Backward),
            WM_HOTKEY => match msg.wParam.0 as i32 {
                HOTKEY_FORWARD_ID => Some(CycleDirection::Forward),
                HOTKEY_BACKWARD_ID => Some(CycleDirection::Backward),
                _ => None,
            },
            _ => None,
        };

        if let Some(direction) = action {
            // Run the actual window switch in-place. We're not in the hook
            // callback here — taking 10ms is fine.
            if let Err(e) = perform_cycle(&wm, &state, direction, minimize_inactive) {
                eprintln!("Cycle action failed: {}", e);
            }
        }
    }
    Ok(())
}

#[derive(Copy, Clone)]
enum CycleDirection {
    Forward,
    Backward,
}

fn perform_cycle(
    wm: &Arc<dyn WindowManager>,
    state: &Arc<Mutex<CycleState>>,
    direction: CycleDirection,
    minimize_inactive: bool,
) -> Result<()> {
    let mut state = state.lock().unwrap();
    if let Ok(active) = wm.get_active_window() {
        state.sync_with_active(active);
    }
    match direction {
        CycleDirection::Forward => state.cycle_forward(&**wm, minimize_inactive)?,
        CycleDirection::Backward => state.cycle_backward(&**wm, minimize_inactive)?,
    }
    Ok(())
}
