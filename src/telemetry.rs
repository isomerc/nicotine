// Anonymous launch ping so we can count active users without bundling
// any analytics SDK. Mirrors the pattern used in the sister "moon"
// project: a per-install UUID is generated once on first run, persisted
// to disk, and POSTed alongside the version + OS to the Illuminated
// telemetry endpoint at every daemon start.
//
// Privacy posture: no PII, no IP address logging beyond what HTTPS to
// the endpoint already implies, no behavioral data (cycle counts, EVE
// character names, hotkey bindings — none of it leaves the machine).
//
// Disabled at compile time when NICOTINE_TELEMETRY_TOKEN is unset, so
// dev builds (and any third-party fork that doesn't set the env var)
// never call out.

use std::fs;
use std::path::PathBuf;
use uuid::Uuid;

const TELEMETRY_ENDPOINT: &str = "https://nicotine-telemetry.will-5c0.workers.dev/ping";
const TELEMETRY_TOKEN: Option<&str> = option_env!("NICOTINE_TELEMETRY_TOKEN");

fn device_id_path() -> Option<PathBuf> {
    dirs::data_local_dir().map(|p| p.join("nicotine").join("device_id"))
}

fn get_or_create_device_id() -> Option<String> {
    let path = device_id_path()?;

    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    let id = Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&path, &id);
    Some(id)
}

/// Fire a single telemetry POST in a detached thread. Blocking reqwest
/// is fine — the thread is throwaway and the timeout caps how long we
/// wait. Failures are intentionally swallowed; telemetry must never
/// affect the user-visible behavior of the app.
pub fn send_launch_ping() {
    let token = match TELEMETRY_TOKEN {
        Some(t) => t.to_string(),
        None => return,
    };
    let device_id = match get_or_create_device_id() {
        Some(id) => id,
        None => return,
    };
    let version = env!("CARGO_PKG_VERSION").to_string();
    let os = std::env::consts::OS.to_string();

    std::thread::spawn(move || {
        let client = match reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
        {
            Ok(c) => c,
            Err(_) => return,
        };
        let _ = client
            .post(TELEMETRY_ENDPOINT)
            .header("Authorization", format!("Bearer {}", token))
            .json(&serde_json::json!({
                "device_id": device_id,
                "product": "nicotine",
                "version": version,
                "os": os,
            }))
            .send();
    });
}
