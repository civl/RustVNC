use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use security_framework::passwords::{PasswordOptions, generic_password, set_generic_password};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::paths;

const AGENT_SERVICE: &str = "com.rustvnc.agent";
const CLIENT_SERVICE: &str = "com.rustvnc.client";
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25300;

/// Stores private key material in the login Keychain when available.
///
/// `--no-keychain` / `keychain: false` is used by tests and CI so they do not
/// touch the developer's Keychain. If a Keychain write fails we fall back to a
/// 0600 file so the agent remains usable.
#[derive(Clone, Debug)]
pub struct SecretStore {
    service: &'static str,
    dir: PathBuf,
    keychain: bool,
}

impl SecretStore {
    pub fn agent(dir: &Path, keychain: bool) -> Self {
        Self {
            service: AGENT_SERVICE,
            dir: dir.to_path_buf(),
            keychain,
        }
    }

    pub fn client(dir: &Path, keychain: bool) -> Self {
        Self {
            service: CLIENT_SERVICE,
            dir: dir.to_path_buf(),
            keychain,
        }
    }

    pub fn client_account(dir: &Path) -> String {
        let mut hasher = Sha256::new();
        hasher.update(dir.to_string_lossy().as_bytes());
        let hash = hex::encode(hasher.finalize());
        format!("client-key-{}", &hash[..16])
    }

    pub fn get(&self, account: &str, file_name: &str) -> Result<Option<Vec<u8>>> {
        if self.keychain {
            let options = PasswordOptions::new_generic_password(self.service, account);
            match generic_password(options) {
                Ok(data) => return Ok(Some(data)),
                Err(err) if err.code() == ERR_SEC_ITEM_NOT_FOUND => {
                    // Fall through to the legacy file for migration.
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "Keychain read failed for {}/{} (use --no-keychain to fall back to files)",
                            self.service, account
                        )
                    });
                }
            }
        }

        let path = self.dir.join(file_name);
        match fs::read(&path) {
            Ok(data) => Ok(Some(data)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err).with_context(|| format!("read {}", path.display())),
        }
    }

    pub fn set(&self, account: &str, file_name: &str, data: &[u8]) -> Result<()> {
        if self.keychain {
            match set_generic_password(self.service, account, data) {
                Ok(()) => {
                    let legacy = self.dir.join(file_name);
                    if legacy.exists() {
                        let _ = fs::remove_file(&legacy);
                    }
                    return Ok(());
                }
                Err(err) => {
                    warn!(
                        "Keychain write failed for {}/{}: {err}; falling back to {}",
                        self.service, account, file_name
                    );
                }
            }
        }

        paths::ensure_private_dir(&self.dir)?;
        paths::write_private(&self.dir.join(file_name), data)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::SecretStore;

    #[test]
    fn client_accounts_are_deterministic_and_distinct() {
        let a = SecretStore::client_account(std::path::Path::new("/tmp/a"));
        let b = SecretStore::client_account(std::path::Path::new("/tmp/b"));
        assert_eq!(
            a,
            SecretStore::client_account(std::path::Path::new("/tmp/a"))
        );
        assert_ne!(a, b);
        assert!(a.starts_with("client-key-"));
    }
}
