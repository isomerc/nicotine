//! Test fixture: spin up "fake EVE" — real X11 windows with the
//! title shape Nicotine looks for, backed by real processes whose
//! `/proc/<pid>/comm` reads as `exefile.exe`. This is enough state to
//! exercise the full enumeration + filtering path (`get_eve_windows`,
//! `pid_is_eve_client`) without launching the actual game.
//!
//! The child processes are created via `fork()` + `prctl(PR_SET_NAME)`
//! rather than by spawning an external binary — that sidesteps the
//! coreutils-sleep multi-call problem (it overwrites its own comm
//! based on argv[0] after the kernel sets the initial value).
//!
//! Cleanup runs on `Drop`: child processes are killed and reaped;
//! X11 windows go away with the connection. The fixture is
//! Linux-only (X11 + /proc + Linux-specific prctl).

#![cfg(unix)]

use nix::libc;
use nix::sys::prctl;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag};
use nix::unistd::{fork, ForkResult, Pid};
use std::ffi::CStr;
use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

const FAKE_COMM: &CStr = c"exefile.exe";

/// One fake EVE client: a forked child whose comm is `exefile.exe`,
/// paired with a real X11 window whose title is `EVE - <name>` and
/// whose `_NET_WM_PID` points at that child. Fields are public so
/// future tests can match on pid/window IDs; the existing two only
/// read `name` indirectly via the harness so the unused-field warning
/// would fire without this allow.
#[allow(dead_code)]
pub struct FakeEveClient {
    pub name: String,
    pub pid: u32,
    pub window: u32,
}

/// Owns the fixture. Holding this alive holds the windows + processes
/// alive; dropping it tears both down.
pub struct FakeEveHarness {
    pub clients: Vec<FakeEveClient>,
    conn: RustConnection,
}

impl FakeEveHarness {
    /// Build a harness with one fake client per name. The names are
    /// what should appear after "EVE - " in window titles.
    pub fn new(names: &[&str]) -> anyhow::Result<Self> {
        let (conn, screen_num) = RustConnection::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;

        let net_wm_name = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let net_wm_pid = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;

        let mut clients = Vec::with_capacity(names.len());
        for (idx, name) in names.iter().enumerate() {
            let pid = spawn_fake_eve_process()?;

            let window = conn.generate_id()?;
            // Bigger than 1x1 with a non-zero background — KWin under
            // Wayland XWayland refuses to focus windows that don't
            // have a real surface to back them. A 1x1 window with no
            // background_pixel got created, mapped, but never gave
            // KWin a Wayland surface to assign focus to, which broke
            // the cycle / switch tests with no active client visible
            // post-activation. 320x200 + a solid background_pixel is
            // enough for KWin to spin up a surface; size is still
            // small enough not to be visually intrusive during tests.
            // Offset each window so they don't all overlap (helps
            // the WM treat them as independent toplevels).
            let aux = CreateWindowAux::new().background_pixel(screen.black_pixel);
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                window,
                root,
                (idx as i16) * 20,
                0,
                320,
                200,
                0,
                WindowClass::INPUT_OUTPUT,
                0,
                &aux,
            )?;

            let title = format!("EVE - {}", name);
            conn.change_property(
                PropMode::REPLACE,
                window,
                net_wm_name,
                utf8_string,
                8,
                title.len() as u32,
                title.as_bytes(),
            )?;
            conn.change_property32(
                PropMode::REPLACE,
                window,
                net_wm_pid,
                AtomEnum::CARDINAL,
                &[pid],
            )?;
            conn.map_window(window)?;

            clients.push(FakeEveClient {
                name: name.to_string(),
                pid,
                window,
            });
        }

        conn.flush()?;

        // Window managers update _NET_CLIENT_LIST asynchronously in
        // response to MapNotify. 250ms is well over the typical
        // KWin/Mutter/Sway update latency on a desktop; on an
        // unloaded box it's usually <20ms.
        std::thread::sleep(std::time::Duration::from_millis(250));

        Ok(Self { clients, conn })
    }

    /// Spawn an extra "lookalike" client whose title starts with
    /// `EVE - ` but whose backing process is NOT named `exefile.exe`.
    /// Used to verify the process-name filter rejects non-EVE windows
    /// that happen to share the title prefix (browser tab, Discord
    /// channel, etc. — the real-world reason `pid_is_eve_client`
    /// exists). Returns the created window id.
    pub fn add_lookalike(&mut self, title_suffix: &str) -> anyhow::Result<u32> {
        // Same fork pattern but DON'T set comm — the child inherits
        // the parent's comm ("fake_eve-…test runner") which definitely
        // isn't "exefile.exe".
        let pid = match unsafe { fork() }? {
            ForkResult::Parent { child } => child.as_raw() as u32,
            ForkResult::Child => {
                // SAFETY: only async-signal-safe calls below — pause + _exit.
                // _exit returns ! so the match arm has type !, coercing to
                // u32 for the parent's value.
                unsafe {
                    libc::pause();
                    libc::_exit(0)
                }
            }
        };

        let screen = &self.conn.setup().roots[0];
        let root = screen.root;

        let net_wm_name = self.conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string = self.conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let net_wm_pid = self.conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;

        let window = self.conn.generate_id()?;
        let aux = CreateWindowAux::new().background_pixel(screen.black_pixel);
        self.conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            320,
            200,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &aux,
        )?;
        let title = format!("EVE - {}", title_suffix);
        self.conn.change_property(
            PropMode::REPLACE,
            window,
            net_wm_name,
            utf8_string,
            8,
            title.len() as u32,
            title.as_bytes(),
        )?;
        self.conn.change_property32(
            PropMode::REPLACE,
            window,
            net_wm_pid,
            AtomEnum::CARDINAL,
            &[pid],
        )?;
        self.conn.map_window(window)?;
        self.conn.flush()?;

        self.clients.push(FakeEveClient {
            name: format!("LOOKALIKE:{}", title_suffix),
            pid,
            window,
        });

        std::thread::sleep(std::time::Duration::from_millis(250));
        Ok(window)
    }
}

impl Drop for FakeEveHarness {
    fn drop(&mut self) {
        for c in &self.clients {
            let pid = Pid::from_raw(c.pid as i32);
            let _ = kill(pid, Signal::SIGTERM);
            // Reap with WNOHANG first; if the child hasn't exited
            // yet, SIGKILL it and reap again. Without the reap step
            // the child becomes a zombie and lingers until the test
            // binary exits, polluting later runs that look at
            // /proc/<pid>/comm.
            let _ = waitpid(pid, Some(WaitPidFlag::WNOHANG));
            let _ = kill(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
        }
        // X windows die with the connection automatically.
    }
}

/// Fork a child whose `/proc/<pid>/comm` reads as `exefile.exe`, and
/// which blocks forever (pause(2)) until killed by the test
/// teardown. Returns the child's PID. The child runs only
/// async-signal-safe calls so this is safe even from the
/// multi-threaded Cargo test harness.
fn spawn_fake_eve_process() -> anyhow::Result<u32> {
    match unsafe { fork() }? {
        ForkResult::Parent { child } => Ok(child.as_raw() as u32),
        ForkResult::Child => {
            // SAFETY: every call here is documented async-signal-safe.
            // prctl is on the safe list (signal-safety(7)); pause and
            // _exit are too. We intentionally do NOT call exit() or
            // any Drop machinery — that would run the parent's
            // destructors against shared state (libc allocator,
            // x11rb buffers) and could deadlock.
            let _ = prctl::set_name(FAKE_COMM);
            unsafe {
                libc::pause();
                libc::_exit(0)
            }
        }
    }
}

/// Path to the built nicotine binary corresponding to this test run.
/// Cargo puts the bin next to the test binaries (sometimes one level
/// up in `deps/`); this resolves either layout.
pub fn nicotine_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("current_exe");
    path.pop(); // strip the test binary name
    if path.ends_with("deps") {
        path.pop();
    }
    // Binary name is capitalized "Nicotine" per Cargo.toml [[bin]].
    path.join("Nicotine")
}

/// Skip-with-message helper. Returns true if the test should be
/// skipped because X11 isn't available (e.g. running headless).
pub fn skip_if_no_display() -> bool {
    if std::env::var("DISPLAY").is_err() {
        eprintln!(
            "SKIP: DISPLAY not set. Run this test from an X11 / XWayland \
             session (e.g. `cargo test --ignored fake_eve` from your \
             desktop terminal)."
        );
        return true;
    }
    false
}

/// Minimal config.toml the test daemon reads. Disables previews so we
/// don't paint 3 squares on the user's actual screen for every test;
/// disables mouse/keyboard input listening because the test drives
/// the daemon over IPC, not via real input events. The dimension
/// fields are required by the Config schema (no `serde(default)`);
/// the values themselves don't matter to these tests because nothing
/// asserts on stacked geometry.
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
/// hold its socket, runtime files (lock + index), and config. Each
/// test gets a private daemon so multiple test runs in parallel (and
/// any real daemon the user is running) don't fight over the default
/// `/tmp/nicotine.sock` path.
///
/// On drop the daemon is sent `quit`, then SIGKILL'd if it doesn't
/// exit promptly, and the temp directories are removed.
pub struct TestDaemon {
    child: Child,
    socket_path: PathBuf,
    base_dir: PathBuf,
    /// Snapshot of the env vars we pass to every subprocess invocation
    /// against this daemon — same NICOTINE_SOCKET_PATH /
    /// NICOTINE_RUNTIME_DIR / XDG_CONFIG_HOME so `nicotine list` and
    /// `nicotine active` hit the same isolated state.
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
        let config_dir_root = base_dir.join("config");
        let config_dir = config_dir_root.join("nicotine");
        std::fs::create_dir_all(&runtime_dir)?;
        std::fs::create_dir_all(&config_dir)?;
        std::fs::write(config_dir.join("config.toml"), TEST_CONFIG)?;

        let socket_path = base_dir.join("daemon.sock");

        let env = vec![
            (
                "NICOTINE_SOCKET_PATH".to_string(),
                socket_path.to_string_lossy().into_owned(),
            ),
            (
                "NICOTINE_RUNTIME_DIR".to_string(),
                runtime_dir.to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".to_string(),
                config_dir_root.to_string_lossy().into_owned(),
            ),
        ];

        let mut cmd = Command::new(nicotine_binary());
        cmd.arg("daemon");
        for (k, v) in &env {
            cmd.env(k, v);
        }
        // Daemon prints "listening for IPC commands" once bound. We
        // don't read its output but Stdio::null avoids pipe-fill
        // hangs across longer test runs.
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::null());
        let child = cmd.spawn()?;

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if socket_path.exists() {
                // Socket file exists ≠ accepting yet on some IPC
                // backends; do a connect probe.
                if UnixStream::connect(&socket_path).is_ok() {
                    // Daemon's enumeration loop ticks every 500ms;
                    // give it one tick to discover the harness's
                    // fake windows before the test sends commands.
                    std::thread::sleep(Duration::from_millis(600));
                    return Ok(Self {
                        child,
                        socket_path,
                        base_dir,
                        env,
                    });
                }
            }
            std::thread::sleep(Duration::from_millis(40));
        }
        anyhow::bail!("test daemon never bound its IPC socket");
    }

    /// Send a single line command (e.g. "forward", "switch:2") to
    /// the daemon's IPC socket. Returns when the line has been
    /// written + flushed; the daemon's reply (if any) is ignored.
    pub fn send(&self, command: &str) -> anyhow::Result<()> {
        let mut stream = UnixStream::connect(&self.socket_path)?;
        writeln!(stream, "{}", command)?;
        stream.flush()?;
        Ok(())
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
    /// if no EVE window is focused right now. The daemon polls
    /// `_NET_ACTIVE_WINDOW` and matches against its enumerated
    /// EVE list — so this is the same observation the cycle /
    /// switch code uses to decide what to advance from. The nicotine
    /// binary prefixes informational lines like "Detected Wayland
    /// display server..." to stdout, so we filter for the
    /// tab-delimited data lines instead of taking the first line.
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
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        // Try a clean shutdown first via the daemon's own quit
        // command, so it tears down its X11 connection + hot-reload
        // thread gracefully. Failure (already dead, socket gone)
        // is fine — the SIGKILL below covers it.
        let _ = self.send("quit");
        // Wait briefly for the process to exit on its own.
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

/// Cross-test helper: read stdout from `nicotine list` (no daemon)
/// using the same env-var isolation we'd use against a daemon. Useful
/// for the no-daemon variants of enumeration tests so the user's
/// real daemon (if running) doesn't perturb them.
#[allow(dead_code)]
pub fn nicotine_list_isolated() -> String {
    let base_dir = std::env::temp_dir().join(format!(
        "nicotine-list-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::create_dir_all(&base_dir).expect("mk base_dir");
    let _guard = TempDirGuard(base_dir.clone());

    let mut cmd = Command::new(nicotine_binary());
    cmd.arg("list");
    cmd.env("NICOTINE_SOCKET_PATH", base_dir.join("no.sock"));
    cmd.env("NICOTINE_RUNTIME_DIR", &base_dir);
    cmd.env("XDG_CONFIG_HOME", &base_dir);
    let out = cmd.output().expect("spawn nicotine list");
    assert!(out.status.success(), "nicotine list exit {}", out.status);
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Best-effort cleanup of a temp directory when the wrapping scope
/// exits — used by `nicotine_list_isolated` so we don't leak fixtures
/// into /tmp on every test run.
struct TempDirGuard(PathBuf);
impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Activate one of the harness's fake EVE windows directly via X11,
/// bypassing nicotine. Used as the "set known starting point" step
/// before testing what `nicotine forward` does to focus. Returns
/// only after the WM has actually applied the change (polled with a
/// short timeout) so subsequent cycle commands see a deterministic
/// initial state.
pub fn activate_window_directly(window: u32) -> anyhow::Result<()> {
    let (conn, screen_num) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let net_active = conn
        .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
        .reply()?
        .atom;
    // Send the same EWMH ClientMessage nicotine sends. source = 2
    // (pager) tells the WM this is an explicit user request and
    // should bypass focus-stealing prevention.
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window,
        type_: net_active,
        data: ClientMessageData::from([2u32, x11rb::CURRENT_TIME, 0, 0, 0]),
    };
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event,
    )?;
    conn.flush()?;
    // Poll _NET_ACTIVE_WINDOW for up to 500ms until it reflects our
    // requested window. Some WMs (KWin under load) take a few frames
    // to update the property.
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline {
        let reply = conn
            .get_property(false, root, net_active, AtomEnum::WINDOW, 0, 1)?
            .reply()?;
        let active = reply.value32().and_then(|mut v| v.next()).unwrap_or(0);
        if active == window {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Didn't observe the activation — return Ok so the caller still
    // runs the rest of the test; assertions later will surface the
    // misbehavior if it matters.
    Ok(())
}

/// Resolve the directory that the test fixtures should write to and
/// the daemon should read from. Exposed so future tests that need a
/// PathBuf can call this without re-implementing the layout.
#[allow(dead_code)]
pub fn temp_base(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nicotine-{}-{}-{}",
        label,
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ))
}
