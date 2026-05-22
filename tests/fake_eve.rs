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

#[cfg(unix)]
use common::VirtualInput;
use common::{
    activate_window_directly, nicotine_binary, skip_if_no_display, window_root_geometry,
    FakeEveHarness, TestDaemon,
};
#[cfg(unix)]
use evdev::Key;
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

// ===== Phase 3: input-simulation tests =====================================
//
// These tests inject real keyboard / mouse events through a virtual
// device (uinput on Linux, SendInput on Windows — Windows half is a
// follow-up). They exercise the daemon's keyboard_listener and
// mouse_listener layers end-to-end, which the IPC-driven tests above
// skip entirely. The class of regression they catch is the F16+ binding
// bug + multi-device mouse bug fixed in earlier sessions.

#[cfg(unix)]
#[test]
#[ignore = "requires DISPLAY + /dev/uinput rw access (NixOS user ACL or `chmod 666 /dev/uinput` on CI)"]
fn per_character_hotkey_activates_target_window() {
    if skip_if_no_display() {
        return;
    }

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");

    // Build the virtual keyboard BEFORE the daemon so the device
    // node exists when keyboard_listener tries to open it on its
    // first tick. F16 is the key under test (KEY_F16 = 186).
    let mut vkbd = VirtualInput::keyboard(&[Key::KEY_F16, Key::KEY_LEFTSHIFT])
        .expect("create virtual keyboard");
    let kbd_path = vkbd.devnode().to_string_lossy().into_owned();

    // Bootstrap focus on Alpha so we have a known starting point
    // and the daemon's foreground-tracking code path is exercised.
    activate_window_directly(harness.clients[0].window).expect("bootstrap focus");

    let daemon = TestDaemon::spawn().expect("daemon");

    // Bind F16 → "Beta" via hot-reloaded config. Also explicitly
    // point keyboard_device_path at our virtual device so the
    // listener doesn't pick a real keyboard on a dev machine.
    let config = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
keyboard_device_path = \"{kbd_path}\"
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Beta]
vk = 186
"
    );
    daemon.rewrite_config(&config).expect("rewrite config");

    // Give the listener a beat to (re-)attach to the virtual device
    // after picking up the new path from hot-reload. The listener's
    // poll loop opens / re-opens the device on path change.
    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap(Key::KEY_F16).expect("tap F16");
    // Daemon's listener -> hotkey planner -> activate is synchronous
    // once the key event arrives, but the WM applies focus a few
    // frames later. 400ms is comfortably above KWin / Mutter latency.
    std::thread::sleep(std::time::Duration::from_millis(400));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active,
        "Beta",
        "F16 bound to Beta should activate Beta when pressed; got {active:?}{}",
        daemon.diagnostic_log()
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires DISPLAY + /dev/uinput rw access"]
fn modifier_combo_hotkey_activates_target_window() {
    if skip_if_no_display() {
        return;
    }

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let mut vkbd = VirtualInput::keyboard(&[Key::KEY_F1, Key::KEY_LEFTSHIFT]).expect("vkbd");
    let kbd_path = vkbd.devnode().to_string_lossy().into_owned();

    // Bootstrap on Gamma so the test can prove Shift+F1 actually
    // moved focus (not just happened to keep Alpha selected).
    activate_window_directly(harness.clients[2].window).expect("bootstrap focus");
    let daemon = TestDaemon::spawn().expect("daemon");

    // Bind Shift+F1 → "Alpha". KEY_F1 = 59, KEY_LEFTSHIFT = 42 on
    // Linux evdev. The character_hotkeys entry stores `modifier`
    // alongside `vk` so the planner's modifier-aware resolution path
    // is exercised.
    let config = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
keyboard_device_path = \"{kbd_path}\"
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Alpha]
vk = 59
modifier = 42
"
    );
    daemon.rewrite_config(&config).expect("rewrite config");

    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap_with_modifier(Key::KEY_LEFTSHIFT, Key::KEY_F1)
        .expect("Shift+F1");
    std::thread::sleep(std::time::Duration::from_millis(400));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active,
        "Alpha",
        "Shift+F1 bound to Alpha should activate Alpha; got {active:?}{}",
        daemon.diagnostic_log()
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires DISPLAY + /dev/uinput rw access"]
fn mouse_side_button_cycles_forward() {
    if skip_if_no_display() {
        return;
    }

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");

    // BTN_SIDE = 275, BTN_EXTRA = 276 — the daemon's default mouse
    // forward button is 276 (BTN_EXTRA on evdev). We register both
    // codes on the virtual mouse so we can also click backward in
    // future tests without rebuilding the device.
    let mut vmouse = VirtualInput::mouse(&[Key::BTN_SIDE, Key::BTN_EXTRA]).expect("vmouse");
    let mouse_path = vmouse.devnode().to_string_lossy().into_owned();

    activate_window_directly(harness.clients[0].window).expect("bootstrap focus");
    let daemon = TestDaemon::spawn().expect("daemon");

    let config = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = true
enable_keyboard_buttons = false
mouse_device_path = \"{mouse_path}\"
"
    );
    daemon.rewrite_config(&config).expect("rewrite config");
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Anchor on whatever the daemon sees first post-bootstrap so the
    // expected-next computation is platform-agnostic. The mouse
    // listener fires forward → daemon cycles → activate; same code
    // path as IPC "forward".
    let initial = common_list_names(&daemon);
    let anchor = &initial[0];
    let post_activate_id = harness
        .clients
        .iter()
        .find(|c| c.name == *anchor)
        .map(|c| c.window)
        .unwrap();
    activate_window_directly(post_activate_id).unwrap();
    daemon.wait_for_enum_tick();

    let post_order = common_list_names(&daemon);
    let anchor_pos = post_order.iter().position(|n| n == anchor).unwrap();
    let expected_next = post_order[(anchor_pos + 1) % post_order.len()].clone();

    vmouse.tap(Key::BTN_EXTRA).expect("click BTN_EXTRA");
    std::thread::sleep(std::time::Duration::from_millis(400));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active,
        expected_next,
        "BTN_EXTRA (forward) should advance cycle in post-activation order {post_order:?}; got {active:?}{}",
        daemon.diagnostic_log()
    );
}

#[cfg(unix)]
#[test]
#[ignore = "requires DISPLAY + /dev/uinput rw access"]
fn hotkey_rebind_via_hot_reload_takes_effect() {
    if skip_if_no_display() {
        return;
    }

    // Start with F16 → Beta. Hit F16, assert Beta activated. Then
    // rewrite config so F16 → Gamma. Hit F16 again, assert Gamma
    // activated. Catches regressions in the daemon's keyboard
    // hot-reload path (the shared KeyboardConfig mutex listener
    // threads read on each poll tick).
    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let mut vkbd = VirtualInput::keyboard(&[Key::KEY_F16]).expect("vkbd");
    let kbd_path = vkbd.devnode().to_string_lossy().into_owned();
    activate_window_directly(harness.clients[0].window).expect("bootstrap");
    let daemon = TestDaemon::spawn().expect("daemon");

    let config_beta = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
keyboard_device_path = \"{kbd_path}\"
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Beta]
vk = 186
"
    );
    daemon.rewrite_config(&config_beta).expect("config -> Beta");
    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap(Key::KEY_F16).expect("first F16");
    std::thread::sleep(std::time::Duration::from_millis(400));
    let after_first = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        after_first,
        "Beta",
        "first F16 should activate Beta under the initial binding; got {after_first:?}{}",
        daemon.diagnostic_log()
    );

    let config_gamma = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
keyboard_device_path = \"{kbd_path}\"
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Gamma]
vk = 186
"
    );
    daemon
        .rewrite_config(&config_gamma)
        .expect("config -> Gamma");
    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap(Key::KEY_F16).expect("second F16");
    std::thread::sleep(std::time::Duration::from_millis(400));
    let after_second = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        after_second,
        "Gamma",
        "second F16 should activate Gamma after hot-reloading the binding; got {after_second:?}{}",
        daemon.diagnostic_log()
    );
}

/// Tiny helper used by input-sim tests. Same shape as
/// `daemon_list_order` but accessible from a `cfg(unix)` block; the
/// alias keeps the cfg gating local to where it's used.
#[cfg(unix)]
fn common_list_names(daemon: &TestDaemon) -> Vec<String> {
    daemon_list_order(daemon)
}

// ===== Phase 3 (Windows): input-simulation tests ============================
//
// Mirror of the Linux uinput tests above, but driving the daemon
// through `SendInput` instead. Windows hotkeys go through
// `RegisterHotKey` (system-wide) which `SendInput` triggers exactly
// like a real keypress; the daemon's mouse-side-button cycling goes
// through a `WH_MOUSE_LL` hook which also sees injected events.
//
// VK_ constants are 1:1 with the Win32 virtual-key codes used in
// `windows::Win32::UI::Input::KeyboardAndMouse::VK_*`. We define
// them here as plain `u16` so the test body can pass them to both
// `VirtualInput::tap` (raw VK) and the daemon's config TOML (which
// stores `vk = <u16>`).

#[cfg(windows)]
const VK_F1: u16 = 0x70;
#[cfg(windows)]
const VK_F16: u16 = 0x7F;
#[cfg(windows)]
const VK_LSHIFT: u16 = 0xA0;

/// XBUTTON1 = back, XBUTTON2 = forward — same encoding the daemon's
/// low-level mouse hook reads from `MSLLHOOKSTRUCT::mouseData`.
#[cfg(windows)]
const XBUTTON2_FORWARD: u16 = 2;

#[cfg(windows)]
#[test]
#[ignore = "needs an interactive Windows session (windows-latest runner has one)"]
fn per_character_hotkey_activates_target_window_windows() {
    use common::VirtualInput;

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let mut vkbd = VirtualInput::keyboard(&[VK_F16]).expect("vkbd");

    // Bootstrap focus so the daemon's force_activate has a known
    // foreground to do its AttachThreadInput dance against.
    activate_window_directly(harness.clients[0].window).expect("bootstrap");
    let daemon = TestDaemon::spawn().expect("daemon");

    let config = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Beta]
vk = {VK_F16}
"
    );
    daemon.rewrite_config(&config).expect("rewrite config");
    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap(VK_F16).expect("tap VK_F16");
    std::thread::sleep(std::time::Duration::from_millis(400));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active,
        "Beta",
        "VK_F16 bound to Beta should activate Beta; got {active:?}{}",
        daemon.diagnostic_log()
    );
}

#[cfg(windows)]
#[test]
#[ignore = "needs an interactive Windows session"]
fn modifier_combo_hotkey_activates_target_window_windows() {
    use common::VirtualInput;

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let mut vkbd = VirtualInput::keyboard(&[VK_F1, VK_LSHIFT]).expect("vkbd");

    activate_window_directly(harness.clients[2].window).expect("bootstrap on Gamma");
    let daemon = TestDaemon::spawn().expect("daemon");

    let config = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Alpha]
vk = {VK_F1}
modifier = {VK_LSHIFT}
"
    );
    daemon.rewrite_config(&config).expect("rewrite config");
    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap_with_modifier(VK_LSHIFT, VK_F1).expect("Shift+F1");
    std::thread::sleep(std::time::Duration::from_millis(400));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active,
        "Alpha",
        "Shift+F1 bound to Alpha should activate Alpha; got {active:?}{}",
        daemon.diagnostic_log()
    );
}

#[cfg(windows)]
#[test]
#[ignore = "needs an interactive Windows session"]
fn mouse_side_button_cycles_forward_windows() {
    use common::VirtualInput;

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let mut vmouse = VirtualInput::mouse(&[XBUTTON2_FORWARD]).expect("vmouse");

    activate_window_directly(harness.clients[0].window).expect("bootstrap");
    let daemon = TestDaemon::spawn().expect("daemon");

    let config = "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = true
enable_keyboard_buttons = false
forward_button = 2
backward_button = 1
"
    .to_string();
    daemon.rewrite_config(&config).expect("rewrite config");
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Re-anchor so EnumWindows post-activation order is deterministic.
    let initial = daemon_list_order(&daemon);
    let anchor_name = initial[0].clone();
    let anchor_id = harness
        .clients
        .iter()
        .find(|c| c.name == anchor_name)
        .map(|c| c.window)
        .unwrap();
    activate_window_directly(anchor_id).unwrap();
    daemon.wait_for_enum_tick();
    let post_order = daemon_list_order(&daemon);
    let anchor_pos = post_order.iter().position(|n| n == &anchor_name).unwrap();
    let expected_next = post_order[(anchor_pos + 1) % post_order.len()].clone();

    // Find the expected target HWND so we can assert on the
    // daemon's force_activate intent rather than on the OS-level
    // focus change. Windows CI runners deny cross-process
    // SetForegroundWindow even with the canonical workarounds
    // (AttachThreadInput, AllowSetForegroundWindow) because the
    // session is non-interactive enough that the OS won't honor
    // synthetic-input-triggered foreground changes. The daemon's
    // *intent* is what we actually want to test — that the mouse
    // hook → classify → PostThreadMessage → listener → cycle →
    // force_activate pipeline produced the right target. The final
    // OS-applied focus is environmental and verified manually +
    // implicitly by the IPC cycle tests on Windows (which work
    // because their daemon thread has its own input recency).
    let expected_target_hwnd = harness
        .clients
        .iter()
        .find(|c| c.name == expected_next)
        .map(|c| c.window)
        .expect("expected_next must be a known harness client");

    common::grant_set_foreground_to_all();
    vmouse.tap_xbutton(XBUTTON2_FORWARD).expect("XBUTTON2");
    std::thread::sleep(std::time::Duration::from_millis(400));

    // Parse the daemon stderr (NICOTINE_DEBUG_INPUT is on) for the
    // last `force_activate: target=0x<hex>` line. The daemon emits
    // one per activation attempt; if cycle dispatch worked, this
    // line records the HWND the daemon TRIED to focus. Compare to
    // expected_next's HWND.
    let log = daemon.stderr_log_contents_pub();
    let last_target = log
        .lines()
        .rev()
        .find_map(|line| {
            line.find("force_activate: target=0x").and_then(|i| {
                let rest = &line[i + "force_activate: target=0x".len()..];
                let hex_end = rest
                    .find(|c: char| !c.is_ascii_hexdigit())
                    .unwrap_or(rest.len());
                u64::from_str_radix(&rest[..hex_end], 16).ok()
            })
        })
        .unwrap_or_else(|| {
            panic!(
                "no `force_activate: target=` line in daemon stderr — the cycle dispatch never ran:{}",
                daemon.diagnostic_log()
            )
        });

    assert_eq!(
        last_target as u32,
        expected_target_hwnd,
        "XBUTTON2 (forward) should have advanced cycle to {expected_next} (HWND {:#x}) in post-activation order {post_order:?}; daemon's force_activate targeted {last_target:#x}{}",
        expected_target_hwnd,
        daemon.diagnostic_log()
    );
}

#[cfg(windows)]
#[test]
#[ignore = "needs an interactive Windows session"]
fn hotkey_rebind_via_hot_reload_takes_effect_windows() {
    use common::VirtualInput;

    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("harness");
    let mut vkbd = VirtualInput::keyboard(&[VK_F16]).expect("vkbd");
    activate_window_directly(harness.clients[0].window).expect("bootstrap");
    let daemon = TestDaemon::spawn().expect("daemon");

    let config_beta = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Beta]
vk = {VK_F16}
"
    );
    daemon.rewrite_config(&config_beta).expect("config -> Beta");
    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap(VK_F16).expect("first F16");
    std::thread::sleep(std::time::Duration::from_millis(400));
    let after_first = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        after_first,
        "Beta",
        "first F16 should activate Beta; got {after_first:?}{}",
        daemon.diagnostic_log()
    );

    let config_gamma = format!(
        "\
display_width = 1920
display_height = 1080
panel_height = 100
eve_width = 1024
eve_height = 768
show_previews = false
enable_mouse_buttons = false
enable_keyboard_buttons = true
characters = [\"Alpha\", \"Beta\", \"Gamma\"]
[character_hotkeys.Gamma]
vk = {VK_F16}
"
    );
    daemon
        .rewrite_config(&config_gamma)
        .expect("config -> Gamma");
    std::thread::sleep(std::time::Duration::from_millis(400));

    vkbd.tap(VK_F16).expect("second F16");
    std::thread::sleep(std::time::Duration::from_millis(400));
    let after_second = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        after_second,
        "Gamma",
        "second F16 should activate Gamma after rebind; got {after_second:?}{}",
        daemon.diagnostic_log()
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
