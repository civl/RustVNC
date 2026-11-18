//! Explicit, session-scoped macOS display-mode control.
//!
//! This is intentionally opt-in: changing the Mac's display mode rearranges the
//! local desktop. The original mode is written to the agent state directory so
//! a crash can be recovered on the next start.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use core_foundation::array::CFArray;
use core_foundation::base::TCFType;
use core_graphics::base::CGDirectDisplayID;
use core_graphics::display::{
    CGConfigureOption, CGDisplay, CGDisplayCopyAllDisplayModes, CGDisplayMode, CGDisplayModeRetain,
};
use foreign_types_shared::ForeignType;

const ORIGINAL_FILE: &str = "display-original.txt";
const APPLY_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy)]
pub struct AppliedMode {
    pub width: u16,
    pub height: u16,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

enum Command {
    Apply {
        width: u16,
        height: u16,
        reply: Sender<Result<AppliedMode>>,
    },
    Restore(Sender<()>),
    Shutdown,
}

/// Handle to a dedicated display thread. Dropping it restores the original
/// mode before returning.
pub struct DisplayGuard {
    commands: Sender<Command>,
    thread: Option<JoinHandle<()>>,
}

impl DisplayGuard {
    pub fn new(state_dir: PathBuf) -> Result<Self> {
        let (commands, receiver) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("rustvnc-display".to_string())
            .spawn(move || display_thread(state_dir, receiver))
            .context("spawn display thread")?;
        Ok(Self {
            commands,
            thread: Some(thread),
        })
    }

    pub fn apply(&self, width: u16, height: u16) -> Result<AppliedMode> {
        let (reply, response) = mpsc::channel();
        self.commands
            .send(Command::Apply {
                width,
                height,
                reply,
            })
            .context("display thread stopped")?;
        response
            .recv_timeout(APPLY_TIMEOUT)
            .context("display mode change timed out")?
    }

    fn restore(&mut self) {
        let (reply, done) = mpsc::channel();
        if self.commands.send(Command::Restore(reply)).is_ok() {
            let _ = done.recv_timeout(APPLY_TIMEOUT);
        }
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for DisplayGuard {
    fn drop(&mut self) {
        self.restore();
    }
}

fn display_thread(state_dir: PathBuf, receiver: Receiver<Command>) {
    let display = CGDisplay::main();
    let mut original: Option<CGDisplayMode> = None;

    while let Ok(command) = receiver.recv() {
        match command {
            Command::Apply {
                width,
                height,
                reply,
            } => {
                if original.is_none() {
                    match display.display_mode() {
                        Some(mode) => {
                            if let Err(err) = save_original(&state_dir, &mode) {
                                let _ = reply.send(Err(err));
                                continue;
                            }
                            original = Some(mode);
                        }
                        None => {
                            let _ = reply
                                .send(Err(anyhow::anyhow!("cannot read the current display mode")));
                            continue;
                        }
                    }
                }
                let _ = reply.send(apply_mode(&display, width, height));
            }
            Command::Restore(done) => {
                if let Some(mode) = original.take() {
                    if let Err(err) = set_mode(&display, &mode) {
                        tracing::warn!("restore display mode failed: {err:#}");
                    } else {
                        let _ = fs::remove_file(original_path(&state_dir));
                    }
                }
                let _ = done.send(());
            }
            Command::Shutdown => {
                if let Some(mode) = original.take() {
                    if let Err(err) = set_mode(&display, &mode) {
                        tracing::warn!("restore display mode on shutdown failed: {err:#}");
                    } else {
                        let _ = fs::remove_file(original_path(&state_dir));
                    }
                }
                break;
            }
        }
    }
}

/// Restore a mode left behind by a previous process. Called once at server
/// startup; a missing file is the normal case.
pub fn restore_saved(state_dir: &Path) -> Result<bool> {
    let path = original_path(state_dir);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(false);
    };
    let values: Vec<u64> = text
        .split_whitespace()
        .filter_map(|value| value.parse().ok())
        .collect();
    if values.len() < 2 {
        let _ = fs::remove_file(&path);
        bail!("invalid saved display mode: {}", path.display());
    }
    let width = values[0] as u16;
    let height = values[1] as u16;
    let display = CGDisplay::main();
    let mode = choose_mode(&display, width, height)
        .with_context(|| format!("find saved mode {width}x{height}"))?;
    set_mode(&display, &mode)?;
    let _ = fs::remove_file(&path);
    Ok(true)
}

fn apply_mode(display: &CGDisplay, width: u16, height: u16) -> Result<AppliedMode> {
    let mode = choose_mode(display, width, height)?;
    set_mode(display, &mode)?;
    let applied = display
        .display_mode()
        .context("read display mode after change")?;
    Ok(AppliedMode {
        width: applied.width() as u16,
        height: applied.height() as u16,
        pixel_width: applied.pixel_width() as u32,
        pixel_height: applied.pixel_height() as u32,
    })
}

fn all_modes(display: &CGDisplay) -> Result<Vec<CGDisplayMode>> {
    let array_ref = unsafe { CGDisplayCopyAllDisplayModes(display.id, std::ptr::null()) };
    if array_ref.is_null() {
        bail!("cannot enumerate display modes");
    }
    let array: CFArray<*const std::ffi::c_void> =
        unsafe { CFArray::wrap_under_create_rule(array_ref) };
    let mut modes = Vec::with_capacity(array.len() as usize);
    for value in array.iter() {
        let pointer = *value as *mut core_graphics::sys::CGDisplayMode;
        if pointer.is_null() {
            continue;
        }
        // CGDisplayCopyAllDisplayModes returns an array that owns its modes.
        // Retain before creating an owning wrapper, otherwise the array and
        // the wrapper both release the same object.
        unsafe { CGDisplayModeRetain(pointer) };
        modes.push(unsafe { CGDisplayMode::from_ptr(pointer) });
    }
    Ok(modes)
}

fn choose_mode(display: &CGDisplay, width: u16, height: u16) -> Result<CGDisplayMode> {
    let modes = all_modes(display)?;
    let mut exact: Vec<CGDisplayMode> = modes
        .iter()
        .filter(|mode| mode.width() == width as u64 && mode.height() == height as u64)
        .cloned()
        .collect();
    if exact.is_empty() {
        exact = modes
            .iter()
            .filter(|mode| {
                mode.pixel_width() == width as u64 && mode.pixel_height() == height as u64
            })
            .cloned()
            .collect();
    }
    if exact.is_empty() {
        let available = modes
            .iter()
            .map(|mode| format!("{}x{}@{}", mode.width(), mode.height(), mode.refresh_rate()))
            .take(12)
            .collect::<Vec<_>>()
            .join(", ");
        bail!("no display mode matches {width}x{height}; available: {available}");
    }
    exact.sort_by(|left, right| {
        right
            .refresh_rate()
            .partial_cmp(&left.refresh_rate())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    exact.into_iter().next().context("no mode")
}

fn set_mode(display: &CGDisplay, mode: &CGDisplayMode) -> Result<()> {
    let config = display
        .begin_configuration()
        .map_err(|err| anyhow::anyhow!("begin display configuration: {err:?}"))?;
    if let Err(err) = display.configure_display_with_display_mode(&config, mode) {
        let _ = display.cancel_configuration(&config);
        bail!("configure display mode: {err:?}");
    }
    display
        .complete_configuration(&config, CGConfigureOption::ConfigureForSession)
        .map_err(|err| anyhow::anyhow!("complete display configuration: {err:?}"))?;
    Ok(())
}

fn save_original(state_dir: &Path, mode: &CGDisplayMode) -> Result<()> {
    fs::create_dir_all(state_dir).with_context(|| format!("create {}", state_dir.display()))?;
    let path = original_path(state_dir);
    fs::write(
        &path,
        format!(
            "{} {} {} {}\n",
            mode.width(),
            mode.height(),
            mode.pixel_width(),
            mode.pixel_height()
        ),
    )
    .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn original_path(state_dir: &Path) -> PathBuf {
    state_dir.join(ORIGINAL_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_at_least_the_current_mode() {
        let display = CGDisplay::main();
        let modes = all_modes(&display).expect("display modes");
        assert!(!modes.is_empty());
    }

    #[test]
    #[ignore = "changes the display mode; run manually"]
    fn applies_1080p_and_restores() {
        let display = CGDisplay::main();
        let current = display.display_mode().expect("current mode");
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = DisplayGuard::new(dir.path().to_path_buf()).expect("guard");
        let applied = guard.apply(1920, 1080).expect("apply 1080p");
        assert_eq!((applied.width, applied.height), (1920, 1080));
        drop(guard);
        let restored = display.display_mode().expect("restored");
        assert_eq!(restored.width(), current.width());
        assert_eq!(restored.height(), current.height());
    }

    #[test]
    #[ignore = "changes the display configuration; run manually on a real Mac"]
    fn applies_the_current_mode_and_restores_it() {
        let display = CGDisplay::main();
        let current = display.display_mode().expect("current mode");
        let dir = tempfile::tempdir().expect("tempdir");
        let guard = DisplayGuard::new(dir.path().to_path_buf()).expect("guard");
        let applied = guard
            .apply(current.width() as u16, current.height() as u16)
            .expect("apply same mode");
        assert_eq!(applied.width as u64, current.width());
        assert_eq!(applied.height as u64, current.height());
        drop(guard);
        let restored = display.display_mode().expect("restored mode");
        assert_eq!(restored.width(), current.width());
        assert_eq!(restored.height(), current.height());
    }
}

/// Mirrors every display onto the main one (single-screen mode) and restores the
/// previous mirroring layout when dropped.
///
/// Mirroring is the only way to make macOS Screen Sharing send one screen: the
/// RFB framebuffer is whatever CGDisplay reports, and a mirrored set collapses to
/// a single surface.
pub struct MirrorGuard {
    previous: Vec<(CGDirectDisplayID, CGDirectDisplayID)>,
}

impl MirrorGuard {
    /// `single = true` mirrors everything onto the main display; `false` unmirrors
    /// every display that is currently mirroring.
    pub fn apply(single: bool) -> Result<Self> {
        let ids = CGDisplay::active_displays()
            .map_err(|err| anyhow::anyhow!("enumerate displays: {err:?}"))?;
        let previous: Vec<(CGDirectDisplayID, CGDirectDisplayID)> = ids
            .iter()
            .map(|id| (*id, CGDisplay::new(*id).mirrors_display()))
            .collect();
        let guard = Self { previous };
        if single {
            let main = CGDisplay::main();
            let config = main
                .begin_configuration()
                .map_err(|err| anyhow::anyhow!("begin display configuration: {err:?}"))?;
            for id in &ids {
                let display = CGDisplay::new(*id);
                if display.is_main() {
                    continue;
                }
                if let Err(err) = display.configure_display_mirror_of_display(&config, &main) {
                    let _ = main.cancel_configuration(&config);
                    bail!("mirror display {id} onto the main one: {err:?}");
                }
            }
            main.complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                .map_err(|err| anyhow::anyhow!("complete display configuration: {err:?}"))?;
        } else {
            let config = CGDisplay::main()
                .begin_configuration()
                .map_err(|err| anyhow::anyhow!("begin display configuration: {err:?}"))?;
            let none = CGDisplay::new(0);
            for id in &ids {
                let display = CGDisplay::new(*id);
                if display.mirrors_display() == 0 && !display.is_main() {
                    continue;
                }
                if let Err(err) = display.configure_display_mirror_of_display(&config, &none) {
                    let _ = CGDisplay::main().cancel_configuration(&config);
                    bail!("unmirror display {id}: {err:?}");
                }
            }
            CGDisplay::main()
                .complete_configuration(&config, CGConfigureOption::ConfigureForSession)
                .map_err(|err| anyhow::anyhow!("complete display configuration: {err:?}"))?;
        }
        Ok(guard)
    }
}

impl Drop for MirrorGuard {
    fn drop(&mut self) {
        let Ok(config) = CGDisplay::main().begin_configuration() else {
            return;
        };
        for (id, master) in &self.previous {
            let display = CGDisplay::new(*id);
            if display.mirrors_display() == *master {
                continue;
            }
            let target = CGDisplay::new(*master);
            let _ = display.configure_display_mirror_of_display(&config, &target);
        }
        let _ = CGDisplay::main()
            .complete_configuration(&config, CGConfigureOption::ConfigureForSession);
    }
}
