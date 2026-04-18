use crate::config::Config;
use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT,
    VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_MENU, VK_RCONTROL, VK_RMENU, VK_RSHIFT,
    VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, HHOOK, MSG, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_HOTKEY, WM_USER, WM_XBUTTONDOWN,
};

const HOTKEY_FORWARD_ID: i32 = 1001;
const HOTKEY_BACKWARD_ID: i32 = 1002;
/// Per-character hotkey IDs are assigned starting here, one per bound
/// character, in the order the config lists them. Separated from the
/// cycle IDs so the message dispatch can tell them apart by ID range.
const HOTKEY_CHARACTER_BASE: i32 = 2000;

/// Lookup from per-character hotkey ID → the character name to
/// activate on WM_HOTKEY. Rebuilt from scratch each time hotkeys are
/// registered so stale entries never leak between config changes.
static CHARACTER_HOTKEY_LOOKUP: OnceLock<Mutex<HashMap<i32, String>>> = OnceLock::new();

fn character_lookup() -> &'static Mutex<HashMap<i32, String>> {
    CHARACTER_HOTKEY_LOOKUP.get_or_init(|| Mutex::new(HashMap::new()))
}

const WM_USER_FORWARD: u32 = WM_USER + 1;
const WM_USER_BACKWARD: u32 = WM_USER + 2;
const WM_USER_PAUSE: u32 = WM_USER + 3;
const WM_USER_RESUME: u32 = WM_USER + 4;

/// Thread ID of the running input listener, exposed so the config
/// panel can PostThreadMessage pause/resume signals when the user is
/// binding keys. Zero means "no listener running."
pub static LISTENER_THREAD_ID: AtomicU32 = AtomicU32::new(0);

/// When true, the listener ignores incoming WM_HOTKEY and posted
/// cycle messages. Used to suppress daemon action while the user is
/// rebinding keys in the config panel.
static LISTENER_PAUSED: AtomicBool = AtomicBool::new(false);

/// Whether x-button presses should trigger cycle actions. Mirrors
/// `config.enable_mouse_buttons` but is checked per-press, so the
/// setting can hot-toggle without reinstalling the hook. Most users
/// actually remap their mouse side buttons to keyboard keys via driver
/// software (Logi Options+, etc.) and use Nicotine's keyboard
/// hotkeys instead — the native XBUTTON path remains as a fallback
/// for mice that emit raw x-button events.
static MOUSE_CYCLE_ENABLED: AtomicBool = AtomicBool::new(false);

/// Ask the input listener to stop acting on hotkeys. This unregisters
/// its global hotkeys so the keys become available to the focused
/// window (the config panel) for capture. No-op if the listener isn't
/// running yet.
pub fn pause_hotkeys() {
    let tid = LISTENER_THREAD_ID.load(Ordering::Acquire);
    if tid == 0 {
        return;
    }
    unsafe {
        let _ = PostThreadMessageW(tid, WM_USER_PAUSE, WPARAM(0), LPARAM(0));
    }
}

/// Ask the input listener to resume. It will re-read the latest
/// config.toml and re-register hotkeys with whatever the user just
/// bound.
pub fn resume_hotkeys() {
    let tid = LISTENER_THREAD_ID.load(Ordering::Acquire);
    if tid == 0 {
        return;
    }
    unsafe {
        let _ = PostThreadMessageW(tid, WM_USER_RESUME, WPARAM(0), LPARAM(0));
    }
}

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

        // Pass-through when cycling is disabled (user set
        // enable_mouse_buttons = false in config).
        if !MOUSE_CYCLE_ENABLED.load(Ordering::Acquire) {
            return CallNextHookEx(None, code, wparam, lparam);
        }

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
    LISTENER_THREAD_ID.store(listener_thread_id, Ordering::Release);

    // Install the low-level mouse hook unconditionally — we need it
    // running even when mouse cycling is disabled so that the config
    // panel can still capture x-button presses for binding. The hook
    // gates actual cycle actions on MOUSE_CYCLE_ENABLED (below), so a
    // disabled user config won't trigger cycling.
    let _ = HOOK_CTX.set(HookContext {
        forward_button: config.forward_button,
        backward_button: config.backward_button,
        listener_thread_id,
    });
    MOUSE_CYCLE_ENABLED.store(config.enable_mouse_buttons, Ordering::Release);

    let module = unsafe { GetModuleHandleW(None) }.context("GetModuleHandleW failed")?;
    let _hook: HHOOK = unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook_proc),
            Some(HINSTANCE(module.0)),
            0,
        )
    }
    .context("SetWindowsHookExW failed — check that the daemon process has UI access")?;
    println!("Mouse side-button hook installed");

    // Register keyboard hotkeys if enabled.
    register_hotkeys(&config);

    let mut msg = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if !got.as_bool() {
            // WM_QUIT or error — both terminate the loop.
            break;
        }

        // Config-panel binding mode: temporarily stop consuming hotkeys
        // so egui sees the user's next key press.
        if msg.message == WM_USER_PAUSE {
            LISTENER_PAUSED.store(true, Ordering::Release);
            unregister_hotkeys();
            continue;
        }
        if msg.message == WM_USER_RESUME {
            LISTENER_PAUSED.store(false, Ordering::Release);
            // Unregister first so this path is safe to call as a
            // generic "rebind" even when we weren't paused. Re-read
            // config.toml so the hotkeys the user just bound take
            // effect immediately.
            unregister_hotkeys();
            if let Ok(fresh) = Config::load() {
                register_hotkeys(&fresh);
            }
            continue;
        }
        // While paused, drop any cycle-triggering events — the mouse
        // hook can still fire XBUTTON posts, but we don't want the
        // daemon to act on them mid-capture.
        if LISTENER_PAUSED.load(Ordering::Acquire) {
            continue;
        }

        // Read config.minimize_inactive fresh each action so user
        // toggles apply without restart. One small file read, cheap.
        let minimize_inactive_lookup =
            || Config::load().map(|c| c.minimize_inactive).unwrap_or(false);

        // Cycle action?
        let cycle: Option<CycleDirection> = match msg.message {
            WM_USER_FORWARD => Some(CycleDirection::Forward),
            WM_USER_BACKWARD => Some(CycleDirection::Backward),
            WM_HOTKEY => match msg.wParam.0 as i32 {
                HOTKEY_FORWARD_ID => Some(CycleDirection::Forward),
                HOTKEY_BACKWARD_ID => Some(CycleDirection::Backward),
                _ => None,
            },
            _ => None,
        };
        if let Some(direction) = cycle {
            let minimize_inactive = minimize_inactive_lookup();
            if let Err(e) = perform_cycle(&wm, &state, direction, minimize_inactive) {
                eprintln!("Cycle action failed: {}", e);
            }
            continue;
        }

        // Per-character jump hotkey?
        if msg.message == WM_HOTKEY {
            let id = msg.wParam.0 as i32;
            if id >= HOTKEY_CHARACTER_BASE {
                let name = character_lookup().lock().unwrap().get(&id).cloned();
                if let Some(name) = name {
                    let minimize_inactive = minimize_inactive_lookup();
                    let mut state_guard = state.lock().unwrap();
                    if let Ok(active) = wm.get_active_window() {
                        state_guard.sync_with_active(active);
                    }
                    if let Err(e) = state_guard.switch_to_character(&name, &*wm, minimize_inactive)
                    {
                        eprintln!("Character switch failed: {}", e);
                    }
                }
            }
        }
    }
    LISTENER_THREAD_ID.store(0, Ordering::Release);
    Ok(())
}

/// Register the configured forward / backward cycle hotkeys AND every
/// per-character hotkey, all on the current thread. Silently ignores
/// failures (another app may own the key) — the listener still runs,
/// it just won't fire for the contested key.
unsafe fn do_register_hotkeys(config: &Config) {
    // Cycle hotkeys — gated by enable_keyboard_buttons so users can
    // disable cycle hotkeys while still using per-character ones.
    if config.enable_keyboard_buttons {
        let _ = RegisterHotKey(
            None,
            HOTKEY_FORWARD_ID,
            HOT_KEY_MODIFIERS(0),
            config.forward_key as u32,
        );
        let modifier = config.modifier_key.map(vk_to_modifier);
        let backward_mod = if config.forward_key == config.backward_key {
            modifier.unwrap_or(HOT_KEY_MODIFIERS(0))
        } else {
            HOT_KEY_MODIFIERS(0)
        };
        if config.forward_key != config.backward_key || backward_mod.0 != 0 {
            let _ = RegisterHotKey(
                None,
                HOTKEY_BACKWARD_ID,
                backward_mod,
                config.backward_key as u32,
            );
        }
    }

    // Per-character hotkeys — iterate the characters list in order so
    // hotkey IDs are stable for the same config, and populate the
    // ID → name lookup.
    let mut lookup = character_lookup().lock().unwrap();
    lookup.clear();
    let mut next_id = HOTKEY_CHARACTER_BASE;
    for name in &config.characters {
        let Some(hk) = config.character_hotkeys.get(name) else {
            continue;
        };
        // vk == 0 is a placeholder entry — the user picked a modifier
        // but hasn't captured a key yet. Skip registration; the entry
        // becomes active only once a real VK is bound.
        if hk.vk == 0 {
            continue;
        }
        let modifier = hk
            .modifier
            .map(vk_to_modifier)
            .unwrap_or(HOT_KEY_MODIFIERS(0));
        if RegisterHotKey(None, next_id, modifier, hk.vk as u32).is_ok() {
            lookup.insert(next_id, name.clone());
        } else {
            eprintln!(
                "Failed to register per-character hotkey for '{}' (another app may own it)",
                name
            );
        }
        next_id += 1;
    }
}

fn register_hotkeys(config: &Config) {
    unsafe { do_register_hotkeys(config) }
}

fn unregister_hotkeys() {
    unsafe {
        let _ = UnregisterHotKey(None, HOTKEY_FORWARD_ID);
        let _ = UnregisterHotKey(None, HOTKEY_BACKWARD_ID);
        let mut lookup = character_lookup().lock().unwrap();
        for id in lookup.keys() {
            let _ = UnregisterHotKey(None, *id);
        }
        lookup.clear();
    }
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
