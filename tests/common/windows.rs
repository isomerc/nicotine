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
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    GetWindowThreadProcessId, IsWindowVisible, SetForegroundWindow,
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
/// Nicotine. SetForegroundWindow is the obvious choice; on modern
/// Windows it's subject to focus-stealing restrictions, but our test
/// process is the foreground one when we call this so it succeeds.
pub fn activate_window_directly(window: u32) -> anyhow::Result<()> {
    let hwnd = HWND(window as usize as *mut std::ffi::c_void);
    unsafe {
        // Best-effort: SetForegroundWindow can return FALSE under
        // focus restrictions even when the foreground change DID
        // happen. Don't error on FALSE; later assertions will reveal
        // whether the activation actually landed.
        let _ = SetForegroundWindow(hwnd);
    }
    // Poll GetForegroundWindow until it reflects our request or we
    // time out. Mirrors the Linux activate_window_directly contract
    // so tests see a deterministic starting focus.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let current = unsafe { GetForegroundWindow() };
        if current == hwnd {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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
