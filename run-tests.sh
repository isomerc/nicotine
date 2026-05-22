#!/usr/bin/env bash
# Nicotine - Test runner
#
# Runs unit tests + the integration tests that exercise the daemon
# end-to-end. The integration tests are marked `#[ignore]` in source
# so a plain `cargo test` skips them on headless CI; this script
# opts them in explicitly.
#
# Usage:
#   ./run-tests.sh                  # everything (unit + integration + wayland)
#   ./run-tests.sh unit             # unit tests only (no display needed)
#   ./run-tests.sh integration      # X11 fake-EVE integration tests
#   ./run-tests.sh wayland          # Wayland xdg-activation test
#   ./run-tests.sh <pattern>        # forward a name pattern to cargo test
#                                   # e.g. ./run-tests.sh cycle_forward
#
# What each category covers:
#
#   unit          — Cycle planner, hotkey planner, x-button classify,
#                   thread-attach gating, EVE process-name filter,
#                   keyboard listener resolve, config parsing, etc.
#                   Pure functions; no environment dependencies.
#
#   integration   — Full daemon driven end-to-end against real OS
#                   windows. Linux: 17 tests including cycle, switch,
#                   stack, hot-reload, mouse / keyboard hotkeys via
#                   uinput. Requires $DISPLAY (any X server, including
#                   XWayland on a Wayland desktop).
#
#   wayland       — XdgActivation protocol round-trip. Requires
#                   WAYLAND_DISPLAY pointing at a compositor that
#                   advertises xdg_activation_v1 (KWin, sway, weston,
#                   etc). On a real desktop this is usually set.
#
# The integration tests are sequential (--test-threads=1) because
# they each create real OS windows + spawn a daemon subprocess;
# parallel runs can race on focus events.

set -e

cd "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

MODE="${1:-all}"

run_unit() {
    echo "=== Unit tests ==="
    cargo test --quiet
}

run_integration() {
    echo
    echo "=== Integration tests (fake-EVE harness) ==="
    # The harness creates real OS windows + spawns the daemon; on
    # Linux it needs $DISPLAY pointing at an X server (the daemon's
    # KWinManager / X11Manager require it). On Windows the runner
    # has a graphical session by default.
    case "$(uname -s)" in
        Linux|*BSD)
            if [ -z "${DISPLAY:-}" ]; then
                echo "WARN: \$DISPLAY is not set; integration tests will skip themselves."
                echo "      Run from a desktop terminal (KDE / GNOME / Sway / etc.)."
            fi
            ;;
    esac
    cargo test --test fake_eve -- --ignored --test-threads=1 --nocapture
}

run_wayland() {
    echo
    echo "=== Wayland xdg-activation test ==="
    case "$(uname -s)" in
        Linux|*BSD)
            if [ -z "${WAYLAND_DISPLAY:-}" ]; then
                echo "WARN: \$WAYLAND_DISPLAY is not set; the test will skip itself."
                echo "      Run from a Wayland session (KDE Plasma Wayland, sway, etc.)"
                echo "      or start weston headless first."
            fi
            ;;
        *)
            echo "(skipped — Wayland tests only run on Linux/BSD)"
            return 0
            ;;
    esac
    # The xdg_activation test lives in the bin's #[cfg(test)] module,
    # so we route through --bin Nicotine specifically; the integration
    # tests file (--test fake_eve) doesn't include it.
    cargo test --bin Nicotine xdg_activation -- --ignored --nocapture
}

case "$MODE" in
    all)
        run_unit
        run_integration
        run_wayland
        ;;
    unit)
        run_unit
        ;;
    integration|integ|fake_eve)
        run_integration
        ;;
    wayland|xdg|xdg_activation)
        run_wayland
        ;;
    -h|--help|help)
        sed -n '2,36p' "$0"
        exit 0
        ;;
    *)
        # Treat unknown args as a name pattern. Run unit tests
        # matching the pattern, then integration tests matching it
        # (with --ignored so #[ignore]'d ones are eligible).
        echo "=== Unit tests matching '$MODE' ==="
        cargo test --quiet "$MODE" || true
        echo
        echo "=== Integration tests matching '$MODE' ==="
        cargo test --test fake_eve "$MODE" -- --ignored --test-threads=1 --nocapture
        ;;
esac
