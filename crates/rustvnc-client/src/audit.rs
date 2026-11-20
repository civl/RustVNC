//! Append-only audit trail (M3).
//!
//! The security story needs evidence, not assertions: who connected where, with
//! which transport, when a host key was accepted, when the plaintext warning was
//! acknowledged, when an update was installed or rolled back.
//!
//! One JSON object per line, never rewritten, rotated at 4 MiB, secrets redacted.
//! It is deliberately *not* part of the diagnostic bundle: the bundle is shared
//! with support, the audit trail stays local.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hardening::redact;

const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub struct Audit {
    path: PathBuf,
    // Serialised so two events cannot interleave inside a line.
    lock: Mutex<()>,
}

impl Audit {
    pub fn default_path() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("RustVNC")
            .join("client")
            .join("audit.jsonl")
    }

    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            lock: Mutex::new(()),
        }
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Append one event. `fields` are `(name, value)` pairs; values are redacted.
    pub fn record(&self, event: &str, fields: &[(&str, &str)]) {
        let _guard = self.lock.lock();
        let mut line = String::with_capacity(160);
        line.push('{');
        line.push_str(&format!("\"ts\":{},", timestamp_millis()));
        line.push_str(&format!("\"event\":{},", json_string(event)));
        for (index, (name, value)) in fields.iter().enumerate() {
            if index > 0 {
                line.push(',');
            }
            // A value called "password" is a secret whatever it looks like;
            // `redact` handles secrets that hide inside ordinary prose.
            let value = if sensitive_name(name) {
                "<redacted>".to_string()
            } else {
                redact(value)
            };
            line.push_str(&format!("{}:{}", json_string(name), json_string(&value)));
        }
        line.push_str("}\n");

        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        self.rotate_if_needed();
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn rotate_if_needed(&self) {
        let too_big = fs::metadata(&self.path)
            .map(|metadata| metadata.len() > MAX_BYTES)
            .unwrap_or(false);
        if !too_big {
            return;
        }
        let mut rotated = self.path.clone();
        rotated.set_extension("jsonl.1");
        let _ = fs::rename(&self.path, rotated);
    }

    /// Number of lines, used by tests and diagnostics.
    pub fn line_count(&self) -> usize {
        fs::read_to_string(&self.path)
            .map(|text| text.lines().count())
            .unwrap_or(0)
    }
}

/// Field names whose value is a secret by definition.
fn sensitive_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.contains("password")
        || name.contains("passwd")
        || name.contains("secret")
        || name.contains("token")
        || name.contains("private")
}

fn timestamp_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|delta| delta.as_millis())
        .unwrap_or(0)
}

/// Minimal JSON string escaping; enough for the values this module writes.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_one_json_object_per_line() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::new(dir.path().join("audit.jsonl"));
        audit.record("connect", &[("host", "192.0.2.10"), ("transport", "ssh")]);
        audit.record("disconnect", &[("reason", "user")]);
        let text = fs::read_to_string(audit.path()).unwrap();
        assert_eq!(text.lines().count(), 2);
        for line in text.lines() {
            let value: serde_json::Value = serde_json::from_str(line).expect("valid json");
            assert!(value.get("ts").is_some());
            assert!(value.get("event").is_some());
        }
        assert!(text.contains("\"transport\":\"ssh\""));
    }

    #[test]
    fn secrets_do_not_reach_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::new(dir.path().join("audit.jsonl"));
        audit.record("auth", &[("password", "hunter2"), ("user", "zhouwei")]);
        let text = fs::read_to_string(audit.path()).unwrap();
        assert!(!text.contains("hunter2"), "leaked: {text}");
        assert!(text.contains("zhouwei"));
    }

    #[test]
    fn quotes_and_newlines_are_escaped() {
        let dir = tempfile::tempdir().unwrap();
        let audit = Audit::new(dir.path().join("audit.jsonl"));
        audit.record("note", &[("text", "a \"quote\"\nand a newline")]);
        let text = fs::read_to_string(audit.path()).unwrap();
        let value: serde_json::Value = serde_json::from_str(text.trim()).expect("valid json");
        assert_eq!(value["text"], "a \"quote\"\nand a newline");
    }
}
