use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::{Context, Result};
use keyring::Entry;

/// Stores per-profile passwords in the OS credential vault.
///
/// macOS: login Keychain. Windows: Credential Manager. A memory backend is
/// used by tests and by the "don't save" option.
pub struct CredentialStore {
    service: String,
    memory: Option<Mutex<HashMap<String, String>>>,
}

impl CredentialStore {
    pub fn system() -> Self {
        Self {
            service: "com.rustvnc.client".to_string(),
            memory: None,
        }
    }

    pub fn memory() -> Self {
        Self {
            service: "com.rustvnc.client.memory".to_string(),
            memory: Some(Mutex::new(HashMap::new())),
        }
    }

    pub fn is_memory(&self) -> bool {
        self.memory.is_some()
    }

    pub fn set(&self, profile_id: &str, password: &str) -> Result<()> {
        if let Some(memory) = &self.memory {
            memory
                .lock()
                .expect("credential memory")
                .insert(profile_id.to_string(), password.to_string());
            return Ok(());
        }
        let entry = Entry::new(&self.service, profile_id).context("open credential entry")?;
        entry.set_password(password).context("save credential")?;
        Ok(())
    }

    pub fn get(&self, profile_id: &str) -> Result<Option<String>> {
        if let Some(memory) = &self.memory {
            return Ok(memory
                .lock()
                .expect("credential memory")
                .get(profile_id)
                .cloned());
        }
        let entry = Entry::new(&self.service, profile_id).context("open credential entry")?;
        match entry.get_password() {
            Ok(password) => Ok(Some(password)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(err) => Err(err).context("read credential"),
        }
    }

    pub fn delete(&self, profile_id: &str) -> Result<()> {
        if let Some(memory) = &self.memory {
            memory.lock().expect("credential memory").remove(profile_id);
            return Ok(());
        }
        let entry = Entry::new(&self.service, profile_id).context("open credential entry")?;
        match entry.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(err) => Err(err).context("delete credential"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_backend_roundtrip() {
        let store = CredentialStore::memory();
        assert_eq!(store.get("p1").unwrap(), None);
        store.set("p1", "secret").unwrap();
        assert_eq!(store.get("p1").unwrap().as_deref(), Some("secret"));
        store.delete("p1").unwrap();
        assert_eq!(store.get("p1").unwrap(), None);
    }
}
