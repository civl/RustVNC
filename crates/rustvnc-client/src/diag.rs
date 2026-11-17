use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

pub fn log_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("RustVNC")
        .join("client")
        .join("logs")
}

/// Initialise stderr + daily rolling file logging. Keep the guard alive for
/// the lifetime of the app so the non-blocking writer flushes at exit.
pub fn init_tracing() -> tracing_appender::non_blocking::WorkerGuard {
    let directory = log_dir();
    let _ = std::fs::create_dir_all(&directory);
    let file_appender = tracing_appender::rolling::daily(&directory, "client.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // wgpu/Vulkan log their device enumeration at INFO, which drowns the
        // client's own messages in a diagnostics bundle. Their warnings still
        // matter, so only the level is raised.
        tracing_subscriber::EnvFilter::new(
            "info,eframe=warn,wgpu=warn,wgpu_hal=warn,wgpu_core=warn,wgpu_types=warn,naga=warn,quinn=warn,quinn_proto=warn,quinn_udp=warn",
        )
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_writer)
                .with_ansi(false)
                .with_target(false),
        )
        .init();
    guard
}

pub fn open_log_dir() -> Result<()> {
    let directory = log_dir();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create {}", directory.display()))?;
    #[cfg(target_os = "macos")]
    let mut command = Command::new("open");
    #[cfg(target_os = "windows")]
    let mut command = Command::new("explorer");
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let mut command = Command::new("xdg-open");
    command
        .arg(&directory)
        .spawn()
        .with_context(|| format!("open {}", directory.display()))?;
    Ok(())
}

/// Write a text diagnostic bundle containing app/OS info, a profile summary
/// (never passwords) and the tail of the newest client log.
pub fn write_bundle(summary: &str) -> Result<PathBuf> {
    let directory = log_dir();
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("create {}", directory.display()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default();
    let path = directory.join(format!("diagnostics-{stamp}.txt"));

    let mut content = String::new();
    content.push_str(&format!(
        "RustVNC client {}\nOS: {} / {}\n\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
    content.push_str(summary);
    content.push_str("\n\n--- newest client log (tail) ---\n");
    if let Some(log) = newest_log(&directory) {
        content.push_str(&format!("{}\n", log.display()));
        if let Ok(text) = std::fs::read_to_string(&log) {
            let lines: Vec<&str> = text.lines().collect();
            let start = lines.len().saturating_sub(400);
            for line in &lines[start..] {
                content.push_str(line);
                content.push('\n');
            }
        }
    } else {
        content.push_str("(no client log found)\n");
    }

    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn newest_log(directory: &std::path::Path) -> Option<PathBuf> {
    let mut newest = None;
    let mut newest_modified = None;
    for entry in std::fs::read_dir(directory).ok()?.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("client.log"))
        {
            continue;
        }
        let modified = entry.metadata().ok()?.modified().ok()?;
        if newest_modified.is_none_or(|current| modified > current) {
            newest_modified = Some(modified);
            newest = Some(path);
        }
    }
    newest
}
