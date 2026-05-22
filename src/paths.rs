use std::path::PathBuf;

#[cfg(windows)]
fn runtime_dir() -> PathBuf {
    let mut p = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    p.push("nicotine");
    let _ = std::fs::create_dir_all(&p);
    p
}

/// On Linux, integration tests set `NICOTINE_RUNTIME_DIR` to a private
/// tmp directory so the lock + index files don't collide with a real
/// daemon's. Falls back to `/tmp` in production (matches the
/// historical hardcoded paths). Windows already uses a per-user cache
/// dir, so it doesn't need the override.
#[cfg(unix)]
fn unix_runtime_path(filename: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("NICOTINE_RUNTIME_DIR") {
        let mut p = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&p);
        p.push(filename);
        p
    } else {
        PathBuf::from(format!("/tmp/{}", filename))
    }
}

pub fn lock_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        unix_runtime_path("nicotine-cycle.lock")
    }
    #[cfg(windows)]
    {
        runtime_dir().join("nicotine-cycle.lock")
    }
}

pub fn index_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        unix_runtime_path("nicotine-index")
    }
    #[cfg(windows)]
    {
        runtime_dir().join("nicotine-index")
    }
}
