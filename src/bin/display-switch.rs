//! Temporary display-mode switch used by scripts/capture-apple.sh.
//!
//! Usage: display-switch <width> <height>
//! Applies the mode, holds it for two seconds, then restores the original.

use std::time::Duration;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let width: u16 = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: display-switch <width> <height>"))?
        .parse()?;
    let height: u16 = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("usage: display-switch <width> <height>"))?
        .parse()?;
    let state_dir = std::env::temp_dir().join("rustvnc-display-switch");
    let guard = rustvnc_agent::display::DisplayGuard::new(state_dir)?;
    let applied = guard.apply(width, height)?;
    println!(
        "display-switch applied {}x{} ({}x{} px)",
        applied.width, applied.height, applied.pixel_width, applied.pixel_height
    );
    std::thread::sleep(Duration::from_secs(2));
    drop(guard);
    Ok(())
}
