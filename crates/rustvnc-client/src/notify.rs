//! Desktop notifications for events the user is not looking at.
//!
//! The policy is the interesting part, not the toast: a flapping link emits
//! `Reconnecting` once per backoff attempt (1 s, 2 s, 4 s…), so notifying on
//! every event would fill the Action Center with the same sentence. [`Notifier`]
//! collapses repeats per kind, and the caller suppresses everything while the
//! window is in front of the user.
//!
//! Only `show` and `ensure_registered` touch the shell; the rest is plain state
//! and is unit-tested without raising a real toast.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Events worth interrupting the user for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// The session gave up (non-retryable failure).
    Disconnected,
    /// The first failure of a streak; later attempts stay quiet for a while.
    Reconnecting,
    /// Back online after a reconnect streak.
    Reconnected,
    /// A newer release was found by an update check.
    UpdateAvailable,
}

/// Repeats of one kind inside this window are dropped.
///
/// A minute is long enough to swallow a backoff ladder (1+2+4+8+15+15…) yet short
/// enough that a second outage an hour later still notifies.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// `AppUserModelID` the shell attributes toasts to. Registered under the current
/// user's `HKCU\Software\Classes\AppUserModelId`, which is what lets an
/// unpackaged desktop app show a named, icon-bearing toast.
pub const APP_ID: &str = "RustVNC.Client";

pub struct Notifier {
    enabled: bool,
    last: HashMap<Kind, Instant>,
}

impl Notifier {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last: HashMap::new(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Show `title`/`body` unless notifications are off or this kind is on
    /// cooldown. Returns whether a toast was handed to the shell.
    ///
    /// `now` is a parameter so the cooldown can be exercised without sleeping.
    pub fn notify(&mut self, kind: Kind, title: &str, body: &str, now: Instant) -> bool {
        self.notify_with(kind, title, body, now, show)
    }

    /// [`Self::notify`] with the shell call injected.
    ///
    /// Tests pass a recorder here: exercising the real entry point must not pop
    /// a toast on the machine running `cargo test`.
    fn notify_with(
        &mut self,
        kind: Kind,
        title: &str,
        body: &str,
        now: Instant,
        mut sink: impl FnMut(&str, &str),
    ) -> bool {
        if !self.should_show(kind, now) {
            return false;
        }
        sink(title, body);
        true
    }

    /// The decision half of [`Self::notify`], kept separate so tests never reach
    /// the shell.
    fn should_show(&mut self, kind: Kind, now: Instant) -> bool {
        if !self.enabled {
            return false;
        }
        if let Some(last) = self.last.get(&kind)
            && now.saturating_duration_since(*last) < COOLDOWN
        {
            return false;
        }
        self.last.insert(kind, now);
        true
    }
}

/// Register [`APP_ID`] so the toast carries the app's name and icon.
///
/// Idempotent and best-effort: a failure only costs the attribution, never the
/// notification, so it must not stop start-up.
pub fn ensure_registered() {
    #[cfg(windows)]
    platform::ensure_app_id();
}

/// Send a toast now, ignoring the enable switch and the cooldown.
///
/// Backs `--test-notification`: "notifications do not appear" is nearly always a
/// Focus Assist / app-permission setting, and a one-shot self-test separates
/// that from a bug in this code without needing a disconnect.
pub fn show_now(title: &str, body: &str) {
    show(title, body);
}

#[cfg(windows)]
fn show(title: &str, body: &str) {
    use tauri_winrt_notification::Toast;
    if let Err(err) = Toast::new(APP_ID).title(title).text1(body).show() {
        tracing::warn!("notification failed: {err}");
    }
}

#[cfg(not(windows))]
fn show(title: &str, body: &str) {
    let _ = (title, body);
}

#[cfg(windows)]
mod platform {
    /// `windows-registry` is already in the graph through `auto-launch`, so this
    /// costs no extra build; its safe API avoids hand-rolling RegSetValueExW.
    pub fn ensure_app_id() {
        let path = format!(r"SOFTWARE\Classes\AppUserModelId\{}", super::APP_ID);
        let result = (|| -> windows_registry::Result<()> {
            let key = windows_registry::CURRENT_USER.create(&path)?;
            // Only write what is missing or stale: rewriting the values on every
            // launch would be rude and is unnecessary once they are right.
            if key
                .get_string("DisplayName")
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
            {
                key.set_string("DisplayName", "RustVNC")?;
            }
            let icon = std::env::current_exe()
                .map(|path| path.display().to_string())
                .unwrap_or_default();
            if !icon.is_empty() && key.get_string("IconUri").unwrap_or_default() != icon {
                key.set_string("IconUri", &icon)?;
            }
            Ok(())
        })();
        if let Err(err) = result {
            tracing::warn!("register notification app id: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn repeats_inside_the_cooldown_are_dropped() {
        let mut notifier = Notifier::new(true);
        let start = base();
        assert!(notifier.should_show(Kind::Reconnecting, start));
        assert!(!notifier.should_show(Kind::Reconnecting, start + Duration::from_secs(1)));
        assert!(!notifier.should_show(Kind::Reconnecting, start + COOLDOWN / 2));
        assert!(notifier.should_show(
            Kind::Reconnecting,
            start + COOLDOWN + Duration::from_secs(1)
        ));
    }

    #[test]
    fn different_kinds_do_not_block_each_other() {
        let mut notifier = Notifier::new(true);
        let start = base();
        assert!(notifier.should_show(Kind::Reconnecting, start));
        assert!(notifier.should_show(Kind::Disconnected, start));
        assert!(notifier.should_show(Kind::Reconnected, start));
    }

    #[test]
    fn a_disabled_notifier_shows_nothing_and_recovers_when_reenabled() {
        let mut notifier = Notifier::new(false);
        let start = base();
        assert!(!notifier.should_show(Kind::Disconnected, start));
        notifier.set_enabled(true);
        assert!(notifier.enabled());
        // Nothing was recorded while disabled, so the first enabled event passes.
        assert!(notifier.should_show(Kind::Disconnected, start));
    }

    #[test]
    fn a_real_call_goes_through_the_sink_and_reports_what_it_did() {
        // The shipping entry point, with the shell call captured: the first call
        // must pass one toast to the sink, the repeat inside the cooldown none.
        let mut notifier = Notifier::new(true);
        let start = base();
        let mut shown = Vec::new();
        assert!(
            notifier.notify_with(Kind::UpdateAvailable, "t", "b", start, |title, body| shown
                .push(format!("{title}/{body}")),)
        );
        assert!(
            !notifier.notify_with(Kind::UpdateAvailable, "t", "b", start, |title, body| shown
                .push(format!("{title}/{body}")),)
        );
        assert_eq!(shown, vec!["t/b".to_string()]);
    }
}
