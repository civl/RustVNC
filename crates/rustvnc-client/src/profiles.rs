use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::ClientConfig;

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub config: ClientConfig,
}

impl Profile {
    pub fn new(name: impl Into<String>, config: ClientConfig) -> Self {
        Self {
            id: new_id(),
            name: name.into(),
            config,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    profiles: Vec<Profile>,
}

#[derive(Debug)]
pub struct ProfileStore {
    path: PathBuf,
    profiles: Vec<Profile>,
}

impl ProfileStore {
    pub fn default_path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("RustVNC")
            .join("client")
            .join("profiles.json")
    }

    pub fn empty(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            profiles: Vec::new(),
        }
    }

    pub fn load(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let profiles = if path.exists() {
            let contents =
                fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
            let file: StoreFile = serde_json::from_str(&contents)
                .with_context(|| format!("parse {}", path.display()))?;
            file.profiles
        } else {
            Vec::new()
        };
        Ok(Self { path, profiles })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn profiles(&self) -> &[Profile] {
        &self.profiles
    }

    pub fn profiles_mut(&mut self) -> &mut Vec<Profile> {
        &mut self.profiles
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|profile| profile.id == id)
    }

    pub fn add(&mut self, profile: Profile) -> Result<()> {
        self.profiles.push(profile);
        self.save()
    }

    pub fn update(&mut self, profile: Profile) -> Result<()> {
        if let Some(existing) = self
            .profiles
            .iter_mut()
            .find(|existing| existing.id == profile.id)
        {
            *existing = profile;
            self.save()
        } else {
            self.add(profile)
        }
    }

    pub fn remove(&mut self, id: &str) -> Result<()> {
        self.profiles.retain(|profile| profile.id != id);
        self.save()
    }

    pub fn search(&self, query: &str) -> Vec<&Profile> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return self.profiles.iter().collect();
        }
        self.profiles
            .iter()
            .filter(|profile| {
                profile.name.to_lowercase().contains(&query)
                    || profile.config.host.to_lowercase().contains(&query)
            })
            .collect()
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let file = StoreFile {
            version: 1,
            profiles: self.profiles.clone(),
        };
        let json = serde_json::to_string_pretty(&file)?;
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, json).with_context(|| format!("write {}", temp.display()))?;
        fs::rename(&temp, &self.path).with_context(|| format!("rename {}", self.path.display()))?;
        Ok(())
    }

    pub fn export(&self) -> Result<String> {
        Ok(serde_json::to_string_pretty(&StoreFile {
            version: 1,
            profiles: self.profiles.clone(),
        })?)
    }

    pub fn import(&mut self, json: &str) -> Result<usize> {
        let file: StoreFile = serde_json::from_str(json).context("parse import JSON")?;
        let count = file.profiles.len();
        for profile in file.profiles {
            if !self
                .profiles
                .iter()
                .any(|existing| existing.id == profile.id)
            {
                self.profiles.push(profile);
            }
        }
        self.save()?;
        Ok(count)
    }
}

fn new_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}-{counter:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_store_roundtrip_and_import() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.json");
        let mut store = ProfileStore::load(&path).unwrap();
        let profile = Profile::new("Office Mac", ClientConfig::default());
        store.add(profile.clone()).unwrap();

        let reloaded = ProfileStore::load(&path).unwrap();
        assert_eq!(reloaded.profiles().len(), 1);
        assert_eq!(reloaded.profiles()[0].name, "Office Mac");

        let exported = store.export().unwrap();
        let mut other = ProfileStore::load(dir.path().join("other.json")).unwrap();
        assert_eq!(other.import(&exported).unwrap(), 1);
        assert_eq!(other.profiles().len(), 1);
    }
}
