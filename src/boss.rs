//! "Boss key" support: panic actions that hide or kill everything.
//!
//! Two modes, bound to separate hotkeys in the config:
//!   * **Hide + mute** (a toggle): minimize every EVE client, mute their
//!     audio, and hide the overlay. Press again to restore. Implemented in
//!     the input listeners (which own the
//!     window-manager handle and the live `bossed` flag); the audio + pid
//!     helpers live here.
//!   * **Kill all**: force-terminate every EVE client and every Nicotine
//!     process (including this one). No confirmation — that's the point.
//!
//! Process matching reuses the same executable-name signal as
//! [`crate::eve_match`]: EVE clients are `exefile.exe`; Nicotine is
//! `Nicotine`/`nicotine` (Linux) or `nicotine.exe` (Windows).

/// PIDs of all running EVE clients (`exefile.exe`), including the
/// pre-login client.
#[cfg(unix)]
pub fn eve_client_pids() -> Vec<u32> {
    pids_with_comm(|comm| comm == "exefile.exe")
}

/// PIDs of all Nicotine processes (daemon + config panel).
#[cfg(unix)]
pub fn nicotine_pids() -> Vec<u32> {
    pids_with_comm(|comm| comm.eq_ignore_ascii_case("nicotine"))
}

/// Scan `/proc` for processes whose `comm` matches `pred`.
#[cfg(unix)]
fn pids_with_comm(pred: impl Fn(&str) -> bool) -> Vec<u32> {
    let mut pids = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return pids;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        if let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) {
            if pred(comm.trim()) {
                pids.push(pid);
            }
        }
    }
    pids
}

/// Force-kill every EVE client and every Nicotine process. Kills others
/// first, then exits this process last so the remaining kills always run.
#[cfg(unix)]
pub fn kill_all() -> ! {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let self_pid = std::process::id();
    let mut targets: Vec<u32> = eve_client_pids();
    targets.extend(nicotine_pids());
    for pid in targets {
        if pid == self_pid {
            continue;
        }
        let _ = kill(Pid::from_raw(pid as i32), Signal::SIGKILL);
    }
    std::process::exit(0);
}

/// Mute the PulseAudio/PipeWire sink-inputs belonging to `eve_pids`.
/// Returns the sink-input indices we muted so [`unmute`] can restore
/// exactly those. Best-effort: any failure (no `pactl`, no audio server,
/// unparseable output) yields an empty list rather than an error.
#[cfg(unix)]
pub fn mute_eve(eve_pids: &[u32]) -> Vec<u32> {
    let mut muted = Vec::new();
    let Ok(output) = std::process::Command::new("pactl")
        .args(["-f", "json", "list", "sink-inputs"])
        .output()
    else {
        return muted;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return muted;
    };
    let Some(inputs) = json.as_array() else {
        return muted;
    };
    for input in inputs {
        let pid = input
            .get("properties")
            .and_then(|p| p.get("application.process.id"))
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<u32>().ok());
        let index = input.get("index").and_then(|v| v.as_u64());
        if let (Some(pid), Some(index)) = (pid, index) {
            if eve_pids.contains(&pid) {
                let _ = std::process::Command::new("pactl")
                    .args(["set-sink-input-mute", &index.to_string(), "1"])
                    .status();
                muted.push(index as u32);
            }
        }
    }
    muted
}

/// Unmute the sink-inputs previously muted by [`mute_eve`].
#[cfg(unix)]
pub fn unmute(sink_inputs: &[u32]) {
    for &index in sink_inputs {
        let _ = std::process::Command::new("pactl")
            .args(["set-sink-input-mute", &index.to_string(), "0"])
            .status();
    }
}

// ---- Windows ----

/// PIDs of all running EVE clients (`exefile.exe`).
#[cfg(windows)]
pub fn eve_client_pids() -> Vec<u32> {
    pids_with_exe(|exe| exe.eq_ignore_ascii_case("exefile.exe"))
}

/// PIDs of all Nicotine processes.
#[cfg(windows)]
pub fn nicotine_pids() -> Vec<u32> {
    pids_with_exe(|exe| exe.eq_ignore_ascii_case("nicotine.exe"))
}

/// Enumerate processes via the toolhelp snapshot, returning PIDs whose
/// executable basename matches `pred`.
#[cfg(windows)]
fn pids_with_exe(pred: impl Fn(&str) -> bool) -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                let end = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let exe = String::from_utf16_lossy(&entry.szExeFile[..end]);
                if pred(&exe) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snapshot);
    }
    pids
}

/// Force-kill every EVE client and every Nicotine process, then exit.
#[cfg(windows)]
pub fn kill_all() -> ! {
    use windows::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
    let self_pid = std::process::id();
    let mut targets = eve_client_pids();
    targets.extend(nicotine_pids());
    for pid in targets {
        if pid == self_pid {
            continue;
        }
        unsafe {
            if let Ok(handle) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(handle, 1);
                let _ = windows::Win32::Foundation::CloseHandle(handle);
            }
        }
    }
    std::process::exit(0);
}

/// Per-application audio muting isn't wired up on Windows yet (it needs the
/// Core Audio session API, `ISimpleAudioVolume`). The boss key still hides
/// and minimizes clients; muting is a no-op here for now.
#[cfg(windows)]
pub fn mute_eve(_eve_pids: &[u32]) -> Vec<u32> {
    Vec::new()
}

/// No-op counterpart to the Windows [`mute_eve`] stub.
#[cfg(windows)]
pub fn unmute(_sink_inputs: &[u32]) {}
