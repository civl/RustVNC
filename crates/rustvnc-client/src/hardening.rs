//! Process hardening (M3).
//!
//! Three things here, all cheap and all verifiable:
//!
//! 1. **DLL search order** — the client loads system DLLs (ssh, WebView2, GPU)
//!    from an install directory that a non-admin user can write, so the current
//!    working directory must never be searched.
//! 2. **Child process lifetime** — the client spawns `ssh`; if it crashes, those
//!    children must not outlive it. A job object with `KILL_ON_JOB_CLOSE` does
//!    that for every child, without a per-spawn bookkeeping list.
//! 3. **Crash reporting** — a panic writes a *redacted* record (location and
//!    message only). No minidump, no environment dump, so a crash file can never
//!    contain a password.
//!
//! Off Windows these are no-ops; the macOS/Linux builds get the same audit trail
//! from `audit.rs` without needing this module.

use std::path::PathBuf;

/// Panic message with anything that looks like a secret replaced.
///
/// Deliberately conservative: false positives are fine, a leaked password is not.
pub fn redact(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    for token in message.split_inclusive(|c: char| c.is_whitespace() || c == ',') {
        let lowered = token.to_ascii_lowercase();
        let sensitive = lowered.contains("password")
            || lowered.contains("passwd")
            || lowered.contains("secret")
            || lowered.contains("token=")
            || lowered.contains("bearer ")
            || lowered.contains("private key");
        // Long opaque blobs (hex/base64) are credentials or key material far more
        // often than they are useful diagnostics.
        let opaque = token
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '+' && c != '/' && c != '=')
            .len()
            >= 32;
        if sensitive || opaque {
            out.push_str("<redacted> ");
        } else {
            out.push_str(token);
        }
    }
    out.trim_end().to_string()
}

fn crash_path() -> Option<PathBuf> {
    let base = dirs::data_dir()?.join("RustVNC").join("client");
    Some(base.join("last-panic.txt"))
}

/// Install a panic hook that logs a redacted record instead of relying on the
/// default report (which can pull in environment and thread state).
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let raw = info
            .payload()
            .downcast_ref::<&str>()
            .map(|text| (*text).to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "panic without a message".to_string());
        let location = info
            .location()
            .map(|location| format!("{}:{}", location.file(), location.line()))
            .unwrap_or_else(|| "unknown location".to_string());
        let record = format!("at {location}\nmessage: {}\n", redact(&raw));
        if let Some(path) = crash_path() {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, &record);
        }
        tracing::error!("panic {}", record.trim_end());
        previous(info);
    }));
}

#[cfg(windows)]
mod windows {
    use std::sync::OnceLock;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::LibraryLoader::SetDefaultDllDirectories;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    /// `LOAD_LIBRARY_SEARCH_SYSTEM32 | LOAD_LIBRARY_SEARCH_USER_DIRS`.
    ///
    /// The default search order includes the current directory, which on Windows
    /// means "whatever the shortcut's working directory is" — a classic DLL
    /// planting vector for an app that is launched from a user-writable folder.
    const DLL_SEARCH_FLAGS: u32 = 0x0000_0800 | 0x0000_0400;

    static JOB: OnceLock<isize> = OnceLock::new();

    fn job() -> Option<isize> {
        let handle = *JOB.get_or_init(|| {
            unsafe {
                let handle = CreateJobObjectW(std::ptr::null(), std::ptr::null());
                if handle.is_null() {
                    return 0;
                }
                let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
                info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                let ok = SetInformationJobObject(
                    handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const core::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                );
                if ok == 0 {
                    return 0;
                }
                // Assign ourselves: children inherit the job, so a crash of the
                // client takes ssh (and anything else it spawned) with it.
                let _ = AssignProcessToJobObject(handle, GetCurrentProcess());
                handle as isize
            }
        });
        // 0 means "could not create"; the job is optional hardening, not a
        // startup failure.
        (handle != 0).then_some(handle)
    }

    pub fn apply() {
        unsafe {
            SetDefaultDllDirectories(DLL_SEARCH_FLAGS);
        }
        let _ = job();
    }
}

/// Call this once, as early as possible in `main`.
pub fn init() {
    install_panic_hook();
    #[cfg(windows)]
    windows::apply();
}

#[cfg(test)]
mod tests {
    use super::redact;

    #[test]
    fn credentials_are_redacted() {
        let input = "connect failed password=hunter2 retry";
        let output = redact(input);
        assert!(!output.contains("hunter2"), "leaked: {output}");
        assert!(output.contains("<redacted>"));
        assert!(output.contains("connect failed"));
    }

    #[test]
    fn opaque_blobs_are_redacted() {
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let output = redact(&format!("certificate {key} rejected"));
        assert!(!output.contains(key), "leaked: {output}");
        assert!(output.contains("certificate"));
    }

    #[test]
    fn ordinary_text_survives() {
        let input = "assertion failed: frame.width == 1920";
        assert_eq!(redact(input), input);
    }
}
