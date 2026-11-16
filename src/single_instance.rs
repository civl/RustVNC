use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

use fs2::FileExt;

fn lock_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustVNC")
        .join("agent.lock")
}

/// Acquires an exclusive advisory lock for the lifetime of the returned file.
///
/// The lock is released automatically when the process exits (or the file is
/// dropped). A second instance gets `WouldBlock` and should exit cleanly.
pub fn acquire() -> std::io::Result<File> {
    let path = lock_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&path)?;

    file.try_lock_exclusive()?;
    Ok(file)
}
