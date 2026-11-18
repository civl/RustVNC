//! Signed (checksum + Authenticode) update manifest support.
//!
//! The manifest is intentionally tiny:
//! ```json
//! { "version": "0.2.0", "url": "https://…/RustVNC-Setup-0.2.0.exe",
//!   "sha256": "…", "notes": "…" }
//! ```
//! A download is rejected unless its SHA-256 matches and, on Windows, its
//! Authenticode signature verifies. The current executable is backed up before
//! launching an installer so a failed upgrade can be rolled back.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const MAX_DOWNLOAD: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    pub url: String,
    pub sha256: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    pub version: String,
    pub url: String,
    pub sha256: String,
    pub notes: String,
}

impl From<UpdateManifest> for UpdateInfo {
    fn from(value: UpdateManifest) -> Self {
        Self {
            version: value.version,
            url: value.url,
            sha256: value.sha256,
            notes: value.notes,
        }
    }
}

pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

pub fn is_newer(remote: &str, current: &str) -> bool {
    version_key(remote) > version_key(current)
}

fn version_key(value: &str) -> Vec<u64> {
    value
        .trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .map(|part| part.parse::<u64>().unwrap_or(0))
        .collect()
}

pub fn parse_manifest(text: &str) -> Result<UpdateInfo> {
    let manifest: UpdateManifest = serde_json::from_str(text).context("parse update manifest")?;
    if manifest.version.trim().is_empty() || manifest.url.trim().is_empty() {
        bail!("update manifest is missing version or url");
    }
    let sha = manifest.sha256.trim();
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("update manifest has an invalid sha256");
    }
    Ok(manifest.into())
}

/// Fetch `url` and return an update only when it is newer than this build.
pub fn check(url: &str) -> Result<Option<UpdateInfo>> {
    let mut response = ureq::get(url)
        .call()
        .with_context(|| format!("fetch update manifest {url}"))?;
    let text = response
        .body_mut()
        .read_to_string()
        .context("read update manifest")?;
    let update = parse_manifest(&text)?;
    Ok(is_newer(&update.version, current_version()).then_some(update))
}

/// Download to `dest`, verify SHA-256 and (on Windows) Authenticode.
pub fn download_and_verify(update: &UpdateInfo, dest: &Path) -> Result<PathBuf> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut response = ureq::get(&update.url)
        .call()
        .with_context(|| format!("download update {}", update.url))?;
    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(dest).with_context(|| format!("create {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = reader.read(&mut buffer).context("read update body")?;
        if read == 0 {
            break;
        }
        total += read as u64;
        if total > MAX_DOWNLOAD {
            let _ = fs::remove_file(dest);
            bail!("update download exceeds {MAX_DOWNLOAD} bytes");
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])
            .context("write update download")?;
    }
    file.flush().context("flush update download")?;
    let actual = hex::encode(hasher.finalize());
    if !actual.eq_ignore_ascii_case(update.sha256.trim()) {
        let _ = fs::remove_file(dest);
        bail!(
            "update checksum mismatch: expected {}, got {actual}",
            update.sha256
        );
    }
    verify_authenticode(dest)?;
    Ok(dest.to_path_buf())
}

#[cfg(target_os = "windows")]
fn verify_authenticode(path: &Path) -> Result<()> {
    if std::env::var_os("RUSTVNC_ALLOW_UNSIGNED_UPDATE").is_some() {
        return Ok(());
    }
    let script = format!(
        "(Get-AuthenticodeSignature -LiteralPath '{}').Status",
        path.display().to_string().replace('\'', "''")
    );
    let output = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .context("run Get-AuthenticodeSignature")?;
    let status = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if !output.status.success() || status != "Valid" {
        bail!("update Authenticode signature is not valid ({status}); refusing to install");
    }
    Ok(())
}

#[cfg(not(target_os = "windows"))]
fn verify_authenticode(_path: &Path) -> Result<()> {
    Ok(())
}

pub fn updates_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustVNC")
        .join("updates")
}

/// Copy the running executable so a failed upgrade can be started again.
pub fn backup_current_exe() -> Result<PathBuf> {
    let source = std::env::current_exe().context("locate current executable")?;
    let dir = updates_dir().join("rollback");
    fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let name = format!(
        "{}-{}",
        source
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("rustvnc-client.exe"),
        current_version()
    );
    let destination = dir.join(name);
    fs::copy(&source, &destination)
        .with_context(|| format!("backup {} to {}", source.display(), destination.display()))?;
    Ok(destination)
}

pub fn rollback_installer() -> Option<PathBuf> {
    let dir = updates_dir().join("rollback");
    let mut entries: Vec<_> = fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    entries.sort();
    entries.pop()
}

/// Launch the downloaded installer silently. The caller should exit right after
/// this returns so the installer can replace the running executable.
pub fn launch_installer(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let mut command = if extension == "msi" {
            let mut command = std::process::Command::new("msiexec");
            command.args(["/i", &path.display().to_string(), "/qn", "/norestart"]);
            command
        } else {
            let mut command = std::process::Command::new(path);
            command.args(["/SILENT", "/NORESTART", "/CLOSEAPPLICATIONS"]);
            command
        };
        command.spawn().context("launch updater")?;
        Ok(())
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        bail!("in-place update installers are only supported on Windows")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison_is_numeric() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
    }

    #[test]
    fn manifest_validates_checksum_and_fields() {
        let sha = "a".repeat(64);
        let text = format!(
            r#"{{"version":"0.2.0","url":"https://example.invalid/a.exe","sha256":"{sha}","notes":"x"}}"#
        );
        let info = parse_manifest(&text).unwrap();
        assert_eq!(info.version, "0.2.0");
        assert!(parse_manifest("{}").is_err());
        assert!(parse_manifest(r#"{"version":"1","url":"x","sha256":"bad"}"#).is_err());
    }
}
