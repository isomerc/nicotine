//! Cross-platform test fixtures + the daemon-driver type.
//!
//! Platform-specific fake-EVE harnesses live in `unix.rs` and
//! `windows.rs`; this module re-exports them so tests can use
//! `FakeEveHarness` / `activate_window_directly` / `window_root_geometry`
//! without knowing which platform they're on. `TestDaemon` is itself
//! cross-platform (uses the `interprocess` crate's local-socket API)
//! and lives here.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;

use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::{prelude::*, Stream};

/// Path to the built `Nicotine` binary corresponding to this test
/// run. Cargo puts the bin next to the test binaries (sometimes one
/// level up in `deps/`); this resolves either layout. On Windows the
/// executable has a `.exe` suffix; Cargo + std `Command` handle the
/// extension transparently when invoking, but we need the exact path
/// for the `bin/` resolution.
pub fn nicotine_binary() -> PathBuf {
    locate_bin("Nicotine")
}

/// Path to the built `fake-eve-stub` test helper binary. Only used by
/// the Windows fake-EVE harness, but the locator is cross-platform
/// because Cargo builds the bin on every host (the stub itself is a
/// no-op on non-Windows).
#[cfg_attr(not(windows), allow(dead_code))]
pub fn fake_eve_stub_binary() -> PathBuf {
    locate_bin("fake-eve-stub")
}

fn locate_bin(name: &str) -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // strip the test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    let mut bin = path.join(name);
    // Windows binaries have a `.exe` extension; std::process::Command
    // resolves with or without it but Path::is_file does not.
    if cfg!(windows) && !bin.exists() {
        bin.set_extension("exe");
    }
    bin
}

/// Skip-with-message helper. Returns true if the test should be
/// skipped because the platform's display server isn't available.
/// On Linux that's `$DISPLAY` (X11 / XWayland). On Windows the
/// CI runner's session is always present, so we don't gate.
pub fn skip_if_no_display() -> bool {
    #[cfg(unix)]
    {
        if std::env::var("DISPLAY").is_err() {
            eprintln!(
                "SKIP: DISPLAY not set. Run this test from an X11 / XWayland \
                 session (e.g. `cargo test --ignored fake_eve` from your \
                 desktop terminal)."
            );
            return true;
        }
    }
    false
}

/// Minimal config.toml the test daemon reads. Disables previews so we
/// don't paint 3 squares on the user's actual screen for every test;
/// disables mouse/keyboard input listening because the test drives
/// the daemon over IPC, not via real input events. The dimension
/// fields are required by the Config schema (no `serde(default)`);
/// the values themselves don't matter to these tests beyond what the
/// stack test checks.
const TEST_CONFIG: &str = "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = false
";

/// Owns an isolated daemon subprocess and the temp directories that
/// hold its socket / pipe, runtime files (lock + index), and config.
/// Each test gets a private daemon so multiple test runs in parallel
/// (and any real daemon the user is running) don't fight over the
/// default socket name.
///
/// On drop the daemon is sent `quit`, then killed if it doesn't exit
/// promptly, and the temp directories are removed.
pub struct TestDaemon {
    child: Child,
    /// Socket/pipe printname — `interprocess::local_socket` uses this
    /// to construct the platform-appropriate name (fs path on Linux,
    /// namespaced pipe on Windows).
    socket_printname: String,
    base_dir: PathBuf,
    /// Path of the config.toml the daemon reads. Exposed so tests can
    /// rewrite it mid-run to exercise hot-reload behavior.
    pub config_path: PathBuf,
    /// Snapshot of the env vars we pass to every subprocess invocation
    /// against this daemon — same NICOTINE_SOCKET_PATH /
    /// NICOTINE_RUNTIME_DIR / XDG_CONFIG_HOME (or APPDATA on Windows)
    /// so `nicotine list` and `nicotine active` hit the same isolated
    /// state.
    pub env: Vec<(String, String)>,
}

impl TestDaemon {
    /// Spawn `Nicotine daemon` in an isolated environment and block
    /// until it has bound its IPC socket (or `bail!` after ~2s).
    pub fn spawn() -> anyhow::Result<Self> {
        let base_dir = std::env::temp_dir().join(format!(
            "nicotine-daemon-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let runtime_dir = base_dir.join("runtime");
        // Config isolation: write config.toml into a per-test temp
        // dir and tell the daemon to look there via NICOTINE_CONFIG_DIR.
        // We can't rely on XDG_CONFIG_HOME / APPDATA alone — on
        // Windows `dirs::config_dir()` uses SHGetKnownFolderPath which
        // ignores APPDATA env-var overrides, so the daemon would read
        // the user's real config dir instead of ours. The explicit
        // env override is parallel to NICOTINE_SOCKET_PATH /
        // NICOTINE_RUNTIME_DIR.
        let config_dir = base_dir.join("nicotine");
        std::fs::create_dir_all(&runtime_dir)?;
        std::fs::create_dir_all(&config_dir)?;
        let config_path = config_dir.join("config.toml");
        std::fs::write(&config_path, TEST_CONFIG)?;

        // Unique socket / pipe name per test. On Linux this becomes a
        // filesystem path; on Windows it's a `\\.\pipe\<name>` mapped
        // by the interprocess crate. Either way the env var honored
        // by `ipc::socket_printname()` selects this isolated name
        // instead of the default `/tmp/nicotine.sock` / `nicotine.sock`.
        let socket_printname = if cfg!(windows) {
            format!("nicotine-test-{}.sock", uuid::Uuid::new_v4().simple())
        } else {
            base_dir.join("daemon.sock").to_string_lossy().into_owned()
        };

        let env = vec![
            ("NICOTINE_SOCKET_PATH".to_string(), socket_printname.clone()),
            (
                "NICOTINE_RUNTIME_DIR".to_string(),
                runtime_dir.to_string_lossy().into_owned(),
            ),
            (
                "NICOTINE_CONFIG_DIR".to_string(),
                config_dir.to_string_lossy().into_owned(),
            ),
            // Turn on the daemon's input-pipeline diagnostic log
            // (Windows: mouse hook -> listener -> force_activate;
            // Linux: noop today). Cheap when nothing's happening;
            // surfaces the cycle path's intermediate state when a
            // test calls `diagnostic_log()` after failure.
            ("NICOTINE_DEBUG_INPUT".to_string(), "1".to_string()),
        ];

        let mut cmd = Command::new(nicotine_binary());
        cmd.arg("daemon");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        // Capture both stdout and stderr to log files. Stdout carries
        // the daemon's confirmation messages — including the
        // "Reloaded N character(s) from config.toml" line that
        // `rewrite_config` polls on to know its config write was
        // actually picked up (instead of guessing with a sleep).
        // Stderr is dumped on send-failure for diagnostics.
        let stdout_log = base_dir.join("daemon.stdout.log");
        let stderr_log = base_dir.join("daemon.stderr.log");
        let stdout_file = std::fs::File::create(&stdout_log)?;
        let stderr_file = std::fs::File::create(&stderr_log)?;
        cmd.stdout(stdout_file);
        cmd.stderr(stderr_file);
        let child = cmd.spawn()?;

        // Don't probe with a Stream::connect — on Windows named pipes
        // each accept consumes the current pipe instance and creates
        // the next, so probe-connect-and-drop can race with the
        // daemon's accept loop. Use a fixed warmup sleep instead;
        // 1.2s comfortably covers daemon init + first window-enum
        // tick on a cold Windows runner, and is brief enough on Linux
        // that the existing tests' 21s budget barely moves.
        std::thread::sleep(Duration::from_millis(1200));
        Ok(Self {
            child,
            socket_printname,
            base_dir,
            config_path,
            env,
        })
    }

    /// Dump the daemon's stderr log to the test's stderr. Used by
    /// failure paths to surface why a send / spawn went wrong.
    fn dump_stderr(&self) {
        let log = self.base_dir.join("daemon.stderr.log");
        if let Ok(content) = std::fs::read_to_string(&log) {
            if !content.is_empty() {
                eprint!("[daemon stderr]\n{}", content);
            }
        }
    }

    /// Send a single line command (e.g. "forward", "switch:2") to
    /// the daemon's IPC socket. Returns when the line has been
    /// written + flushed; the daemon's reply (if any) is ignored.
    ///
    /// Retries briefly on ENOENT-style errors so a transient gap in
    /// the listener (e.g. mid-accept on Windows named pipes) doesn't
    /// fail the test. Dumps daemon stderr on final failure so the
    /// underlying reason is visible in CI logs.
    pub fn send(&self, command: &str) -> anyhow::Result<()> {
        let deadline = Instant::now() + Duration::from_millis(2000);
        let mut last_err: Option<anyhow::Error> = None;
        while Instant::now() < deadline {
            match try_connect(&self.socket_printname) {
                Ok(mut stream) => {
                    writeln!(stream, "{}", command)?;
                    stream.flush()?;
                    return Ok(());
                }
                Err(e) => {
                    last_err = Some(e);
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
        self.dump_stderr();
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("send {command:?} timed out with no error captured")
        }))
    }

    /// Run `Nicotine <args>` against this daemon (env vars pinned to
    /// the isolated state). Returns stdout as a String; panics on
    /// non-zero exit. Stderr is captured and forwarded to the test
    /// runner's stderr so debug eprintln from the binary surfaces in
    /// `--nocapture` runs.
    pub fn nicotine_stdout(&self, args: &[&str]) -> String {
        let mut cmd = Command::new(nicotine_binary());
        cmd.args(args);
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        let out = cmd
            .output()
            .unwrap_or_else(|e| panic!("spawn Nicotine {:?}: {}", args, e));
        let stderr = String::from_utf8_lossy(&out.stderr);
        if !stderr.is_empty() {
            eprint!("[Nicotine {:?} stderr]\n{}", args, stderr);
        }
        assert!(
            out.status.success(),
            "Nicotine {:?} exited {}: stderr={:?}",
            args,
            out.status,
            stderr
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Return the daemon's view of the focused EVE client, or None
    /// if no EVE window is focused right now. The nicotine binary
    /// prefixes informational lines like "Detected Wayland display
    /// server..." to stdout, so we filter for the tab-delimited data
    /// lines instead of taking the first line.
    pub fn active_client_name(&self) -> Option<String> {
        let out = self.nicotine_stdout(&["active"]);
        out.lines()
            .find(|line| line.contains('\t'))?
            .split('\t')
            .nth(1)
            .map(|s| s.to_string())
    }

    /// Drive a single full pass of the daemon's hot-reload tick.
    /// Useful after the harness adds/removes a fake EVE window — the
    /// daemon re-enumerates every 500ms, so sleeping past one tick
    /// guarantees the new state is visible.
    pub fn wait_for_enum_tick(&self) {
        std::thread::sleep(Duration::from_millis(700));
    }

    /// Replace the daemon's config.toml with the given content and
    /// block until the daemon has actually picked it up. The write
    /// is staged through a sibling temp file + rename so the daemon
    /// never reads a half-written file (Windows `std::fs::write`
    /// truncates before writing, which the daemon's hot-reload could
    /// race with and parse-fail silently). The completion handshake
    /// polls the daemon's stdout log for "Reloaded N character(s)"
    /// — the daemon emits this line on every successful reload that
    /// changes the character list. Falls back to a plain sleep after
    /// a generous deadline so a non-character-changing config write
    /// doesn't hang the test.
    pub fn rewrite_config(&self, toml: &str) -> anyhow::Result<()> {
        let baseline = self.stdout_log_contents();

        // Atomic replace: write to <path>.tmp, then rename onto path.
        // `std::fs::rename` on Windows uses MoveFileExW with
        // MOVEFILE_REPLACE_EXISTING so the swap is observable as a
        // single transition. On Linux rename(2) is atomic too.
        let tmp = self.config_path.with_extension("toml.tmp");
        std::fs::write(&tmp, toml)?;
        std::fs::rename(&tmp, &self.config_path)?;

        // Any one of these log lines confirms the daemon's hot-reload
        // thread processed our new config:
        //   - "Reloaded N character(s) ..."     — character_order changed
        //   - "Character list cleared ..."      — character_order cleared
        //   - "Hot-reload: mouse config ..."    — mouse_device_path / buttons / enable_mouse
        //   - "Hot-reload: keyboard config ..." — keyboard_device_path / character_hotkeys / etc.
        //   - "Hot-reload: hotkey config ..."   — Windows-only RegisterHotKey rebind path
        const RELOAD_MARKERS: &[&str] = &[
            "Reloaded ",
            "Character list cleared",
            "Hot-reload: mouse config",
            "Hot-reload: keyboard config",
            "Hot-reload: hotkey config",
        ];
        let deadline = Instant::now() + Duration::from_millis(5000);
        while Instant::now() < deadline {
            let now = self.stdout_log_contents();
            if let Some(diff) = now.strip_prefix(&baseline) {
                if RELOAD_MARKERS.iter().any(|m| diff.contains(m)) {
                    // Give the daemon one more tick to also apply
                    // any side effects (hotkey rebind on Windows,
                    // listener mutex updates on Linux). 100ms is
                    // plenty; the work itself is sub-ms.
                    std::thread::sleep(Duration::from_millis(100));
                    return Ok(());
                }
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        // No marker observed within the deadline. Surface both daemon
        // log streams so CI shows exactly what (if anything) the
        // daemon thought about the new config — was the file read?
        // did parse fail? Bail explicitly rather than fall through
        // and let the downstream assertion fail mysteriously.
        let stdout = self.stdout_log_contents();
        let stderr = self.stderr_log_contents();
        anyhow::bail!(
            "rewrite_config: daemon never logged a config reload within 5s.\n\
             --- config written ({} bytes) ---\n{}\n\
             --- daemon stdout ---\n{}\n\
             --- daemon stderr ---\n{}",
            toml.len(),
            toml,
            stdout,
            stderr
        );
    }

    fn stdout_log_contents(&self) -> String {
        std::fs::read_to_string(self.base_dir.join("daemon.stdout.log")).unwrap_or_default()
    }

    fn stderr_log_contents(&self) -> String {
        std::fs::read_to_string(self.base_dir.join("daemon.stderr.log")).unwrap_or_default()
    }

    /// Public accessor for the daemon's captured stderr. Tests that
    /// need to parse the `NICOTINE_DEBUG_INPUT` diagnostic stream
    /// (e.g. to assert on `force_activate: target=` lines) use this
    /// rather than going through `diagnostic_log` which is formatted
    /// for human reading. Only the Windows mouse-button test
    /// currently parses this; cfg-gate the dead_code allow so
    /// non-windows builds don't warn.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn stderr_log_contents_pub(&self) -> String {
        self.stderr_log_contents()
    }

    /// Concatenated stdout + stderr from the daemon, formatted for
    /// inclusion in test failure messages. Tests should call this in
    /// assertion failure paths so CI logs surface the daemon's view of
    /// the world (open-failed devices, parse errors, etc.) rather
    /// than just "got X expected Y".
    pub fn diagnostic_log(&self) -> String {
        let stdout = self.stdout_log_contents();
        let stderr = self.stderr_log_contents();
        format!(
            "\n--- daemon stdout ---\n{}\n--- daemon stderr ---\n{}",
            if stdout.is_empty() {
                "(empty)"
            } else {
                &stdout
            },
            if stderr.is_empty() {
                "(empty)"
            } else {
                &stderr
            }
        )
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        // Try a clean shutdown first via the daemon's own quit
        // command so it tears down its display-server connections +
        // hot-reload thread gracefully. Failure (already dead, socket
        // gone) is fine — the kill below covers it.
        let _ = self.send("quit");
        let deadline = Instant::now() + Duration::from_millis(200);
        while Instant::now() < deadline {
            if let Ok(Some(_)) = self.child.try_wait() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.base_dir);
    }
}

/// Probe-connect to the local-socket name. Returns Ok(stream) when
/// the daemon is accepting connections, Err otherwise. Used both as
/// the wait-for-bind handshake and as the underlying transport for
/// `send`.
fn try_connect(printname: &str) -> anyhow::Result<Stream> {
    #[cfg(unix)]
    {
        let name = printname.to_fs_name::<GenericFilePath>()?;
        Ok(Stream::connect(name)?)
    }
    #[cfg(windows)]
    {
        let name = printname.to_ns_name::<GenericNamespaced>()?;
        Ok(Stream::connect(name)?)
    }
}
