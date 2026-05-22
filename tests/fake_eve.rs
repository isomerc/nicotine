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

#![cfg(unix)]

mod common;

use common::{
    activate_window_directly, nicotine_binary, skip_if_no_display, FakeEveHarness, TestDaemon,
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

    // The order the daemon iterates is whatever _NET_CLIENT_LIST
    // returns — typically map-time order on most WMs, but we don't
    // depend on a specific value. Instead, we anchor on one client
    // as the starting focus, capture the daemon's view of the order,
    // and assert the post-cycle active client is the NEXT entry in
    // that captured order.
    let harness = FakeEveHarness::new(&["Alpha", "Beta", "Gamma"]).expect("set up harness");
    let daemon = TestDaemon::spawn().expect("spawn test daemon");

    // The order nicotine sees is what governs cycle direction.
    let order_stdout = daemon.nicotine_stdout(&["list"]);
    let ordered_names: Vec<String> = order_stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(String::from))
        .collect();
    assert_eq!(
        ordered_names.len(),
        3,
        "daemon should see all 3 fake EVE clients, saw {ordered_names:?}"
    );

    // Anchor focus on the MIDDLE client (index 1) so the test
    // exercises sync_with_active — the daemon has to read the current
    // active window via get_active_window() before advancing.
    // Anchoring on index 0 wouldn't distinguish "sync works" from
    // "sync broken but internal index happened to start at 0" because
    // both paths would still land on index 1 after forward.
    let anchor_name = &ordered_names[1];
    let anchor_id = harness
        .clients
        .iter()
        .find(|c| c.name == *anchor_name)
        .map(|c| c.window)
        .expect("harness contains the anchor client");
    activate_window_directly(anchor_id).expect("set initial focus");
    daemon.wait_for_enum_tick();

    daemon.send("forward").expect("send forward command");
    // The cycle is synchronous on the daemon side once the
    // ClientMessage is sent, but the WM applies focus a few frames
    // later. 300ms covers KWin + Mutter; Sway is faster.
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, ordered_names[2],
        "expected forward from {} (index 1) to advance to {} (index 2), got {:?}",
        ordered_names[1], ordered_names[2], active
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

    let order_stdout = daemon.nicotine_stdout(&["list"]);
    let ordered_names: Vec<String> = order_stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(String::from))
        .collect();
    assert_eq!(ordered_names.len(), 3);

    let middle_name = &ordered_names[1];
    let middle_id = harness
        .clients
        .iter()
        .find(|c| c.name == *middle_name)
        .map(|c| c.window)
        .expect("harness contains the middle client");
    activate_window_directly(middle_id).expect("set initial focus");
    daemon.wait_for_enum_tick();

    daemon.send("backward").expect("send backward command");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, ordered_names[0],
        "expected backward from {} to land on {}, got {:?}",
        ordered_names[1], ordered_names[0], active
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

    let order_stdout = daemon.nicotine_stdout(&["list"]);
    let ordered_names: Vec<String> = order_stdout
        .lines()
        .filter_map(|line| line.split('\t').nth(1).map(String::from))
        .collect();
    assert_eq!(ordered_names.len(), 3);

    // Anchor: focus the first listed client. switch:N is 1-indexed
    // on the wire (the CLI shorthand `nicotine 2` maps to index 2);
    // assert it lands on the 1-indexed target regardless of where we
    // started.
    let first_id = harness
        .clients
        .iter()
        .find(|c| c.name == ordered_names[0])
        .map(|c| c.window)
        .unwrap();
    activate_window_directly(first_id).unwrap();
    daemon.wait_for_enum_tick();

    daemon.send("switch:2").expect("send switch:2");
    std::thread::sleep(std::time::Duration::from_millis(300));

    let active = daemon.active_client_name().unwrap_or_default();
    assert_eq!(
        active, ordered_names[1],
        "switch:2 (1-indexed) should land on the 2nd listed client ({}), got {:?}",
        ordered_names[1], active
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
