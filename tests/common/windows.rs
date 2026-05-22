//! Windows-specific fake-EVE harness. Copies the `fake-eve-stub.exe`
//! test helper to a per-test temp dir as `exefile.exe` (so the
//! resulting process's image basename — what
//! `QueryFullProcessImageNameW` returns and what
//! `pid_is_eve_client` checks — matches the real EVE client's), then
//! spawns N of them with `--title-suffix <name>` args. Each stub
//! creates a top-level window titled `EVE - <name>` and pumps Win32
//! messages until killed.
//!
//! The "lookalike" path spawns the stub from its build path
//! (`fake-eve-stub.exe`), without renaming, so the process image
//! basename is *not* `exefile.exe` and the EVE-titled window it
//! creates should be rejected by Nicotine's process-name filter.

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, TRUE};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, EnumWindows, GetForegroundWindow, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible,
    SetForegroundWindow, ASFW_ANY,
};

/// One fake EVE client: a spawned `exefile.exe` child + its
/// top-level HWND (stored as `u32` to match the Linux harness's
/// XID-as-u32 representation; Nicotine itself does the same cast).
#[allow(dead_code)]
pub struct FakeEveClient {
    pub name: String,
    pub pid: u32,
    pub window: u32,
}

/// Owns the fixture: spawned children, the per-test temp directory
/// (holding the copied `exefile.exe`), and the HWNDs we resolved by
/// matching window title + owning PID.
pub struct FakeEveHarness {
    pub clients: Vec<FakeEveClient>,
    children: Vec<Child>,
    base_dir: PathBuf,
}

impl FakeEveHarness {
    /// Build a harness with one fake client per name. Spawns the
    /// stub helper under the `exefile.exe` filename so the
    /// process-name filter accepts each child as a real EVE client.
    pub fn new(names: &[&str]) -> anyhow::Result<Self> {
        let base_dir = std::env::temp_dir().join(format!(
            "nicotine-fake-eve-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&base_dir)?;

        // Copy the stub binary into the temp dir under the EVE
        // executable's canonical name. The kernel will record the
        // process's image path as this copy, so
        // `QueryFullProcessImageNameW` returns a path ending in
        // `exefile.exe` and `pid_is_eve_client` accepts it.
        let exefile_path = base_dir.join("exefile.exe");
        std::fs::copy(super::fake_eve_stub_binary(), &exefile_path)?;

        let mut children = Vec::with_capacity(names.len());
        let mut pids = Vec::with_capacity(names.len());
        for name in names {
            let child = Command::new(&exefile_path)
                .arg("--title-suffix")
                .arg(name)
                .spawn()?;
            pids.push(child.id());
            children.push(child);
        }

        // Wait for each stub's window to appear. The stub registers
        // its window class + creates the window synchronously after
        // process start, but RegisterClassW + CreateWindowExW take
        // a few ms on a cold runner. Poll until every PID has a
        // visible EVE-titled window or we hit the deadline.
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut clients: Vec<FakeEveClient> = Vec::new();
        while clients.len() < names.len() && Instant::now() < deadline {
            clients.clear();
            for (i, pid) in pids.iter().enumerate() {
                if let Some(hwnd) = find_window_by_pid(*pid) {
                    clients.push(FakeEveClient {
                        name: names[i].to_string(),
                        pid: *pid,
                        window: hwnd.0 as usize as u32,
                    });
                }
            }
            if clients.len() < names.len() {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        if clients.len() < names.len() {
            anyhow::bail!(
                "only {} of {} stub windows appeared within 3s",
                clients.len(),
                names.len()
            );
        }

        Ok(Self {
            clients,
            children,
            base_dir,
        })
    }

    /// Spawn an extra "lookalike" client: a stub whose process image
    /// is *not* `exefile.exe`, so the title prefix matches but the
    /// process-name filter rejects the window.
    pub fn add_lookalike(&mut self, title_suffix: &str) -> anyhow::Result<u32> {
        let child = Command::new(super::fake_eve_stub_binary())
            .arg("--title-suffix")
            .arg(title_suffix)
            .spawn()?;
        let pid = child.id();
        self.children.push(child);

        let deadline = Instant::now() + Duration::from_secs(2);
        let mut hwnd_opt: Option<HWND> = None;
        while Instant::now() < deadline {
            if let Some(h) = find_window_by_pid(pid) {
                hwnd_opt = Some(h);
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let hwnd =
            hwnd_opt.ok_or_else(|| anyhow::anyhow!("lookalike stub window never appeared"))?;
        let window = hwnd.0 as usize as u32;

        self.clients.push(FakeEveClient {
            name: format!("LOOKALIKE:{}", title_suffix),
            pid,
            window,
        });
        Ok(window)
    }
}

impl Drop for FakeEveHarness {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = std::fs::remove_dir_all(&self.base_dir);
    }
}

/// Activate one of the harness's fake EVE windows directly, bypassing
/// Nicotine. On modern Windows `SetForegroundWindow` is denied unless
/// the caller has input-chain rights (recently received user input,
/// is the foreground process, etc.). Our test process running under
/// `cargo test` on a CI runner usually has NONE of those — the runner
/// agent or some background process owns the input chain. The
/// canonical workaround: attach our thread's input queue to whatever
/// process is currently foreground, perform SetForegroundWindow
/// (which now succeeds because we share the foreground thread's
/// queue), then detach.
///
/// Returns `Err` if the foreground didn't actually shift to `window`
/// within a generous timeout — tests need to fail loudly here rather
/// than silently proceeding with a stale foreground, because the
/// daemon's later force_activate relies on the foreground being a
/// known fake EVE window to attach to.
pub fn activate_window_directly(window: u32) -> anyhow::Result<()> {
    let hwnd = HWND(window as usize as *mut std::ffi::c_void);
    unsafe {
        let current_thread = GetCurrentThreadId();
        let foreground = GetForegroundWindow();
        let foreground_thread = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let attached = foreground_thread != 0
            && foreground_thread != current_thread
            && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
        let _ = SetForegroundWindow(hwnd);
        let _ = BringWindowToTop(hwnd);
        if attached {
            let _ = AttachThreadInput(current_thread, foreground_thread, false);
        }
    }
    let deadline = Instant::now() + Duration::from_millis(800);
    while Instant::now() < deadline {
        let current = unsafe { GetForegroundWindow() };
        if current == hwnd {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let final_fg = unsafe { GetForegroundWindow() };
    anyhow::bail!(
        "activate_window_directly: foreground didn't become {:?} within 800ms; \
         final foreground = {:?}. The test runner may not allow cross-process \
         foreground changes — check that the test isn't running in a \
         service-isolated session.",
        hwnd.0,
        final_fg.0
    )
}

/// Grant ALL processes the right to call `SetForegroundWindow`
/// during the next user input event. This is the documented Windows
/// API for automation: by default a process can only take foreground
/// if it was the most-recent input chain owner, which our daemon
/// isn't (we're injecting input from the *test* process, not the
/// daemon process). Without this grant, the daemon's
/// `SetForegroundWindow(target)` is refused by the OS even when the
/// fallback `AttachThreadInput` dance is set up correctly.
///
/// Production users don't need this — a real mouse click is delivered
/// to the actually-focused window, which gives the daemon's hook
/// implicit foreground-stealing rights for the cycle's target. The
/// grant is test-only and lasts only until the next input event.
///
/// `ASFW_ANY` is `u32::MAX` — see Microsoft's docs on
/// `AllowSetForegroundWindow`. Failure (rare; happens if the
/// calling process itself doesn't have foreground rights, e.g. when
/// the desktop is locked) is best-effort; the assertion that follows
/// will surface the underlying problem.
pub fn grant_set_foreground_to_all() {
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// Read screen-space geometry (x, y, w, h) of a window. The Linux
/// counterpart returns root-relative coords; on Windows the analog
/// is `GetWindowRect`, which already returns screen coordinates.
pub fn window_root_geometry(window: u32) -> anyhow::Result<(i32, i32, u32, u32)> {
    let hwnd = HWND(window as usize as *mut std::ffi::c_void);
    let mut rect = RECT::default();
    unsafe {
        GetWindowRect(hwnd, &mut rect)?;
    }
    Ok((
        rect.left,
        rect.top,
        (rect.right - rect.left).max(0) as u32,
        (rect.bottom - rect.top).max(0) as u32,
    ))
}

struct FindAcc {
    target_pid: u32,
    found: Option<HWND>,
}

/// Walk every top-level visible window and return the first whose
/// owning process matches `target_pid` and whose title starts with
/// `EVE - `. Used by the harness to associate each spawned stub
/// child with the window it created.
fn find_window_by_pid(target_pid: u32) -> Option<HWND> {
    let mut acc = FindAcc {
        target_pid,
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut acc as *mut _ as isize));
    }
    acc.found
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let acc = &mut *(lparam.0 as *mut FindAcc);
    if !IsWindowVisible(hwnd).as_bool() {
        return TRUE;
    }
    let mut pid: u32 = 0;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid != acc.target_pid {
        return TRUE;
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return TRUE;
    }
    let mut buf: Vec<u16> = vec![0; len as usize + 1];
    let copied = GetWindowTextW(hwnd, &mut buf);
    if copied <= 0 {
        return TRUE;
    }
    let title = String::from_utf16_lossy(&buf[..copied as usize]);
    if !title.starts_with("EVE - ") {
        return TRUE;
    }
    acc.found = Some(hwnd);
    // Returning FALSE from an EnumWindows callback halts the walk.
    windows::Win32::Foundation::FALSE
}

/// Synthetic input on Windows — wraps `SendInput`. Unlike the Linux
/// uinput-backed counterpart, there's no virtual /dev/input/eventN
/// node here; `SendInput` injects events directly into the system
/// input queue, which is what `RegisterHotKey` watches and what the
/// low-level mouse hook (`WH_MOUSE_LL`) intercepts. The struct is
/// unit-shaped because there's no per-device state.
///
/// The API mirrors the Linux version so cross-platform test code can
/// stay symmetric: `keyboard` / `mouse` "register" a logical set of
/// keys/buttons (no-op on Windows, just for API parity),
/// `devnode` returns `None` (Windows has no device path),
/// `tap` / `tap_with_modifier` inject the events.
pub struct VirtualInput;

impl VirtualInput {
    /// Construct a virtual keyboard. The `_keys` slice is ignored on
    /// Windows (kept for API parity with Linux uinput, which needs to
    /// pre-declare device capabilities). Returns immediately — there's
    /// no async device-node provisioning to wait for.
    pub fn keyboard(_keys: &[u16]) -> anyhow::Result<Self> {
        Ok(VirtualInput)
    }

    /// Construct a virtual mouse. Same semantics as `keyboard` —
    /// `_buttons` is ignored; SendInput doesn't require pre-declared
    /// capabilities.
    pub fn mouse(_buttons: &[u16]) -> anyhow::Result<Self> {
        Ok(VirtualInput)
    }

    /// Linux returns the uinput `/dev/input/eventN` node here. On
    /// Windows there is no equivalent — the daemon's hotkey
    /// registration is global via `RegisterHotKey`, not bound to a
    /// device path. Tests must skip the `keyboard_device_path` /
    /// `mouse_device_path` config fields on Windows.
    #[allow(dead_code)]
    pub fn devnode(&self) -> Option<&PathBuf> {
        None
    }

    /// Press + release a Win32 VIRTUAL_KEY (e.g. `0x7F` for VK_F16,
    /// `0x70` for VK_F1). Inserted into the system input queue via
    /// `SendInput`; the daemon's `RegisterHotKey`-registered thread
    /// receives a `WM_HOTKEY` for any matching combination.
    pub fn tap(&mut self, vk: u16) -> anyhow::Result<()> {
        send_keys(&[(vk, true), (vk, false)])
    }

    /// Hold a modifier (e.g. `0x10` for VK_SHIFT), tap a key,
    /// release the modifier. Order matches what a human types so
    /// `RegisterHotKey`'s modifier mask sees the modifier as held
    /// during the key event.
    pub fn tap_with_modifier(&mut self, modifier_vk: u16, vk: u16) -> anyhow::Result<()> {
        send_keys(&[
            (modifier_vk, true),
            (vk, true),
            (vk, false),
            (modifier_vk, false),
        ])
    }

    /// Press + release a mouse X-button. Argument is the X-button
    /// number: 1 = back (XBUTTON1), 2 = forward (XBUTTON2). The
    /// daemon's low-level mouse hook reads the same encoding from
    /// `MSLLHOOKSTRUCT::mouseData`.
    pub fn tap_xbutton(&mut self, xbutton: u16) -> anyhow::Result<()> {
        send_mouse_xbutton(xbutton)
    }
}

/// Build a sequence of `INPUT_KEYBOARD` events and ship them in a
/// single `SendInput` call. Atomic dispatch matters for modifier
/// combos: the OS shouldn't see the key event without the modifier
/// already-down.
fn send_keys(events: &[(u16, bool)]) -> anyhow::Result<()> {
    let inputs: Vec<INPUT> = events
        .iter()
        .map(|(vk, down)| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(*vk),
                    wScan: 0,
                    dwFlags: if *down {
                        KEYBD_EVENT_FLAGS(0)
                    } else {
                        KEYEVENTF_KEYUP
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        })
        .collect();
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if n as usize != events.len() {
        anyhow::bail!(
            "SendInput accepted {n} of {} events; UIPI may be blocking input",
            events.len()
        );
    }
    Ok(())
}

fn send_mouse_xbutton(xbutton: u16) -> anyhow::Result<()> {
    let inputs = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: xbutton as u32,
                    dwFlags: MOUSEEVENTF_XDOWN,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: xbutton as u32,
                    dwFlags: MOUSEEVENTF_XUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    let n = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if n != 2 {
        anyhow::bail!("SendInput accepted {n} of 2 mouse events");
    }
    Ok(())
}
