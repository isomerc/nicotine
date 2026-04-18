use anyhow::{Context, Result};
#[allow(unused_imports)]
use interprocess::local_socket::{
    prelude::*, GenericFilePath, GenericNamespaced, ListenerOptions, Stream,
};
use std::io::Write;

#[cfg(unix)]
const SOCKET_PRINTNAME: &str = "/tmp/nicotine.sock";
#[cfg(windows)]
const SOCKET_PRINTNAME: &str = "nicotine.sock";

fn socket_name() -> Result<interprocess::local_socket::Name<'static>> {
    #[cfg(unix)]
    {
        SOCKET_PRINTNAME
            .to_fs_name::<GenericFilePath>()
            .context("Failed to construct socket name")
    }
    #[cfg(windows)]
    {
        SOCKET_PRINTNAME
            .to_ns_name::<GenericNamespaced>()
            .context("Failed to construct named pipe name")
    }
}

pub fn bind_listener() -> Result<interprocess::local_socket::Listener> {
    // Remove stale Unix socket file from a previous run (no-op on Windows named pipes).
    #[cfg(unix)]
    let _ = std::fs::remove_file(SOCKET_PRINTNAME);

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
