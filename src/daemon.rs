use crate::config::{Config, LiveSettings};
use crate::cycle_state::CycleState;
use crate::ipc;
#[cfg(unix)]
use crate::keyboard_listener::{KeyboardConfig, KeyboardListener};
#[cfg(unix)]
use crate::mouse_listener::{MouseConfig, MouseListener};
use crate::telemetry;
use crate::window_manager::WindowManager;
use anyhow::Result;
use interprocess::local_socket::traits::ListenerExt as _;
use std::io::{BufRead, BufReader};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum Command {
    Forward,
    Backward,
    Switch(usize),
    Refresh,
    Stack,
    Quit,
}

impl Command {
    pub fn from_str(s: &str) -> Option<Self> {
        let s = s.trim();
        match s {
            "forward" => Some(Command::Forward),
            "backward" => Some(Command::Backward),
            "refresh" => Some(Command::Refresh),
            "stack" => Some(Command::Stack),
            "quit" => Some(Command::Quit),
            _ => {
                // Check for switch:N format
                if let Some(num_str) = s.strip_prefix("switch:") {
                    if let Ok(num) = num_str.parse::<usize>() {
                        return Some(Command::Switch(num));
                    }
                }
                None
            }
        }
    }
}

pub struct Daemon {
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    config: Config,
    /// Read by the Windows preview manager via `spawn_input_listeners`.
    /// On Linux nothing reads it — kept for ABI symmetry with the Windows
    /// daemon constructor.
    #[cfg_attr(unix, allow(dead_code))]
    live: Arc<Mutex<LiveSettings>>,
    /// Live mouse-cycle settings shared with the Linux input listener
    /// thread. Hot-reload thread updates this on config changes; the
    /// listener picks them up within `POLL_TIMEOUT_MS` (200 ms).
    #[cfg(unix)]
    mouse_config: Arc<Mutex<MouseConfig>>,
    /// Same idea for the Linux keyboard cycle + per-character hotkey
    /// listener.
    #[cfg(unix)]
    keyboard_config: Arc<Mutex<KeyboardConfig>>,
}

impl Daemon {
    pub fn new(wm: Arc<dyn WindowManager>, config: Config, live: Arc<Mutex<LiveSettings>>) -> Self {
        // Anonymous launch ping. Fires once per daemon start (which is
        // also once per `nicotine start` / double-click on Windows), not
        // per cycle command — those go over IPC and don't construct a
        // new Daemon. Detached + best-effort, so a network failure or
        // missing token never blocks startup.
        telemetry::send_launch_ping();

        let state = Arc::new(Mutex::new(CycleState::new()));

        // Initialize windows
        if let Ok(windows) = wm.get_eve_windows() {
            state.lock().unwrap().update_windows(windows);
        }

        // Character order lives in config.toml under `characters`. Used by
        // both targeted cycling (switch N) and forward/backward cycling.
        // Stored on CycleState too so the cycle methods don't need it as
        // a parameter.
        let character_order = if config.characters.is_empty() {
            None
        } else {
            Some(config.characters.clone())
        };
        match &character_order {
            Some(names) => println!("Loaded {} character(s) from config.toml", names.len()),
            None => println!(
                "No `characters` configured in config.toml — cycling will use detection order"
            ),
        }
        state
            .lock()
            .unwrap()
            .set_character_order(character_order.clone());

        #[cfg(unix)]
        let mouse_config = Arc::new(Mutex::new(MouseConfig::from_config(&config)));
        #[cfg(unix)]
        let keyboard_config = Arc::new(Mutex::new(KeyboardConfig::from_config(&config)));

        Self {
            wm,
            state,
            config,
            live,
            #[cfg(unix)]
            mouse_config,
            #[cfg(unix)]
            keyboard_config,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let listener = ipc::bind_listener()?;
        println!("Nicotine daemon listening for IPC commands");

        // Spawn platform-specific input listeners.
        self.spawn_input_listeners();

        // Refresh window list AND character order periodically in
        // background. Re-reading config.toml on every tick means edits
        // (via the config panel or direct file edit) are picked up
        // within ~500ms — no daemon restart needed.
        let wm_clone = Arc::clone(&self.wm);
        let state_clone = Arc::clone(&self.state);
        let mut last_order: Option<Vec<String>> = if self.config.characters.is_empty() {
            None
        } else {
            Some(self.config.characters.clone())
        };
        // Shared listener-config mutexes; the hot-reload thread writes
        // fresh snapshots into them so the always-running listener
        // threads pick up panel-driven changes within ~200ms.
        #[cfg(unix)]
        let mouse_config_clone = Arc::clone(&self.mouse_config);
        #[cfg(unix)]
        let keyboard_config_clone = Arc::clone(&self.keyboard_config);
        // Signature of all hotkey-related fields, used to detect changes
        // and trigger a daemon-side rebind without restart.
        #[cfg(windows)]
        type HotkeySig = (
            bool,
            u16,
            u16,
            Option<u16>,
            std::collections::HashMap<String, crate::config::CharacterHotkey>,
            Option<u16>,
            Option<u16>,
        );
        #[cfg(windows)]
        fn hotkey_sig(c: &Config) -> HotkeySig {
            (
                c.enable_keyboard_buttons,
                c.forward_key,
                c.backward_key,
                c.modifier_key,
                c.character_hotkeys.clone(),
                c.toggle_previews_key,
                c.toggle_previews_modifier,
            )
        }
        #[cfg(windows)]
        let mut last_hotkey_sig = hotkey_sig(&self.config);
        // Track enable_mouse_buttons separately so hot-reload can log
        // when it flips. The atomic-store path that actually applies
        // the change is cheap and unconditional; the log gate is only
        // about observability (tests + users debugging "did my edit
        // take effect").
        #[cfg(windows)]
        let mut last_mouse_enabled = self.config.enable_mouse_buttons;

        std::thread::spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            if let Ok(windows) = wm_clone.get_eve_windows() {
                state_clone.lock().unwrap().update_windows(windows);
            }
            // Re-read config.toml to detect changes to the characters list.
            if let Ok(fresh_config) = Config::load() {
                let new_order = if fresh_config.characters.is_empty() {
                    None
                } else {
                    Some(fresh_config.characters.clone())
                };
                if new_order != last_order {
                    match &new_order {
                        Some(names) => {
                            println!("Reloaded {} character(s) from config.toml", names.len())
                        }
                        None => println!("Character list cleared in config.toml"),
                    }
                    state_clone
                        .lock()
                        .unwrap()
                        .set_character_order(new_order.clone());
                    last_order = new_order;
                }

                // Linux input listeners read live bindings from the
                // shared MouseConfig/KeyboardConfig mutexes; the
                // listener threads pick up changes within ~200ms
                // (their poll timeout). The PartialEq gate avoids
                // re-locking the mutexes 2x/second when nothing
                // changed.
                #[cfg(unix)]
                {
                    let new_mouse = MouseConfig::from_config(&fresh_config);
                    {
                        let mut guard = mouse_config_clone.lock().unwrap();
                        if *guard != new_mouse {
                            println!("Hot-reload: mouse config updated");
                            *guard = new_mouse;
                        }
                    }
                    let new_keyboard = KeyboardConfig::from_config(&fresh_config);
                    {
                        let mut guard = keyboard_config_clone.lock().unwrap();
                        if *guard != new_keyboard {
                            println!("Hot-reload: keyboard config updated");
                            *guard = new_keyboard;
                        }
                    }
                }

                // Hotkey-config change → rebind so the new keys take
                // effect without a daemon restart.
                #[cfg(windows)]
                {
                    let new_sig = hotkey_sig(&fresh_config);
                    if new_sig != last_hotkey_sig {
                        println!("Hot-reload: hotkey config updated");
                        crate::windows_input::resume_hotkeys();
                        last_hotkey_sig = new_sig;
                    }
                    // Mouse-cycle toggle. The atomic store is cheap,
                    // so apply unconditionally; the change-gated log
                    // is just for observability.
                    let new_mouse_enabled = fresh_config.enable_mouse_buttons;
                    if new_mouse_enabled != last_mouse_enabled {
                        println!("Hot-reload: mouse config updated");
                        last_mouse_enabled = new_mouse_enabled;
                    }
                    crate::windows_input::set_mouse_cycle_enabled(new_mouse_enabled);
                }
            }
        });

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    if let Err(e) = self.handle_client(stream) {
                        eprintln!("Error handling client: {}", e);
                    }
                }
                Err(e) => {
                    eprintln!("Connection error: {}", e);
                }
            }
        }

        Ok(())
    }

    #[cfg(unix)]
    fn spawn_input_listeners(&self) {
        // Always spawn the listener threads — they read live config
        // from the shared MouseConfig/KeyboardConfig mutexes and idle
        // when `enable` is false. That way flipping `enable_*` on in
        // the panel after startup actually does something instead of
        // requiring a daemon restart.
        MouseListener::spawn(
            Arc::clone(&self.mouse_config),
            Arc::clone(&self.wm),
            Arc::clone(&self.state),
        );
        KeyboardListener::spawn(
            Arc::clone(&self.keyboard_config),
            Arc::clone(&self.wm),
            Arc::clone(&self.state),
            Arc::clone(&self.live),
        );
        println!("Mouse + keyboard listeners spawned (live config hot-reload enabled)");

        // Linux preview manager — XComposite redirect + XRender server-
        // side composite. Always spawned (matching the Windows daemon);
        // the manager self-gates each reconcile on LiveSettings.show_previews,
        // so it stays dark until previews are on and can be toggled live —
        // including from a cold start with previews off, via the checkbox
        // or the toggle-previews hotkey. The reconcile loop is cheap when
        // dark (no windows, early return). Failure to start is non-fatal;
        // cycling and the config panel keep working.
        let wm_clone = Arc::clone(&self.wm);
        let state_clone = Arc::clone(&self.state);
        let live_clone = Arc::clone(&self.live);
        match crate::preview_x11::spawn(self.config.clone(), wm_clone, state_clone, live_clone) {
            Ok(_) => println!("Linux preview manager started (XRender path)"),
            Err(e) => eprintln!("Warning: Could not start Linux preview manager: {}", e),
        }
    }

    #[cfg(windows)]
    fn spawn_input_listeners(&self) {
        // Hotkey + low-level mouse hook listener (always spawned).
        let wm_clone = Arc::clone(&self.wm);
        let state_clone = Arc::clone(&self.state);
        let live_clone = Arc::clone(&self.live);
        match crate::windows_input::spawn(self.config.clone(), wm_clone, state_clone, live_clone) {
            Ok(_) => println!("Windows input listeners started"),
            Err(e) => eprintln!("Warning: Could not start Windows input listeners: {}", e),
        }

        // DWM preview windows manager — always spawned. The manager
        // self-gates each reconcile on LiveSettings.show_previews so the
        // panel checkbox can hide/show previews live without a daemon
        // restart. Initial state of LiveSettings.show_previews mirrors
        // config.show_previews, so a user who starts with previews off
        // sees no windows until they tick the box.
        let wm_clone = Arc::clone(&self.wm);
        let state_clone = Arc::clone(&self.state);
        let live_clone = Arc::clone(&self.live);
        match crate::preview_windows::spawn(self.config.clone(), wm_clone, state_clone, live_clone)
        {
            Ok(_) => println!("DWM preview windows manager started"),
            Err(e) => {
                eprintln!("Warning: Could not start preview window manager: {}", e)
            }
        }
    }

    fn handle_client(&mut self, stream: interprocess::local_socket::Stream) -> Result<()> {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line)?;

        if let Some(command) = Command::from_str(&line) {
            match command {
                Command::Forward => {
                    let mut state = self.state.lock().unwrap();

                    // Sync with active window first
                    if let Ok(active) = self.wm.get_active_window() {
                        state.sync_with_active(active);
                    }

                    state.cycle_forward(&*self.wm, self.config.minimize_inactive)?;
                }
                Command::Backward => {
                    let mut state = self.state.lock().unwrap();

                    // Sync with active window first
                    if let Ok(active) = self.wm.get_active_window() {
                        state.sync_with_active(active);
                    }

                    state.cycle_backward(&*self.wm, self.config.minimize_inactive)?;
                }
                Command::Switch(target) => {
                    let mut state = self.state.lock().unwrap();

                    // Sync with active window first
                    if let Ok(active) = self.wm.get_active_window() {
                        state.sync_with_active(active);
                    }

                    // Read the cycle order from CycleState (kept fresh
                    // by the hot-reload thread above). Cloning is
                    // necessary because switch_to borrows &mut state.
                    let order = state.character_order().map(|s| s.to_vec());
                    state.switch_to(
                        target,
                        &*self.wm,
                        self.config.minimize_inactive,
                        order.as_deref(),
                    )?;
                }
                Command::Refresh => {
                    let windows = self.wm.get_eve_windows()?;
                    self.state.lock().unwrap().update_windows(windows);
                }
                Command::Stack => {
                    // Re-center every EVE client to the configured stack
                    // geometry. Mirror of the CLI `nicotine stack` path —
                    // same wm.stack_windows call, just driven by IPC so
                    // the config panel can fire it without spawning a
                    // new process.
                    let windows = self.wm.get_eve_windows()?;
                    if let Err(e) = self.wm.stack_windows(&windows, &self.config) {
                        eprintln!("Restack failed: {}", e);
                    }
                }
                Command::Quit => {
                    std::process::exit(0);
                }
            }
        }

        Ok(())
    }
}

pub fn send_command(command: &str) -> Result<()> {
    ipc::send_line(command)
}
