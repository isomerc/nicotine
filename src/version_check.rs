use anyhow::{Context, Result};
use serde::Deserialize;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const GITHUB_API_URL: &str = "https://api.github.com/repos/isomerc/nicotine/releases/latest";
const TIMEOUT_SECS: u64 = 5;

// The cache + spawn helpers below are consumed by the Windows config
// panel footer only. Linux still uses the synchronous `check_for_updates`
// + `print_update_notification` flow at startup, so on Linux these are
// dead code — `cfg_attr(unix, allow(dead_code))` keeps clippy quiet.

/// Three-state update status. `None`-wrapped at the cache layer to
/// represent the additional "haven't checked yet / check failed" case;
/// the UI renders nothing in that situation, "LATEST VERSION" for
/// `UpToDate`, and "NEW VERSION AVAILABLE" for `Outdated`.
#[cfg_attr(unix, allow(dead_code))]
#[derive(Clone, Debug)]
pub enum UpdateStatus {
    UpToDate,
    Outdated { version: String, url: String },
}

#[cfg_attr(unix, allow(dead_code))]
static UPDATE_STATUS: OnceLock<Mutex<Option<UpdateStatus>>> = OnceLock::new();

#[cfg_attr(unix, allow(dead_code))]
fn status_slot() -> &'static Mutex<Option<UpdateStatus>> {
    UPDATE_STATUS.get_or_init(|| Mutex::new(None))
}

/// Spawn a one-shot background thread that hits the GitHub API and
/// populates the cached update status on success. Called once at app
/// startup; the UI then polls `get_update_status()` per frame.
#[cfg_attr(unix, allow(dead_code))]
pub fn spawn_check() {
    std::thread::spawn(|| {
        if let Ok(status) = fetch_status() {
            *status_slot().lock().unwrap() = Some(status);
        }
    });
}

/// Read the latest cached status. `None` means "haven't checked yet"
/// or "check failed" — render nothing. `Some(UpToDate)` → green
/// "LATEST VERSION". `Some(Outdated { url, .. })` → red link to the
/// new release.
#[cfg_attr(unix, allow(dead_code))]
pub fn get_update_status() -> Option<UpdateStatus> {
    status_slot().lock().unwrap().clone()
}

/// Hits the GitHub API and returns whichever update status applies.
/// Errors (network down, parse failure, etc.) bubble up so the caller
/// can decide whether to surface them or stay silent.
fn fetch_status() -> Result<UpdateStatus> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .user_agent("nicotine")
        .build()
        .context("Failed to build HTTP client")?;

    let response = client
        .get(GITHUB_API_URL)
        .send()
        .context("Failed to fetch latest release from GitHub")?;

    if !response.status().is_success() {
        anyhow::bail!("GitHub API returned {}", response.status());
    }

    let release: GithubRelease = response
        .json()
        .context("Failed to parse GitHub API response")?;
    let latest = release.tag_name.trim_start_matches('v').to_string();

    if is_newer_version(&latest, CURRENT_VERSION)? {
        Ok(UpdateStatus::Outdated {
            version: latest,
            url: release.html_url,
        })
    } else {
        Ok(UpdateStatus::UpToDate)
    }
}

#[derive(Debug, Deserialize)]
struct GithubRelease {
    tag_name: String,
    html_url: String,
}

/// Compatibility wrapper around `fetch_status` that preserves the old
/// `Option<(version, url)>` API the Linux startup banner expects.
/// Returns `Some((..))` when outdated, `None` when up-to-date.
#[cfg(unix)]
pub fn check_for_updates() -> Result<Option<(String, String)>> {
    match fetch_status()? {
        UpdateStatus::Outdated { version, url } => Ok(Some((version, url))),
        UpdateStatus::UpToDate => Ok(None),
    }
}

/// Compares two semantic versions (e.g., "0.2.1" vs "0.2.0")
/// Returns true if `latest` is newer than `current`
fn is_newer_version(latest: &str, current: &str) -> Result<bool> {
    let latest_parts = parse_version(latest)?;
    let current_parts = parse_version(current)?;

    Ok(latest_parts > current_parts)
}

/// Parses a version string like "0.2.1" into (major, minor, patch)
fn parse_version(version: &str) -> Result<(u32, u32, u32)> {
    let parts: Vec<&str> = version.split('.').collect();

    if parts.len() != 3 {
        anyhow::bail!("Invalid version format: {}", version);
    }

    let major = parts[0]
        .parse::<u32>()
        .context("Failed to parse major version")?;
    let minor = parts[1]
        .parse::<u32>()
        .context("Failed to parse minor version")?;
    let patch = parts[2]
        .parse::<u32>()
        .context("Failed to parse patch version")?;

    Ok((major, minor, patch))
}

/// Prints an update notification to the user. Linux-only — the curl-bash
/// install line doesn't apply to Windows users (who get the notification
/// via the config panel's footer link instead).
#[cfg(unix)]
pub fn print_update_notification(new_version: &str, url: &str) {
    println!();
    println!("─────────────────────────────────────────────────────────────────────────");
    println!();
    println!("🚬 A new version of nicotine is available!");
    println!();
    println!("Current version: {}", CURRENT_VERSION);
    println!("Latest version:  {}", new_version);
    println!();
    println!("Update with:");
    println!("curl -sSL https://raw.githubusercontent.com/isomerc/nicotine/refs/heads/master/install-github.sh | bash");
    println!();
    println!("Release notes: {}", url);
    println!();
    println!("─────────────────────────────────────────────────────────────────────────");
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        assert_eq!(parse_version("0.2.1").unwrap(), (0, 2, 1));
        assert_eq!(parse_version("1.0.0").unwrap(), (1, 0, 0));
        assert_eq!(parse_version("10.20.30").unwrap(), (10, 20, 30));
    }

    #[test]
    fn test_is_newer_version() {
        assert!(is_newer_version("0.2.2", "0.2.1").unwrap());
        assert!(is_newer_version("0.3.0", "0.2.9").unwrap());
        assert!(is_newer_version("1.0.0", "0.9.9").unwrap());

        assert!(!is_newer_version("0.2.1", "0.2.1").unwrap());
        assert!(!is_newer_version("0.2.0", "0.2.1").unwrap());
        assert!(!is_newer_version("0.1.9", "0.2.0").unwrap());
    }
}
