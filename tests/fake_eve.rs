//! Integration tests: drive a built `Nicotine` binary against a
//! fixture of fake EVE clients (real X11 windows + real processes).
//!
//! Marked `#[ignore]` so `cargo test` doesn't break on machines or CI
//! runners with no X11 display. Run explicitly:
//!
//!     cargo test --test fake_eve -- --ignored --nocapture
//!
//! Or one at a time:
//!
//!     cargo test --test fake_eve -- --ignored cycle_forward
//!
//! Each daemon-driven test spawns its own `Nicotine daemon` against
//! an isolated socket + runtime dir + config dir, so multiple tests
//! can run in parallel and the user's real running daemon is never
//! disturbed.
//!
//! Cross-platform: the harness module (`common`) re-exports
//! `FakeEveHarness` from its `unix` or `windows` submodule depending
//! on the target. Test bodies assert via `nicotine list` /
//! `nicotine active` stdout, which is identical across platforms.

mod common;

use common::{
    activate_window_directly, nicotine_binary, skip_if_no_display, window_root_geometry,
    FakeEveHarness, TestDaemon,
};
use std::collections::HashSet;
use std::process::Command;

fn list_command_output(binary: &std::path::Path) -> String {
    let out = Command::new(binary)
        .arg("list")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn {}: {}", binary.display(), e));
    assert!(
        out.status.success(),
        "nicotine list exited {}: stderr={:?}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn names_from_list_output(stdout: &str) -> HashSet<String> {
    stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .map(|s| s.to_string())
        .collect()
}

/// Ordered list of EVE client names as `nicotine list` reports them.
/// Same enumeration the daemon will use on its NEXT tick to drive
/// cycle / switch. The order is platform-dependent: Linux X11
/// `_NET_CLIENT_LIST` is map-time-stable (doesn't shift when a
/// window gets focus); Windows `EnumWindows` returns top-of-Z-order
/// first, so activating a window moves it to the front of the list.
/// Tests that anchor + cycle must re-read the order AFTER any
/// activation so assertions match what the daemon actually sees.
fn daemon_list_order(daemon: &TestDaemon) -> Vec<String> {
    daemon
        .nicotine_stdout(&["list"])
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(String::from))
        .collect()
}

#[test]
#[ignore = "requires DISPLAY + a window manager that publishes _NET_CLIENT_LIST"]
fn list_enumerates_fake_eve_clients() {
    if skip_if_no_display() {
        return;
    }

    let _harness =
        FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up fake EVE harness");

    let stdout = list_command_output(&nicotine_binary());
    let names = names_from_list_output(&stdout);

    for expected in ["Alpha", "Beta", "Gamma"] {
        assert!(
            names.contains(expected),
            "{expected} missing from nicotine list output:\n{stdout}"
        );
    }
}

#[test]
#[ignore = "requires DISPLAY + a window manager that publishes _NET_CLIENT_LIST"]
fn list_rejects_eve_titled_non_eve_processes() {
    if skip_if_no_display() {
        return;
    }

    // One real fake EVE, plus a lookalike whose title also starts
    // with "EVE - " but whose backing process is /bin/sleep (comm =
    // "sleep", not "exefile.exe"). The real-world failure this
    // closes is preview windows getting created for browser tabs,
    // Discord channels, etc. that happen to use "EVE - …" titles.
    let mut harness = FakeEveHarness::new(&["RealPilot"]).expect("set up fake EVE harness");
    harness
        .add_lookalike("Lookalike Browser Tab")
        .expect("add lookalike window");

    let stdout = list_command_output(&nicotine_binary());
    let names = names_from_list_output(&stdout);

    assert!(
        names.contains("RealPilot"),
        "RealPilot missing from output:\n{stdout}"
    );
    assert!(
        !names.contains("Lookalike Browser Tab"),
        "Lookalike Browser Tab should have been filtered out:\n{stdout}"
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM (KWin / Mutter / Sway honor _NET_ACTIVE_WINDOW)"]
fn cycle_forward_advances_focus_to_next_eve_client() {
    if skip_if_no_display() {
        return;
    }

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    let initial_order = daemon_list_order(&daemon);
    assert_eq!(
        initial_order.len(),
        3,
        "daemon should see all 3 fake EVE clients, saw {initial_order:?}"
    );

    // Anchor focus on the MIDDLE client (index 1) so the test
    // exercises sync_with_active — the daemon has to read the
    // current active window via get_active_window() before advancing.
    let anchor_name = initial_order[1].clone();
    let anchor_id = harness
        .clients
        .iter()
        .find(|c| c.name == anchor_name)
        .map(|c| c.window)
        .expect("harness contains the anchor client");
    activate_window_directly(anchor_id).expect("set initial focus");
    daemon.wait_for_enum_tick();

    // Re-read order AFTER activation. The daemon iterates on its
    // next tick using the platform-native enumeration order; on
    // Windows that's Z-ordered, so the activated anchor is now at
    // the front of the list. The forward step lands on whatever's
    // next in that post-activation order.
    let post_order = daemon_list_order(&daemon);
    let anchor_pos = post_order
        .iter()
        .position(|n| n == &anchor_name)
        .unwrap_or_else(|| panic!("anchor {anchor_name:?} missing from post-order {post_order:?}"));
    let expected_next = post_order[(anchor_pos + 1) % post_order.len()].clone();

    daemon.send("forward").expect("send forward command");
    // The cycle is synchronous on the daemon side once the
    // ClientMessage is sent, but the WM applies focus a few frames
    // later. 300ms covers KWin + Mutter; Sway is faster.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, expected_next,
        "forward from {anchor_name:?} should advance to next in post-activation order {post_order:?}; got {active:?}"
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn cycle_backward_advances_focus_to_previous_eve_client() {
    if skip_if_no_display() {
        return;
    }

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    let initial_order = daemon_list_order(&daemon);
    assert_eq!(initial_order.len(), 3);

    let anchor_name = initial_order[1].clone();
    let anchor_id = harness
        .clients
        .iter()
        .find(|c| c.name == anchor_name)
        .map(|c| c.window)
        .expect("harness contains the middle client");
    activate_window_directly(anchor_id).expect("set initial focus");
    daemon.wait_for_enum_tick();

    let post_order = daemon_list_order(&daemon);
    let anchor_pos = post_order
        .iter()
        .position(|n| n == &anchor_name)
        .unwrap_or_else(|| panic!("anchor {anchor_name:?} missing from post-order {post_order:?}"));
    let len = post_order.len();
    let expected_prev = post_order[(anchor_pos + len - 1) % len].clone();

    daemon.send("backward").expect("send backward command");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, expected_prev,
        "backward from {anchor_name:?} should land on prev in post-activation order {post_order:?}; got {active:?}"
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn switch_to_n_focuses_the_nth_client() {
    if skip_if_no_display() {
        return;
    }

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    let initial_order = daemon_list_order(&daemon);
    assert_eq!(initial_order.len(), 3);

    // Anchor: focus the first listed client. The activation also
    // serves as a "bootstrap" — without ever activating something
    // from this test process, Windows refuses to let the daemon
    // change foreground later (no input-chain ownership). Linux is
    // tolerant either way; doing it on both keeps the test symmetric.
    let first_name = initial_order[0].clone();
    let first_id = harness
        .clients
        .iter()
        .find(|c| c.name == first_name)
        .map(|c| c.window)
        .unwrap();
    activate_window_directly(first_id).unwrap();
    daemon.wait_for_enum_tick();

    // Re-read after the activation; switch:N walks the daemon's
    // current enumeration order, which on Windows shifts after focus.
    let post_order = daemon_list_order(&daemon);
    let expected = post_order[1].clone();

    daemon.send("switch:2").expect("send switch:2");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, expected,
        "switch:2 (1-indexed) should land on the 2nd in post-activation order {post_order:?}; got {active:?}"
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn daemon_picks_up_newly_spawned_eve_window() {
    if skip_if_no_display() {
        return;
    }

    // Start with two; after the daemon is running and has done its
    // initial enumeration tick, add a third. The daemon's hot-reload
    // loop should pick it up within one tick. This catches
    // regressions in the periodic re-enumeration path — a common
    // place to break things while refactoring the daemon's
    // background thread.
    let mut harness = FakeEveHarness::new(&["Alpha", "Beta"]).expect("initial harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    let before = daemon.nicotine_stdout(&["list"]);
    let before_names = names_from_list_output(&before);
    assert!(
        before_names.contains("Alpha") && before_names.contains("Beta"),
        "daemon should see the initial fakes:\n{before}"
    );
    assert!(
        !before_names.contains("Gamma"),
        "Gamma shouldn't exist yet:\n{before}"
    );

    harness
        .add_lookalike("ignored")
        .expect("add a non-EVE window to confirm the filter still works during hot reload");
    // Use the same fixture path FakeEveHarness uses internally to add
    // a real-EVE client. There's no public API for this on the harness
    // yet, so we do it inline via a second harness scope... but that
    // would tear down on drop. Cheap path: rebuild the harness with 3
    // and let the daemon pick up the diff.
    drop(harness);
    let _harness2 = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("expanded harness");
    daemon.wait_for_enum_tick();
    daemon.wait_for_enum_tick();

    let after = daemon.nicotine_stdout(&["list"]);
    let after_names = names_from_list_output(&after);
    assert!(
        after_names.contains("Gamma"),
        "daemon should have re-enumerated and picked up Gamma:\n{after}"
    );
}

#[test]
#[ignore = "requires DISPLAY"]
fn list_handles_title_edge_cases() {
    if skip_if_no_display() {
        return;
    }

    // Real EVE titles always start with "EVE - " but the rest can
    // contain spaces (full character names like "John Q Pilot"),
    // unicode (Cyrillic / CJK player names), and trailing
    // whitespace (the client occasionally appends an extra space
    // depending on session state). The title-trim logic in
    // x11_manager / wayland_backends is small but easy to break
    // while refactoring; this test pins the behavior.
    let names = &[
        "Pilot Name With Spaces",
        "Игрок",       // Cyrillic
        "ﾊﾟｲﾛｯﾄ",       // half-width katakana
        "Trailing   ", // intentional trailing whitespace
    ];
    let _harness = FakeEveHarness::new(names).expect("set up harness");

    let stdout = list_command_output(&nicotine_binary());
    let observed = names_from_list_output(&stdout);

    for &expected in names {
        // Both x11_manager and the wmctrl-based Wayland backend
        // strip the "EVE - " prefix but preserve the rest verbatim.
        // For the trailing-whitespace case the wmctrl backend can
        // tokenize differently on multi-space titles; accept a
        // trim-trailing match as well as exact.
        let trimmed = expected.trim_end();
        let exact = observed.contains(expected);
        let trimmed_match = observed.iter().any(|o| o.trim_end() == trimmed);
        assert!(
            exact || trimmed_match,
            "{expected:?} (or trimmed: {trimmed:?}) missing from list output:\n{stdout}"
        );
    }
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn stack_command_centers_all_eve_windows_at_same_geometry() {
    if skip_if_no_display() {
        return;
    }

    // `nicotine stack` should move every EVE client to the same x/y
    // and resize them to a uniform width/height (configured via
    // display_width / eve_width / panel_height). Tests that the
    // stack_windows trait impl actually issues move/resize for each
    // window and that they all land on the same root-relative spot.
    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    daemon.send("stack").expect("send stack command");
    // wmctrl-based stack does N synchronous shell-outs; give the WM
    // a moment to settle the resulting ConfigureNotifies.
    std::thread::sleep(std::time::Duration::from_millis(400));

    let mut geoms = Vec::new();
    for client in &harness.clients {
        let g = window_root_geometry(client.window)
            .unwrap_or_else(|e| panic!("read geometry for {}: {e}", client.name));
        geoms.push((client.name.clone(), g));
    }

    // All three should share the same (x, y, w, h) — within a small
    // tolerance because some WMs adjust for frame extents during
    // resize. 5px slack is more than enough to absorb that without
    // accepting a totally broken stack.
    let (_, (ref_x, ref_y, ref_w, ref_h)) = geoms[0].clone();
    for (name, (x, y, w, h)) in &geoms[1..] {
        let dx = (x - ref_x).abs();
        let dy = (y - ref_y).abs();
        let dw = (*w as i32 - ref_w as i32).abs();
        let dh = (*h as i32 - ref_h as i32).abs();
        assert!(
            dx <= 5 && dy <= 5 && dw <= 5 && dh <= 5,
            "stack should land all windows at the same geometry; \
             {name} at ({x},{y},{w},{h}) vs {} at ({ref_x},{ref_y},{ref_w},{ref_h})",
            geoms[0].0
        );
    }
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn cycle_forward_wraps_around_from_last_to_first() {
    if skip_if_no_display() {
        return;
    }

    // Anchor on the LAST listed client. After activation the daemon
    // sees that anchor at the front of its order on platforms with
    // Z-order-based enumeration (Windows); the forward step lands on
    // whatever's next in that post-activation order. The "wrap"
    // framing matches Linux behavior where the order is stable.
    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    let initial_order = daemon_list_order(&daemon);
    assert_eq!(initial_order.len(), 3);

    let anchor_name = initial_order.last().unwrap().clone();
    let anchor_id = harness
        .clients
        .iter()
        .find(|c| c.name == anchor_name)
        .map(|c| c.window)
        .unwrap();
    activate_window_directly(anchor_id).unwrap();
    daemon.wait_for_enum_tick();

    let post_order = daemon_list_order(&daemon);
    let anchor_pos = post_order
        .iter()
        .position(|n| n == &anchor_name)
        .unwrap_or_else(|| panic!("anchor {anchor_name:?} missing from {post_order:?}"));
    let expected_next = post_order[(anchor_pos + 1) % post_order.len()].clone();

    daemon.send("forward").expect("send forward");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, expected_next,
        "forward from {anchor_name:?} should advance in post-activation order {post_order:?}; got {active:?}"
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn cycle_backward_wraps_around_from_first_to_last() {
    if skip_if_no_display() {
        return;
    }

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    let initial_order = daemon_list_order(&daemon);
    assert_eq!(initial_order.len(), 3);

    let anchor_name = initial_order[0].clone();
    let anchor_id = harness
        .clients
        .iter()
        .find(|c| c.name == anchor_name)
        .map(|c| c.window)
        .unwrap();
    activate_window_directly(anchor_id).unwrap();
    daemon.wait_for_enum_tick();

    let post_order = daemon_list_order(&daemon);
    let anchor_pos = post_order
        .iter()
        .position(|n| n == &anchor_name)
        .unwrap_or_else(|| panic!("anchor {anchor_name:?} missing from {post_order:?}"));
    let len = post_order.len();
    let expected_prev = post_order[(anchor_pos + len - 1) % len].clone();

    daemon.send("backward").expect("send backward");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, expected_prev,
        "backward from {anchor_name:?} should wrap in post-activation order {post_order:?}; got {active:?}"
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn switch_n_out_of_range_is_a_no_op_not_a_crash() {
    if skip_if_no_display() {
        return;
    }

    // switch:99 against a 3-client setup should NOT panic the daemon
    // and should NOT change focus. The same goes for switch:0
    // (switch is 1-indexed on the wire; zero is invalid). The test
    // is mostly about daemon survival — if it crashed, the next IPC
    // command would fail to connect.
    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let daemon = TestDaemon::spawn().expect("daemon");

    let order_stdout = daemon.nicotine_stdout(&["list"]);
    let ordered_names: Vec<String> = order_stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(String::from))
        .collect();
    let anchor_id = harness
        .clients
        .iter()
        .find(|c| c.name == ordered_names[0])
        .map(|c| c.window)
        .unwrap();
    activate_window_directly(anchor_id).unwrap();
    daemon.wait_for_enum_tick();

    let before_active = daemon.active_client_name();

    daemon.send("switch:99").expect("send switch:99");
    std::thread::sleep(std::time::Duration::from_millis(200));
    daemon.send("switch:0").expect("send switch:0");
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Daemon still responding (no crash).
    let after_list = daemon.nicotine_stdout(&["list"]);
    let after_names = names_from_list_output(&after_list);
    assert_eq!(
        after_names.len(),
        3,
        "daemon should still enumerate all 3 fakes after invalid switch commands"
    );

    // Focus unchanged.
    let after_active = daemon.active_client_name();
    assert_eq!(
        before_active, after_active,
        "out-of-range switch should leave focus alone; before={:?} after={:?}",
        before_active, after_active
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn switch_uses_character_order_from_config_after_hot_reload() {
    if skip_if_no_display() {
        return;
    }

    // Pin a non-trivial character_order via config hot-reload. The
    // order intentionally inverts the natural _NET_CLIENT_LIST order
    // (which goes Alpha, Beta, Gamma). After the daemon picks up
    // the new order, switch:1 should land on Gamma, not Alpha —
    // proving (a) hot-reload of `characters` works and (b) switch_to
    // uses the configured order, not the WM enumeration order.
    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let daemon = TestDaemon::spawn().expect("daemon");

    // Bootstrap the input chain by activating any fake first. Without
    // this, on Windows the daemon's later SetForegroundWindow call
    // (inside switch_to → activate_window) is silently denied because
    // the test process never relinquished foreground to a window the
    // daemon knows about. Linux doesn't need the bootstrap but it's
    // harmless there.
    let bootstrap_id = harness.clients[0].window;
    activate_window_directly(bootstrap_id).expect("bootstrap activate");
    daemon.wait_for_enum_tick();

    let custom = "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = false
characters = [\"Gamma\", \"Beta\", \"Alpha\"]
";
    daemon.rewrite_config(custom).expect("rewrite config");

    daemon.send("switch:1").expect("send switch:1");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, "Gamma",
        "with characters=[Gamma,Beta,Alpha] in config, switch:1 (1-indexed) should land on Gamma; got {:?}",
        active
    );
}

#[test]
#[ignore = "requires DISPLAY + a cooperative WM"]
fn single_eve_cycle_is_a_no_op() {
    if skip_if_no_display() {
        return;
    }

    // With only one EVE client, both forward and backward cycle
    // operations must NOT crash the daemon and should leave the
    // single client focused (modulo: cycle index advances internally
    // and wraps back to the same client). Guards against
    // division-by-zero or modulo-by-zero in CycleState::cycle_*.
    let harness = FakeEveHarness::new(&["Solo"]).expect("single-fake harness");
    let daemon = TestDaemon::spawn().expect("daemon");

    let only_id = harness.clients[0].window;
    activate_window_directly(only_id).unwrap();
    daemon.wait_for_enum_tick();

    daemon.send("forward").expect("forward survives");
    std::thread::sleep(std::time::Duration::from_millis(200));
    daemon.send("backward").expect("backward survives");
    std::thread::sleep(std::time::Duration::from_millis(200));

    // Daemon still responsive.
    let after_list = daemon.nicotine_stdout(&["list"]);
    let after_names = names_from_list_output(&after_list);
    assert!(
        after_names.contains("Solo"),
        "daemon should still see Solo after single-EVE cycle attempts:\n{after_list}"
    );

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, "Solo",
        "single-EVE cycle should leave Solo focused; got {:?}",
        active
    );
}
