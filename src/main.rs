// Release builds on Windows run under the GUI subsystem so Windows
// doesn't allocate a console (or launch Windows Terminal) when the
// user double-clicks nicotine.exe. Debug builds stay on the default
// CONSOLE subsystem so `cargo run` / `cargo xwin run` during dev still
// prints stdout. The tradeoff: release-binary runs from PowerShell no
// longer show println! output in the shell. That's OK — the app is
// GUI-first; the daemon subcommand still works, it just runs headless.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod config;
#[cfg(windows)]
mod config_panel;
mod cycle_state;
mod daemon;
mod ipc;
mod lock;
mod paths;
mod telemetry;
mod window_manager;

mod version_check;

#[cfg(unix)]
mod keyboard_listener;
#[cfg(unix)]
mod mouse_listener;
#[cfg(unix)]
mod overlay;
#[cfg(unix)]
mod wayland_backends;
#[cfg(unix)]
mod x11_manager;

#[cfg(windows)]
mod preview_windows;
#[cfg(windows)]
mod windows_input;
#[cfg(windows)]
mod windows_manager;

use anyhow::Result;
use config::{Config, LiveSettings};
use cycle_state::CycleState;
use daemon::Daemon;
use std::env;
use std::sync::Arc;
#[cfg(unix)]
use std::sync::Mutex;
use window_manager::WindowManager;

#[cfg(unix)]
use daemonize::Daemonize;
#[cfg(unix)]
use overlay::run_overlay;
#[cfg(unix)]
use wayland_backends::{HyprlandManager, KWinManager, SwayManager};
#[cfg(unix)]
use window_manager::{
    detect_display_server, detect_wayland_compositor, DisplayServer, WaylandCompositor,
};
#[cfg(unix)]
use x11_manager::X11Manager;

#[cfg(unix)]
fn create_window_manager() -> Result<Arc<dyn WindowManager>> {
    let display_server = detect_display_server();

    match display_server {
        DisplayServer::X11 => {
            println!("Detected X11 display server");
            Ok(Arc::new(X11Manager::new()?))
        }
        DisplayServer::Wayland => {
            let compositor = detect_wayland_compositor();
            println!(
                "Detected Wayland display server with {:?} compositor",
                compositor
            );

            match compositor {
                WaylandCompositor::Kde => {
                    println!("Using KDE/KWin backend");
                    Ok(Arc::new(KWinManager::new()?))
                }
                WaylandCompositor::Sway => {
                    println!("Using Sway backend");
                    Ok(Arc::new(SwayManager::new()?))
                }
                WaylandCompositor::Hyprland => {
                    println!("Using Hyprland backend");
                    Ok(Arc::new(HyprlandManager::new()?))
                }
                WaylandCompositor::Gnome => {
                    anyhow::bail!("GNOME Shell is not yet supported due to restrictive window management APIs")
                }
                WaylandCompositor::Other => {
                    anyhow::bail!(
                        "Unknown Wayland compositor. Supported: KDE Plasma, Sway, Hyprland"
                    )
                }
            }
        }
    }
}

#[cfg(windows)]
fn create_window_manager() -> Result<Arc<dyn WindowManager>> {
    println!("Using Windows backend");
    Ok(Arc::new(windows_manager::WindowsManager::new()?))
}

enum CycleOp {
    Forward,
    Backward,
    Switch(usize),
}

#[cfg(unix)]
fn start_command(wm: Arc<dyn WindowManager>, config: Config) -> Result<()> {
    let live = LiveSettings::from_config(&config);
    let daemonize = Daemonize::new().working_directory("/tmp").umask(0o027);

    match daemonize.start() {
        Ok(_) => {
            let wm_daemon = Arc::clone(&wm);
            let config_daemon = config.clone();
            let live_daemon = Arc::clone(&live);
            let daemon_thread = std::thread::spawn(move || {
                let mut daemon = Daemon::new(wm_daemon, config_daemon, live_daemon);
                if let Err(e) = daemon.run() {
                    eprintln!("Daemon error: {}", e);
                }
            });

            std::thread::sleep(std::time::Duration::from_millis(100));

            if config.show_overlay {
                let state = Arc::new(Mutex::new(CycleState::new()));
                if let Ok(windows) = wm.get_eve_windows() {
                    state.lock().unwrap().update_windows(windows);
                }

                if let Err(e) = run_overlay(wm, state, config.overlay_x, config.overlay_y, config) {
                    eprintln!("Overlay error: {}", e);
                    std::process::exit(1);
                }
            } else {
                println!("Overlay disabled - daemon running in background");
                daemon_thread.join().unwrap();
            }
            Ok(())
        }
        Err(e) => {
            eprintln!("Failed to daemonize: {}", e);
            std::process::exit(1);
        }
    }
}

#[cfg(windows)]
fn start_command(wm: Arc<dyn WindowManager>, config: Config) -> Result<()> {
    let live = LiveSettings::from_config(&config);

    // Kick off the GitHub-release check on a detached thread. The
    // config panel's footer renders a "NEW VERSION AVAILABLE" link
    // once the result lands (~1s after launch on a normal connection).
    version_check::spawn_check();

    // If a separate daemon is already running (e.g. the user invoked
    // `nicotine.exe daemon` in headless mode), don't spawn a duplicate —
    // just open the config panel. Edits propagate via hot-reload.
    if !ipc::daemon_running() {
        let wm_daemon = Arc::clone(&wm);
        let config_daemon = config.clone();
        let live_daemon = Arc::clone(&live);
        std::thread::spawn(move || {
            let mut daemon = Daemon::new(wm_daemon, config_daemon, live_daemon);
            if let Err(e) = daemon.run() {
                eprintln!("Daemon error: {}", e);
            }
        });
        // Brief pause so the IPC socket is bound before the panel opens.
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // The config panel is the visible app — it shows in the taskbar and
    // owns the process's main thread. eframe blocks here until the user
    // closes the window; on close, the process exits and the daemon
    // thread terminates with it. The shared LiveSettings lets the panel
    // push slider changes straight to the preview manager without
    // waiting for a save-to-disk round-trip.
    if let Err(e) = config_panel::run(config, live) {
        eprintln!("Config panel error: {}", e);
    }
    Ok(())
}

#[cfg(unix)]
fn stop_command() {
    let _ = std::process::Command::new("pkill")
        .arg("-9")
        .arg("nicotine")
        .output();
    let _ = std::fs::remove_file("/tmp/nicotine.sock");
    let _ = std::fs::remove_file(paths::lock_file_path());
}

#[cfg(windows)]
fn stop_command() {
    // Ask the daemon to quit cleanly; ignore errors (it may not be running).
    let _ = ipc::send_line("quit");
    // Force-kill any stragglers that didn't respond.
    let _ = std::process::Command::new("taskkill")
        .args(["/IM", "nicotine.exe", "/F"])
        .output();
    let _ = std::fs::remove_file(paths::lock_file_path());
    let _ = std::fs::remove_file(paths::index_file_path());
}

fn run_cycle_direct(wm: &Arc<dyn WindowManager>, config: &Config, op: CycleOp) -> Result<()> {
    let windows = wm.get_eve_windows()?;
    if windows.is_empty() {
        return Ok(());
    }

    let character_order = if config.characters.is_empty() {
        None
    } else {
        Some(config.characters.clone())
    };

    let mut state = CycleState::new();
    state.set_character_order(character_order.clone());
    state.update_windows(windows);

    if let Ok(active) = wm.get_active_window() {
        state.sync_with_active(active);
    }

    match op {
        CycleOp::Forward => state.cycle_forward(&**wm, config.minimize_inactive)?,
        CycleOp::Backward => state.cycle_backward(&**wm, config.minimize_inactive)?,
        CycleOp::Switch(target) => {
            state.switch_to(
                target,
                &**wm,
                config.minimize_inactive,
                character_order.as_deref(),
            )?;
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    // Declare DPI awareness before any window gets created. SYSTEM_AWARE
    // means Windows reports real monitor DPI instead of bitmap-scaling a
    // virtual 96 DPI canvas — chrome, text, and window dims then need to
    // be scaled manually (see `preview_windows::px`). Without this, users
    // on high-DPI displays get blurry GDI text or weirdly-sized windows
    // depending on launch-order interaction with eframe/winit's own
    // awareness declaration. Ignoring the Result is intentional: the
    // call can fail harmlessly if awareness was already set (e.g. by a
    // hosting process) and we have no recovery.
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE);
    }

    let args: Vec<String> = env::args().collect();
    let command = args.get(1).map(|s| s.as_str()).unwrap_or("");

    let config = Config::load()?;
    let wm = create_window_manager()?;

    match command {
        "start" => {
            println!("Starting Nicotine 🚬");

            #[cfg(unix)]
            if let Ok(Some((new_version, url))) = version_check::check_for_updates() {
                version_check::print_update_notification(&new_version, &url);
            }

            start_command(wm, config)?;
        }

        "daemon" => {
            println!("Starting Nicotine daemon...");
            let live = LiveSettings::from_config(&config);
            let mut daemon = Daemon::new(wm, config, live);
            daemon.run()?;
        }

        #[cfg(unix)]
        "overlay" => {
            println!("Starting Nicotine Overlay...");
            let state = Arc::new(Mutex::new(CycleState::new()));

            if let Ok(windows) = wm.get_eve_windows() {
                state.lock().unwrap().update_windows(windows);
            }

            if let Err(e) = run_overlay(wm, state, config.overlay_x, config.overlay_y, config) {
                eprintln!("Overlay error: {}", e);
                std::process::exit(1);
            }
        }

        "stack" => {
            println!("Stacking EVE windows...");
            let windows = wm.get_eve_windows()?;

            println!(
                "Centering {} EVE clients ({}x{}) on {}x{} display",
                windows.len(),
                config.eve_width,
                config.eve_height_adjusted(),
                config.display_width,
                config.display_height
            );

            wm.stack_windows(&windows, &config)?;

            println!("✓ Stacked {} windows", windows.len());
        }

        "cycle-forward" | "forward" | "f" => {
            if daemon::send_command("forward").is_ok() {
                return Ok(());
            }
            lock::with_cycle_lock(|| run_cycle_direct(&wm, &config, CycleOp::Forward))?;
        }

        "cycle-backward" | "backward" | "b" => {
            if daemon::send_command("backward").is_ok() {
                return Ok(());
            }
            lock::with_cycle_lock(|| run_cycle_direct(&wm, &config, CycleOp::Backward))?;
        }

        "stop" => {
            println!("Stopping Nicotine...");
            stop_command();
            println!("✓ Nicotine stopped");
        }

        "init-config" => {
            Config::save_default()?;
        }

        // Windows double-click: no command arg → go straight to the GUI
        // start path rather than printing help to a hidden console.
        #[cfg(windows)]
        "" => {
            start_command(wm, config)?;
        }

        // Handle switch command or numeric shorthand
        cmd => {
            // Check for "switch N" format
            let target = if cmd == "switch" {
                args.get(2).and_then(|s| s.parse::<usize>().ok())
            } else {
                // Check if it's just a number (shorthand)
                cmd.parse::<usize>().ok()
            };

            if let Some(target) = target {
                if daemon::send_command(&format!("switch:{}", target)).is_ok() {
                    return Ok(());
                }
                lock::with_cycle_lock(|| run_cycle_direct(&wm, &config, CycleOp::Switch(target)))?;
            } else {
                println!();
                println!("🚬 N I C O T I N E 🚬");
                println!();
                println!("Questions or suggestions?");
                println!("Reach out to isomerc on Discord or open a Github issue");
                println!();
                println!("Usage:");
                #[cfg(unix)]
                println!("  nicotine start         - Start everything (daemon + overlay)");
                #[cfg(windows)]
                println!("  nicotine start         - Start everything (daemon + previews)");
                println!("  nicotine stop          - Stop all Nicotine processes");
                println!("  nicotine stack         - Stack all EVE windows");
                println!("  nicotine forward       - Cycle forward");
                println!("  nicotine backward      - Cycle backward");
                println!("  nicotine switch N      - Switch to client N (targeted cycling)");
                println!("  nicotine N             - Shorthand for switch N");
                println!("  nicotine init-config   - Create default config.toml");
                println!();
                println!("Advanced:");
                println!("  nicotine daemon        - Start daemon only");
                #[cfg(unix)]
                println!("  nicotine overlay       - Start overlay only");
                println!();
                println!("Quick start:");
                println!("  nicotine start         # Starts in background automatically");
            }
        }
    }

    Ok(())
}
