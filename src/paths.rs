use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use std::os::unix::fs::PermissionsExt;

pub fn agent_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RUSTVNC_AGENT_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustVNC")
        .join("agent")
}

pub fn client_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("RUSTVNC_CLIENT_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustVNC")
        .join("client")
}

pub fn ensure_private_dir(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

pub fn write_private(path: &Path, contents: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    fs::write(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}
