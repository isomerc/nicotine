// Per-character preview window positions, persisted to disk so previews
// come back where the user left them across daemon restarts. Schema is
// platform-agnostic; both the Windows DWM thumbnail manager and the
// Linux XComposite manager share this file at
// `~/.config/nicotine/preview_positions.toml`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct PreviewPositions {
    #[serde(default)]
    pub positions: HashMap<String, (i32, i32)>,
    /// Last-known position of the single list-view window (Display Mode =
    /// List). Saved on drag-end, restored on window (re)create. Without
    /// this, toggling display mode would reset the list window to its
    /// default spawn position every time. Currently Windows-only —
    /// Linux ships List mode in a later phase.
    #[serde(default)]
    pub list_position: Option<(i32, i32)>,
}

impl PreviewPositions {
    fn config_path() -> PathBuf {
        let mut p = dirs::config_dir().unwrap_or_else(|| PathBuf::from("."));
        p.push("nicotine");
        p.push("preview_positions.toml");
        p
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(p) = toml::from_str::<Self>(&s) {
                return p;
            }
        }
        Self::default()
    }

    pub fn save(&self) {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(s) = toml::to_string_pretty(self) {
            let _ = std::fs::write(&path, s);
        }
    }

    pub fn get(&self, name: &str) -> Option<(i32, i32)> {
        self.positions.get(name).copied()
    }

    pub fn set(&mut self, name: String, x: i32, y: i32) {
        self.positions.insert(name, (x, y));
    }
}
