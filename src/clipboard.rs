use std::process::Stdio;

use anyhow::{Context, Result, bail};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub const MAX_TEXT_BYTES: usize = 1024 * 1024;

/// Read UTF-8 text from the macOS pasteboard.
///
/// A subprocess is used so this works from the headless CLI and the menu bar
/// app without taking an AppKit main-thread dependency.
pub async fn get_text() -> Result<Option<String>> {
    let output = Command::new("/usr/bin/pbpaste")
        .output()
        .await
        .context("run pbpaste")?;
    if !output.status.success() {
        bail!("pbpaste exited with {}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.is_empty() {
        Ok(None)
    } else {
        Ok(Some(text))
    }
}

/// Write UTF-8 text to the macOS pasteboard.
pub async fn set_text(text: &str) -> Result<()> {
    if text.len() > MAX_TEXT_BYTES {
        bail!("clipboard text too large ({} bytes)", text.len());
    }

    let mut child = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
        .context("spawn pbcopy")?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .await
            .context("write to pbcopy")?;
    }
    let status = child.wait().await.context("wait for pbcopy")?;
    if !status.success() {
        bail!("pbcopy exited with {status}");
    }
    Ok(())
}

pub fn normalize(text: &str) -> Option<String> {
    if text.is_empty() || text.len() > MAX_TEXT_BYTES {
        None
    } else {
        Some(text.to_string())
    }
}
