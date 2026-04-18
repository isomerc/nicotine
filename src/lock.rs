use crate::paths;
use anyhow::Result;
use fd_lock::RwLock;
use std::fs::OpenOptions;

/// Run `f` while holding an exclusive cross-process lock on the cycle lock file.
/// Returns Ok(Some(value)) if the lock was acquired and `f` ran. Returns
/// Ok(None) if another process already holds the lock — in that case `f` is
/// skipped entirely (the caller should treat this as "another cycle is in
/// flight, drop this one"). Errors only on filesystem failures.
pub fn with_cycle_lock<F, R>(f: F) -> Result<Option<R>>
where
    F: FnOnce() -> Result<R>,
{
    let path = paths::lock_file_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;

    let mut lock = RwLock::new(file);
    let result = match lock.try_write() {
        Ok(_guard) => Some(f()?),
        Err(_) => None,
    };
    Ok(result)
}
