use anyhow::{Context, Result};
#[allow(unused_imports)]
use interprocess::local_socket::{
    prelude::*, GenericFilePath, GenericNamespaced, ListenerOptions, Stream,
};
use std::io::Write;

#[cfg(unix)]
const DEFAULT_SOCKET_PRINTNAME: &str = "/tmp/nicotine.sock";
#[cfg(windows)]
const DEFAULT_SOCKET_PRINTNAME: &str = "nicotine.sock";

/// Resolve the socket / named-pipe path. Honors `NICOTINE_SOCKET_PATH`
/// when set so integration tests can run an isolated daemon without
/// clashing with a user's real daemon on the default path. Production
/// callers leave the env var unset and get the original behavior.
fn socket_printname() -> String {
    std::env::var("NICOTINE_SOCKET_PATH").unwrap_or_else(|_| DEFAULT_SOCKET_PRINTNAME.to_string())
}

fn socket_name() -> Result<interprocess::local_socket::Name<'static>> {
    let printname = socket_printname();
    #[cfg(unix)]
    {
        printname
            .to_fs_name::<GenericFilePath>()
            .context("Failed to construct socket name")
    }
    #[cfg(windows)]
    {
        printname
            .to_ns_name::<GenericNamespaced>()
            .context("Failed to construct named pipe name")
    }
}

pub fn bind_listener() -> Result<interprocess::local_socket::Listener> {
    // Remove stale Unix socket file from a previous run (no-op on Windows named pipes).
    #[cfg(unix)]
    let _ = std::fs::remove_file(socket_printname());

    let name = socket_name()?;
    ListenerOptions::new()
        .name(name)
        .create_sync()
        .context("Failed to bind IPC listener")
}

pub fn send_line(message: &str) -> Result<()> {
    let name = socket_name()?;
    let mut stream =
        Stream::connect(name).context("Failed to connect to nicotine daemon — is it running?")?;
    writeln!(stream, "{}", message)?;
    stream.flush()?;
    Ok(())
}

#[allow(dead_code)]
pub fn daemon_running() -> bool {
    socket_name()
        .and_then(|n| Stream::connect(n).map_err(Into::into))
        .is_ok()
}
