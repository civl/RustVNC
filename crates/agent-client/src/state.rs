//! Where the pairing material lives on the Windows side.
//!
//! `ca.pem` and `client-cert.pem` are public. `client-key.pem` is the identity
//! the agent allowlist recognises, so it is written with owner-only access where
//! the platform supports it and never leaves this directory.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};

pub struct AgentState {
    pub ca_cert_pem: String,
    pub client_cert_pem: String,
    pub client_key_pem: String,
    /// Fingerprint of the agent's server certificate, pinned at pairing time.
    ///
    /// The agent's certificate only carries `localhost`/`127.0.0.1`/`::1` while
    /// clients reach it over a LAN or Tailscale address, so ordinary TLS name
    /// verification cannot succeed. Pinning the exact certificate is stronger
    /// than matching a name: the connection is accepted only for that one
    /// certificate, and any substitution is refused with a MITM warning.
    pub server_fingerprint: String,
}

/// `%APPDATA%/RustVNC/client/agent` (macOS: `~/Library/Application Support/...`).
pub fn default_state_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustVNC")
        .join("client")
        .join("agent")
}

pub fn is_paired(dir: &Path) -> bool {
    dir.join("ca.pem").is_file()
        && dir.join("client-cert.pem").is_file()
        && dir.join("client-key.pem").is_file()
}

pub fn load(dir: &Path) -> Result<AgentState> {
    let read = |name: &str| -> Result<String> {
        std::fs::read_to_string(dir.join(name)).with_context(|| {
            format!(
                "read {}/{} (pair with the agent first)",
                dir.display(),
                name
            )
        })
    };
    Ok(AgentState {
        ca_cert_pem: read("ca.pem")?,
        client_cert_pem: read("client-cert.pem")?,
        client_key_pem: read("client-key.pem")?,
        server_fingerprint: read("server-fingerprint.txt")?.trim().to_string(),
    })
}

pub fn save(
    dir: &Path,
    ca_cert_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
    server_fingerprint: &str,
) -> Result<()> {
    std::fs::create_dir_all(dir).context("create agent state dir")?;
    write_secret(&dir.join("ca.pem"), ca_cert_pem)?;
    write_secret(&dir.join("client-cert.pem"), client_cert_pem)?;
    write_secret(&dir.join("client-key.pem"), client_key_pem)?;
    write_secret(&dir.join("server-fingerprint.txt"), server_fingerprint)?;
    Ok(())
}

pub fn forget(dir: &Path) -> Result<()> {
    for name in [
        "ca.pem",
        "client-cert.pem",
        "client-key.pem",
        "server-fingerprint.txt",
    ] {
        match std::fs::remove_file(dir.join(name)) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(anyhow!("remove {}: {err}", dir.join(name).display())),
        }
    }
    Ok(())
}

fn write_secret(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod {}", path.display()))?;
    }
    // On Windows the file inherits the user's profile ACL, which already
    // excludes other standard users. Encrypting the key with DPAPI is tracked
    // as a follow-up in the sizeable "harden client" bucket.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_forget_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent");
        assert!(!is_paired(&path));
        save(&path, "ca", "cert", "key", "fp").unwrap();
        assert!(is_paired(&path));
        let state = load(&path).unwrap();
        assert_eq!(state.ca_cert_pem, "ca");
        assert_eq!(state.client_key_pem, "key");
        forget(&path).unwrap();
        assert!(!is_paired(&path));
        assert!(load(&path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), "ca", "cert", "key", "fp").unwrap();
        let mode = std::fs::metadata(dir.path().join("client-key.pem"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}
