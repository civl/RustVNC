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

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
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
