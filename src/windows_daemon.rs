use anyhow::{Context, Result};
use std::os::windows::process::CommandExt;
use std::process::Command;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const DETACHED_PROCESS: u32 = 0x0000_0008;

/// Re-spawn the current executable as a detached background process. The
/// child inherits no console (CREATE_NO_WINDOW) and is not tied to the
/// parent's console session (DETACHED_PROCESS). The env var
/// NICOTINE_DAEMON_CHILD signals to the child's main() that it should run
/// the daemon directly instead of spawning another child.
pub fn spawn_detached_self() -> Result<()> {
    let exe = std::env::current_exe().context("Failed to locate own executable")?;

    Command::new(exe)
        .arg("start")
        .env("NICOTINE_DAEMON_CHILD", "1")
        .creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS)
        .spawn()
        .context("Failed to spawn detached daemon process")?;

    Ok(())
}
