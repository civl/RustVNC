//! Hosting this Mac from the same app bundle.
//!
//! The agent that answers QUIC control sessions, captures audio and switches
//! display modes lives in the `rustvnc-agent` package. Instead of asking the user
//! to install a second app, the bundle ships that binary next to the client and
//! this module starts/stops it on demand — the same split RustDesk uses (UI
//! process plus a service process), without a second installer.
//!
//! Everything here is a no-op off macOS: the Windows build is the controller.

use std::path::PathBuf;

/// Where the helper binary lives, if this bundle shipped one.
pub fn helper_path() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        let candidate = exe.with_file_name("rustvnc-agent");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    // A separately installed agent app still works as a fallback.
    let fallback = PathBuf::from("/Applications/RustVNC Agent.app/Contents/MacOS/rustvnc-agent");
    fallback.is_file().then_some(fallback)
}

pub fn log_path() -> PathBuf {
    let base = dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustVNC")
        .join("client");
    base.join("host.log")
}

#[cfg(target_os = "macos")]
pub fn running() -> bool {
    std::process::Command::new("pgrep")
        .args(["-f", "rustvnc-agent --serve"])
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
pub fn running() -> bool {
    false
}

/// Last `PAIRING CODE:` / `SERVER FINGERPRINT:` lines the helper printed.
///
/// The helper announces them on stdout; reading the log back is the cheapest way
/// to show them in the UI without adding a second control channel.
pub fn last_announcement() -> (Option<String>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(log_path()) else {
        return (None, None);
    };
    let mut code = None;
    let mut fingerprint = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix("PAIRING CODE: ") {
            code = Some(value.trim().to_string());
        }
        if let Some(value) = line.strip_prefix("SERVER FINGERPRINT: ") {
            fingerprint = Some(value.trim().to_string());
        }
    }
    (code, fingerprint)
}

/// Start the helper in the background. Returns the log path on success.
#[cfg(target_os = "macos")]
pub fn start(allow_resolution: bool) -> Result<PathBuf, String> {
    let helper = helper_path()
        .ok_or_else(|| "这个 App 里没有找到 rustvnc-agent（打包时未包含被控端组件）".to_string())?;
    if running() {
        return Ok(log_path());
    }
    let log = log_path();
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let out = std::fs::File::create(&log).map_err(|err| err.to_string())?;
    let err = out.try_clone().map_err(|err| err.to_string())?;
    let mut command = std::process::Command::new(helper);
    command
        .arg("--serve")
        .args(["--listen", "0.0.0.0:8443"])
        .arg("--no-keychain")
        .stdout(std::process::Stdio::from(out))
        .stderr(std::process::Stdio::from(err));
    if allow_resolution {
        command.arg("--allow-resolution");
    }
    command
        .spawn()
        .map_err(|err| format!("启动被控端失败: {err}"))?;
    Ok(log)
}

#[cfg(not(target_os = "macos"))]
pub fn start(_allow_resolution: bool) -> Result<PathBuf, String> {
    Err("只有 macOS 版本包含被控端组件".to_string())
}

#[cfg(target_os = "macos")]
pub fn stop() {
    let _ = std::process::Command::new("pkill")
        .args(["-f", "rustvnc-agent --serve"])
        .status();
}

#[cfg(not(target_os = "macos"))]
pub fn stop() {}
