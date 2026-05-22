#!/usr/bin/env bash
# Nicotine - Test runner
#
# Runs both the unit tests and the X11-driven integration tests in
# tests/fake_eve.rs. The integration tests spawn real X11 windows and
# a real test daemon, so they require a running graphical session
# ($DISPLAY set). They're marked `#[ignore]` in the source so a plain
# `cargo test` doesn't break on headless CI — this script opts them
# in explicitly.
#
# Usage:
#   ./run-tests.sh                  # all tests (unit + integration)
#   ./run-tests.sh unit             # unit tests only (no display needed)
#   ./run-tests.sh integration      # X11 integration tests only
#   ./run-tests.sh <pattern>        # forward a name pattern to cargo test
#                                   # e.g. ./run-tests.sh cycle_forward
#
# Integration tests:
#   - list_enumerates_fake_eve_clients
#   - list_rejects_eve_titled_non_eve_processes
#   - list_handles_title_edge_cases
#   - cycle_forward_advances_focus_to_next_eve_client
#   - cycle_backward_advances_focus_to_previous_eve_client
#   - switch_to_n_focuses_the_nth_client
#   - daemon_picks_up_newly_spawned_eve_window
#
# The integration tests are sequential (--test-threads=1) because
# they each create real X11 windows + spawn a daemon subprocess;
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

case "$MODE" in
    all)
        run_unit
        run_integration
        ;;
    unit)
        run_unit
        ;;
    integration|integ|fake_eve)
        run_integration
        ;;
    -h|--help|help)
        sed -n '2,30p' "$0"
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
