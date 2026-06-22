use crate::config::{Config, LiveSettings, TriggerKind};
use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use crate::windows_helpers::{
    classify_xbutton, modifier_kind, plan_character_hotkeys, plan_cycle_hotkeys, CycleDirection,
    ModifierKind,
};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use windows::Win32::Foundation::{HINSTANCE, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW, HHOOK, MSG, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_HOTKEY, WM_USER, WM_XBUTTONDOWN,
};

const HOTKEY_FORWARD_ID: i32 = 1001;
const HOTKEY_BACKWARD_ID: i32 = 1002;
/// Global hotkey that flips `LiveSettings.show_previews` (show/hide all
/// preview windows). Registered only when the user has bound a key.
const HOTKEY_TOGGLE_PREVIEWS_ID: i32 = 1003;
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

/// Hot-reload setter for MOUSE_CYCLE_ENABLED. Called from the daemon
/// config-watch thread so toggling `enable_mouse_buttons` in the
/// config panel takes effect within ~500ms — no daemon restart.
pub fn set_mouse_cycle_enabled(enabled: bool) {
    MOUSE_CYCLE_ENABLED.store(enabled, Ordering::Release);
}

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
        debug_input(format_args!(
            "mouse_hook_proc: WM_XBUTTONDOWN raw={:#010x} xbutton={} flags={:#x}",
            info.mouseData, xbutton, info.flags
        ));

        // Pass-through when cycling is disabled (user set
        // enable_mouse_buttons = false in config).
        if !MOUSE_CYCLE_ENABLED.load(Ordering::Acquire) {
            debug_input(format_args!(
                "mouse_hook_proc: MOUSE_CYCLE_ENABLED=false, pass-through"
            ));
            return CallNextHookEx(None, code, wparam, lparam);
        }

        if let Some(ctx) = HOOK_CTX.get() {
            let post =
                classify_xbutton(xbutton, ctx.forward_button, ctx.backward_button).map(|dir| {
                    match dir {
                        CycleDirection::Forward => WM_USER_FORWARD,
                        CycleDirection::Backward => WM_USER_BACKWARD,
                    }
                });
            debug_input(format_args!(
                "mouse_hook_proc: classify xbutton={} fwd={} back={} -> post={:?}",
                xbutton, ctx.forward_button, ctx.backward_button, post
            ));

            if let Some(msg) = post {
                // PostThreadMessageW returns Err if the thread queue is
                // unavailable (e.g. listener has exited). Best-effort.
                let result = PostThreadMessageW(ctx.listener_thread_id, msg, WPARAM(0), LPARAM(0));
                debug_input(format_args!(
                    "mouse_hook_proc: PostThreadMessageW tid={} msg={:#x} result={:?}",
                    ctx.listener_thread_id, msg, result
                ));
            }
        } else {
            debug_input(format_args!("mouse_hook_proc: HOOK_CTX unset"));
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

/// Diagnostic logging gated by `NICOTINE_DEBUG_INPUT`. Used by
/// integration tests to instrument the mouse-hook -> listener ->
/// cycle pipeline so a failing test can pinpoint which step dropped
/// the event. Production users never see these lines unless they
/// explicitly set the env var. eprintln from a low-level hook is
/// safe (the hook proc runs on the installing thread, which is also
/// the message-pump thread, but nothing in stdio takes locks that
/// would re-enter the hook).
fn debug_input(args: std::fmt::Arguments) {
    if std::env::var_os("NICOTINE_DEBUG_INPUT").is_some() {
        eprintln!("{}", args);
    }
}

/// Translate a planner-output ModifierKind into the Win32
/// HOT_KEY_MODIFIERS bitmask RegisterHotKey expects. The planner is
/// kept platform-independent (see `windows_helpers`) so this thin
/// adapter is the only place that knows about MOD_SHIFT/etc.
fn modifier_to_winapi(kinds: &[ModifierKind]) -> HOT_KEY_MODIFIERS {
    let mut bits = 0u32;
    for k in kinds {
        bits |= match k {
            ModifierKind::Shift => MOD_SHIFT.0,
            ModifierKind::Ctrl => MOD_CONTROL.0,
            ModifierKind::Alt => MOD_ALT.0,
            ModifierKind::Win => MOD_WIN.0,
        };
    }
    HOT_KEY_MODIFIERS(bits)
}

/// Spawn the Windows input listener thread. The thread installs a low-level
/// mouse hook for the configured side buttons and (optionally) registers
/// keyboard hotkeys, then runs a message pump that triggers cycle actions.
pub fn spawn(
    config: Config,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    live: Arc<Mutex<LiveSettings>>,
) -> Result<JoinHandle<()>> {
    let handle = std::thread::spawn(move || {
        if let Err(e) = run_listener(config, wm, state, live) {
            eprintln!("Windows input listener exited with error: {}", e);
        }
    });
    Ok(handle)
}

fn run_listener(
    config: Config,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    live: Arc<Mutex<LiveSettings>>,
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
            debug_input(format_args!("listener: cycle direction={:?}", direction));
            let minimize_inactive = minimize_inactive_lookup();
            match perform_cycle(&wm, &state, direction, minimize_inactive) {
                Ok(()) => debug_input(format_args!("listener: perform_cycle OK")),
                Err(e) => eprintln!("Cycle action failed: {}", e),
            }
            continue;
        }

        // Preview-visibility toggle hotkey? Flip the shared LiveSettings
        // flag the preview manager watches; it shows/hides on the next
        // reconcile tick, same as the panel checkbox.
        if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_TOGGLE_PREVIEWS_ID {
            let mut live_guard = live.lock().unwrap();
            live_guard.show_previews = !live_guard.show_previews;
            println!(
                "Preview windows toggled {} via hotkey",
                if live_guard.show_previews {
                    "on"
                } else {
                    "off"
                }
            );
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
///
/// All ID assignment and modifier-conditional logic lives in
/// `windows_helpers::plan_cycle_hotkeys` / `plan_character_hotkeys`
/// (with unit tests). This function only consumes the plans and makes
/// the Win32 calls.
unsafe fn do_register_hotkeys(config: &Config) {
    for plan in plan_cycle_hotkeys(
        config.enable_keyboard_buttons,
        &config.forward_key,
        &config.backward_key,
        HOTKEY_FORWARD_ID,
        HOTKEY_BACKWARD_ID,
    ) {
        let _ = RegisterHotKey(
            None,
            plan.id,
            modifier_to_winapi(&plan.modifiers),
            plan.vk as u32,
        );
    }

    // Preview-visibility toggle. Only key-triggered toggles are
    // RegisterHotKey-able (a mouse/wheel toggle would run through the mouse
    // hook). Independent of enable_keyboard_buttons.
    let toggle = &config.toggle_previews_key;
    if toggle.kind == TriggerKind::Key && toggle.code != 0 {
        let mods: Vec<ModifierKind> =
            toggle.mods.iter().filter_map(|&m| modifier_kind(m)).collect();
        let _ = RegisterHotKey(
            None,
            HOTKEY_TOGGLE_PREVIEWS_ID,
            modifier_to_winapi(&mods),
            toggle.code as u32,
        );
    }

    let mut lookup = character_lookup().lock().unwrap();
    lookup.clear();
    for plan in plan_character_hotkeys(
        &config.characters,
        &config.character_hotkeys,
        HOTKEY_CHARACTER_BASE,
    ) {
        let modifier = modifier_to_winapi(&plan.modifiers);
        if RegisterHotKey(None, plan.id, modifier, plan.vk as u32).is_ok() {
            lookup.insert(plan.id, plan.character_name);
        } else {
            eprintln!(
                "Failed to register per-character hotkey for '{}' (another app may own it)",
                plan.character_name
            );
        }
    }
}

fn register_hotkeys(config: &Config) {
    unsafe { do_register_hotkeys(config) }
}

fn unregister_hotkeys() {
    unsafe {
        let _ = UnregisterHotKey(None, HOTKEY_FORWARD_ID);
        let _ = UnregisterHotKey(None, HOTKEY_BACKWARD_ID);
        let _ = UnregisterHotKey(None, HOTKEY_TOGGLE_PREVIEWS_ID);
        let mut lookup = character_lookup().lock().unwrap();
        for id in lookup.keys() {
            let _ = UnregisterHotKey(None, *id);
        }
        lookup.clear();
    }
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
