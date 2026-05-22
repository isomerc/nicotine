//! Test helper: a Windows GUI process that, when invoked under the
//! filename `exefile.exe`, looks identical to a real EVE Online client
//! to Nicotine's enumeration + filter pipeline. The test harness
//! copies this binary into a temp dir as `exefile.exe`, then spawns
//! N instances with a `--title-suffix <name>` arg; each instance
//! creates a top-level window titled `EVE - <name>` and pumps
//! messages until killed.
//!
//! On non-Windows hosts the binary still builds (so Cargo's
//! cross-compile and Linux CI don't break) but the body is a no-op
//! `exit(0)` — the Linux fake-EVE harness uses fork+prctl directly,
//! it doesn't need this stub.
//!
//! Not bundled in releases: `release.yml` copies the `Nicotine`
//! binary specifically, so this helper never ships to end users.

fn main() {
    #[cfg(windows)]
    windows_main::run();
    #[cfg(not(windows))]
    {
        eprintln!("fake-eve-stub is a Windows-only test helper; nothing to do on this platform");
    }
}

#[cfg(windows)]
mod windows_main {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStrExt;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, LoadCursorW,
        PostQuitMessage, RegisterClassW, ShowWindow, TranslateMessage, CW_USEDEFAULT, IDC_ARROW,
        MSG, SW_SHOWNOACTIVATE, WINDOW_EX_STYLE, WM_DESTROY, WNDCLASSW, WS_OVERLAPPEDWINDOW,
    };

    /// UTF-16 encode + NUL-terminate. The Win32 W-suffixed APIs take
    /// `PCWSTR` which is a pointer to a NUL-terminated wchar_t array.
    fn wide(s: &str) -> Vec<u16> {
        OsString::from(s)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    pub fn run() {
        let args: Vec<String> = std::env::args().collect();
        // Accept either `<exe> Alpha` or `<exe> --title-suffix Alpha`
        // so test setup code can be obvious about what it's passing.
        let suffix = args
            .iter()
            .skip(1)
            .position(|a| a == "--title-suffix")
            .and_then(|i| args.get(i + 2).cloned())
            .or_else(|| args.get(1).cloned())
            .unwrap_or_else(|| "Unknown".to_string());
        let title = format!("EVE - {}", suffix);
        let title_w = wide(&title);
        let class_w = wide("NicotineFakeEveStub");

        unsafe {
            let hinstance_mod = GetModuleHandleW(None).expect("GetModuleHandleW");
            let hinstance = HINSTANCE(hinstance_mod.0);

            // Cursor is best-effort — without it the class still
            // registers, the window just doesn't change the cursor on
            // hover. Irrelevant for a test fixture.
            let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();

            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinstance,
                lpszClassName: PCWSTR(class_w.as_ptr()),
                hCursor: cursor,
                ..Default::default()
            };
            // Ignore the atom — re-registration in the same process
            // can fail without consequence for our use (we only ever
            // register once per instance).
            let _ = RegisterClassW(&wc);

            // Place the window deliberately off-screen so it doesn't
            // intrude on whatever the user is doing during local
            // runs. CW_USEDEFAULT for size lets Windows pick a normal
            // top-level size, which keeps EnumWindows + IsWindowVisible
            // happy (a 0-sized window would still be visible per the
            // style bit, but real WMs are sometimes finicky about it).
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                PCWSTR(class_w.as_ptr()),
                PCWSTR(title_w.as_ptr()),
                WS_OVERLAPPEDWINDOW,
                -32000, // off-screen X
                -32000, // off-screen Y
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                None,
                None,
                Some(hinstance),
                None,
            )
            .expect("CreateWindowExW");

            // SW_SHOWNOACTIVATE: visible (so IsWindowVisible passes
            // in enum_collect_eve) but doesn't steal focus from the
            // test driver. The test will explicitly activate windows
            // as needed.
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).into() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == WM_DESTROY {
            PostQuitMessage(0);
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wparam, lparam)
    }
}
