//! Cross-restart workspace: which tabs were open, which one was active, the
//! window geometry and the UI language.
//!
//! `profiles.json` already describes every connection; what it cannot express is
//! the *workspace* around them. This module owns that, in a sibling file, and
//! deliberately stores no secrets — passwords live in the credential store and
//! `SshConfig::password` is skipped by serde.
//!
//! Restoring never reconnects: a tab comes back as a profile with no session, so
//! an autostart at login cannot open N SSH tunnels behind the user's back.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Geometry is only restored when it could plausibly come from a monitor that is
/// still attached. A stale position from a disconnected display would otherwise
/// open the window somewhere invisible with no way to drag it back.
const POSITION_LIMIT: f32 = 8192.0;
const MIN_WIDTH: f32 = 640.0;
const MIN_HEIGHT: f32 = 480.0;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// Schema marker. Bumped only for incompatible shapes; a file written by a
    /// newer build is refused rather than guessed at.
    pub version: u32,
    /// Open tabs, in order, as profile ids. Order is user-visible.
    pub tabs: Vec<String>,
    /// Index into [`Self::tabs`].
    pub active: Option<usize>,
    pub window: WindowState,
    /// `zh` / `en`; absent keeps the built-in default.
    pub lang: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    pub x: Option<f32>,
    pub y: Option<f32>,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub maximized: bool,
}

impl UiState {
    pub const SCHEMA_VERSION: u32 = 1;

    pub fn default_path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("RustVNC")
            .join("client")
            .join("ui.json")
    }

    /// Read the workspace file. A missing file is an empty workspace; a corrupt
    /// one is an error the caller logs and ignores, because a bad `ui.json` must
    /// never stop the client from starting.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents =
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let state: Self =
            serde_json::from_str(&contents).with_context(|| format!("parse {}", path.display()))?;
        if state.version > Self::SCHEMA_VERSION {
            anyhow::bail!(
                "{} uses schema {} but this build understands {}",
                path.display(),
                state.version,
                Self::SCHEMA_VERSION
            );
        }
        Ok(state)
    }

    /// Write atomically: a crash mid-write must not leave a truncated file that
    /// the next start would refuse.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<()> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        }
        let mut file = self.clone();
        file.version = Self::SCHEMA_VERSION;
        let json = serde_json::to_string_pretty(&file)?;
        let temp = path.with_extension("json.tmp");
        fs::write(&temp, json).with_context(|| format!("write {}", temp.display()))?;
        fs::rename(&temp, path).with_context(|| format!("rename {}", path.display()))?;
        Ok(())
    }

    /// Drop everything that cannot be restored: tabs whose profile was deleted,
    /// duplicates, an active index past the end, and geometry that could only
    /// belong to a monitor that is gone.
    pub fn sanitize(&mut self, profile_exists: impl Fn(&str) -> bool) {
        let mut seen = HashSet::new();
        self.tabs
            .retain(|id| profile_exists(id) && seen.insert(id.clone()));
        self.active = self.active.filter(|index| *index < self.tabs.len());
        self.window = self.window.clamped();
    }
}

impl WindowState {
    /// Keep only plausible values; a rejected value falls back to the default.
    fn clamped(self) -> Self {
        let finite_between =
            |value: Option<f32>, min: f32| value.filter(|value| value.is_finite() && *value >= min);
        let position = |value: Option<f32>| {
            value.filter(|value| value.is_finite() && value.abs() <= POSITION_LIMIT)
        };
        Self {
            x: position(self.x),
            y: position(self.y),
            width: finite_between(self.width, MIN_WIDTH),
            height: finite_between(self.height, MIN_HEIGHT),
            maximized: self.maximized,
        }
    }

    /// Inner size for `ViewportBuilder::with_inner_size`.
    pub fn inner_size(&self) -> Option<[f32; 2]> {
        match (self.width, self.height) {
            (Some(width), Some(height)) => Some([width, height]),
            _ => None,
        }
    }

    /// Outer position for `ViewportBuilder::with_position`.
    pub fn position(&self) -> Option<[f32; 2]> {
        match (self.x, self.y) {
            (Some(x), Some(y)) => Some([x, y]),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn known(_: &str) -> bool {
        true
    }

    #[test]
    fn roundtrip_keeps_tabs_active_and_geometry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.json");
        let state = UiState {
            version: UiState::SCHEMA_VERSION,
            tabs: vec!["a".into(), "b".into()],
            active: Some(1),
            window: WindowState {
                x: Some(120.0),
                y: Some(80.0),
                width: Some(1440.0),
                height: Some(900.0),
                maximized: false,
            },
            lang: Some("en".into()),
        };
        state.save(&path).unwrap();

        let reloaded = UiState::load(&path).unwrap();
        assert_eq!(reloaded.tabs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(reloaded.active, Some(1));
        assert_eq!(reloaded.lang.as_deref(), Some("en"));
        assert_eq!(reloaded.window.inner_size(), Some([1440.0, 900.0]));
        assert_eq!(reloaded.window.position(), Some([120.0, 80.0]));
    }

    #[test]
    fn missing_file_is_an_empty_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let state = UiState::load(dir.path().join("absent.json")).unwrap();
        assert!(state.tabs.is_empty());
        assert_eq!(state.active, None);
    }

    #[test]
    fn corrupt_file_is_an_error_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.json");
        fs::write(&path, "{ this is not json").unwrap();
        assert!(UiState::load(&path).is_err());
    }

    #[test]
    fn a_newer_schema_is_refused_instead_of_guessed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.json");
        fs::write(&path, r#"{"version": 99, "tabs": ["a"]}"#).unwrap();
        assert!(UiState::load(&path).is_err());
    }

    #[test]
    fn sanitize_drops_unknown_profiles_duplicates_and_bad_indexes() {
        let mut state = UiState {
            version: 1,
            tabs: vec!["gone".into(), "a".into(), "a".into(), "b".into()],
            active: Some(9),
            ..UiState::default()
        };
        state.sanitize(|id| id == "a" || id == "b");
        assert_eq!(state.tabs, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(state.active, None, "an index past the end must be dropped");
    }

    #[test]
    fn sanitize_discards_geometry_from_a_vanished_monitor() {
        let mut state = UiState {
            version: 1,
            window: WindowState {
                x: Some(-40_000.0),
                y: Some(50_000.0),
                width: Some(20.0),
                height: Some(f32::NAN),
                maximized: true,
            },
            ..UiState::default()
        };
        state.sanitize(known);
        assert_eq!(state.window.position(), None);
        assert_eq!(state.window.inner_size(), None);
        assert!(state.window.maximized, "maximised is a flag, not geometry");
    }

    #[test]
    fn save_creates_missing_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("client").join("ui.json");
        UiState::default().save(&path).unwrap();
        assert!(path.exists());
    }
}
