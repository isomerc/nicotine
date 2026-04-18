use std::path::PathBuf;

#[cfg(windows)]
fn runtime_dir() -> PathBuf {
    let mut p = dirs::cache_dir().unwrap_or_else(std::env::temp_dir);
    p.push("nicotine");
    let _ = std::fs::create_dir_all(&p);
    p
}

pub fn lock_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/tmp/nicotine-cycle.lock")
    }
    #[cfg(windows)]
    {
        runtime_dir().join("nicotine-cycle.lock")
    }
}

pub fn index_file_path() -> PathBuf {
    #[cfg(unix)]
    {
        PathBuf::from("/tmp/nicotine-index")
    }
    #[cfg(windows)]
    {
        runtime_dir().join("nicotine-index")
    }
}
