//! Process-name based EVE client detection. The title-only filter
//! `starts_with("EVE - ") && !contains("Launcher")` accidentally
//! matches browser tabs viewing EVE-related pages, Discord channels
//! named "EVE - …", third-party EVE tools, and similar — a real
//! user-reported bug was a preview window getting created for an
//! "EVE Online application which is weird".
//!
//! The reliable signal is the process executable name. Both Windows
//! (native) and Linux (Wine/Proton) run the EVE client as `exefile.exe`,
//! which is the original CCP binary name. Anything else matching the
//! title heuristic isn't the game.

/// Pure: does the basename of a Windows executable path equal
/// `exefile.exe` (case-insensitive)? Used by the Windows-side
/// `pid_is_eve_client` to filter `QueryFullProcessImageNameW` output.
/// Lives here so it can be unit-tested on any host. The Linux build
/// doesn't call this directly (it uses /proc/<pid>/comm instead), but
/// the tests still exercise it.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn path_basename_is_eve_exe(path: &str) -> bool {
    path.rsplit(['\\', '/'])
        .next()
        .map(|name| name.eq_ignore_ascii_case("exefile.exe"))
        .unwrap_or(false)
}

/// Linux/Unix: returns true if `/proc/<pid>/comm` is `exefile.exe`.
/// `comm` is the in-kernel short name of the executable (truncated to
/// 15 chars but `exefile.exe` is 11). Wine/Proton preserves the comm
/// name from the EXE it's running, so EVE under Steam-Proton ends up
/// with comm = `exefile.exe` exactly. Returns false for missing or
/// unreadable /proc entries — defensive against the source window
/// disappearing between enumeration and the comm read.
#[cfg(unix)]
pub fn pid_is_eve_client(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{}/comm", pid))
        .map(|s| s.trim() == "exefile.exe")
        .unwrap_or(false)
}

/// Windows: returns true if the process backing `pid` is the EVE
/// client. Resolves the full executable image path via
/// `QueryFullProcessImageNameW` and compares the basename against
/// `exefile.exe` via the pure `path_basename_is_eve_exe`. Returns
/// false for any failure (process gone, access denied, etc.).
#[cfg(windows)]
pub fn pid_is_eve_client(pid: u32) -> bool {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        let mut buf = [0u16; 1024];
        let mut size = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if result.is_err() {
            return false;
        }
        let path = String::from_utf16_lossy(&buf[..size as usize]);
        path_basename_is_eve_exe(&path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_basename_matches_canonical_steam_path() {
        assert!(path_basename_is_eve_exe(
            "C:\\Program Files (x86)\\Steam\\steamapps\\common\\EVE\\bin64\\exefile.exe"
        ));
    }

    #[test]
    fn path_basename_is_case_insensitive() {
        assert!(path_basename_is_eve_exe("C:\\anywhere\\ExeFile.EXE"));
        assert!(path_basename_is_eve_exe("EXEFILE.EXE"));
    }

    #[test]
    fn path_basename_rejects_non_eve_executables() {
        assert!(!path_basename_is_eve_exe(
            "C:\\Windows\\System32\\notepad.exe"
        ));
        assert!(!path_basename_is_eve_exe("exefile2.exe"));
        assert!(!path_basename_is_eve_exe("exefile.ex"));
    }

    #[test]
    fn path_basename_handles_forward_slash_separators() {
        // QueryFullProcessImageNameW returns backslash-separated paths,
        // but Wine on Linux occasionally normalises to forward slashes
        // when crossing the prefix boundary. Accept both.
        assert!(path_basename_is_eve_exe(
            "/wine/prefix/drive_c/eve/exefile.exe"
        ));
    }

    #[test]
    fn path_basename_handles_bare_filename() {
        assert!(path_basename_is_eve_exe("exefile.exe"));
    }

    #[test]
    fn path_basename_rejects_empty_string() {
        assert!(!path_basename_is_eve_exe(""));
    }
}
