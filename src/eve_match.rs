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
