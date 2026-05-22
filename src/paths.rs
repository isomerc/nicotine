use std::path::PathBuf;

/// Resolve the directory where Nicotine writes its lock file + cycle
/// index. Integration tests set `NICOTINE_RUNTIME_DIR` to a private
/// tmp directory so the lock + index files don't collide with a real
/// running daemon's. In production:
/// - Linux: `/tmp` (matches the historical hardcoded paths).
/// - Windows: the user's cache dir under a `nicotine` subdir.
fn runtime_path(filename: &str) -> PathBuf {
    if let Ok(dir) = std::env::var("NICOTINE_RUNTIME_DIR") {
        let mut p = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&p);
        p.push(filename);
        return p;
    }
    #[cfg(unix)]
    {
        PathBuf::from(format!("/tmp/{}", filename))
    }
    #[cfg(windows)]
    {
        let mut p = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
        p.push("nicotine");
        let _ = std::fs::create_dir_all(&p);
        p.push(filename);
        p
    }
}

pub fn lock_file_path() -> PathBuf {
    runtime_path("nicotine-cycle.lock")
}

pub fn index_file_path() -> PathBuf {
    runtime_path("nicotine-index")
}
