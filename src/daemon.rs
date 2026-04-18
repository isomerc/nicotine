use crate::config::Config;
use crate::cycle_state::CycleState;
use crate::ipc;
#[cfg(unix)]
use crate::keyboard_listener::KeyboardListener;
#[cfg(unix)]
use crate::mouse_listener::MouseListener;
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
    Quit,
}

impl Command {
    pub fn from_str(s: &str) -> Option<Self> {
        let s = s.trim();
        match s {
            "forward" => Some(Command::Forward),
            "backward" => Some(Command::Backward),
            "refresh" => Some(Command::Refresh),
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
    character_order: Option<Vec<String>>,
}

impl Daemon {
    pub fn new(wm: Arc<dyn WindowManager>, config: Config) -> Self {
        let state = Arc::new(Mutex::new(CycleState::new()));

        // Initialize windows
        if let Ok(windows) = wm.get_eve_windows() {
            state.lock().unwrap().update_windows(windows);
        }

        // Load character order. Used by both targeted cycling (switch N)
        // and forward/backward cycling. Stored on CycleState too so the
        // cycle methods don't need it as a parameter.
        let characters_path = Config::characters_path();
        let character_order = Config::load_characters();
        match &character_order {
            Some(names) => println!(
                "Loaded {} character(s) from {}",
                names.len(),
                characters_path.display()
            ),
            None => println!(
                "characters.txt not found at {} — cycling will use detection order",
                characters_path.display()
            ),
        }
        state
            .lock()
            .unwrap()
            .set_character_order(character_order.clone());

        Self {
            wm,
            state,
            config,
            character_order,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        let listener = ipc::bind_listener()?;
        println!("Nicotine daemon listening for IPC commands");

        // Spawn platform-specific input listeners.
        self.spawn_input_listeners();

        // Refresh window list AND character_order periodically in
        // background. Reloading characters.txt on every tick means edits
        // to the file are picked up within ~500ms — no daemon restart
        // needed when the user adds/reorders character names.
        let wm_clone = Arc::clone(&self.wm);
        let state_clone = Arc::clone(&self.state);
        std::thread::spawn(move || {
            let mut last_order: Option<Vec<String>> = Config::load_characters();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if let Ok(windows) = wm_clone.get_eve_windows() {
                    state_clone.lock().unwrap().update_windows(windows);
                }
                let new_order = Config::load_characters();
                if new_order != last_order {
                    if new_order.is_some() {
                        println!("Reloaded character order from characters.txt");
                    }
                    state_clone
                        .lock()
                        .unwrap()
                        .set_character_order(new_order.clone());
                    last_order = new_order;
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
        if self.config.enable_mouse_buttons {
            let mouse_listener = MouseListener::new(self.config.clone());
            let wm_clone = Arc::clone(&self.wm);
            let state_clone = Arc::clone(&self.state);

            match mouse_listener.spawn(wm_clone, state_clone) {
                Ok(_) => println!("Mouse button listener started"),
                Err(e) => {
                    eprintln!("Warning: Could not start mouse listener: {}", e);
                    eprintln!(
                        "Mouse buttons will not work. You can disable this warning by setting"
                    );
                    eprintln!("'enable_mouse_buttons = false' in ~/.config/nicotine/config.toml");
                }
            }
        }

        if self.config.enable_keyboard_buttons {
            let keyboard_listener = KeyboardListener::new(self.config.clone());
            let wm_clone = Arc::clone(&self.wm);
            let state_clone = Arc::clone(&self.state);

            match keyboard_listener.spawn(wm_clone, state_clone) {
                Ok(_) => println!("Keyboard key listener started"),
                Err(e) => {
                    eprintln!("Warning: Could not start keyboard listener: {}", e);
                    eprintln!(
                        "Keyboard keys will not work.  You can disable this warning by setting"
                    );
                    eprintln!(
                        "'enable_keyboard_buttons = false' in ~/.config/nicotine/config.toml"
                    );
                }
            }
        }
    }

    #[cfg(windows)]
    fn spawn_input_listeners(&self) {
        // Hotkey + low-level mouse hook listener (always spawned).
        let wm_clone = Arc::clone(&self.wm);
        let state_clone = Arc::clone(&self.state);
        match crate::windows_input::spawn(self.config.clone(), wm_clone, state_clone) {
            Ok(_) => println!("Windows input listeners started"),
            Err(e) => eprintln!("Warning: Could not start Windows input listeners: {}", e),
        }

        // DWM preview windows manager (gated by config; defaults to true).
        if self.config.show_previews {
            let wm_clone = Arc::clone(&self.wm);
            let state_clone = Arc::clone(&self.state);
            match crate::preview_windows::spawn(self.config.clone(), wm_clone, state_clone) {
                Ok(_) => println!("DWM preview windows started"),
                Err(e) => {
                    eprintln!("Warning: Could not start preview window manager: {}", e)
                }
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

                    state.switch_to(
                        target,
                        &*self.wm,
                        self.config.minimize_inactive,
                        self.character_order.as_deref(),
                    )?;
                }
                Command::Refresh => {
                    let windows = self.wm.get_eve_windows()?;
                    self.state.lock().unwrap().update_windows(windows);
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
