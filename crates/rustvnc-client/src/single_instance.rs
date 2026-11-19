//! One running client per session.
//!
//! Two copies each hold the whole `profiles.json` and `ui.json` in memory and
//! save them wholesale, so the second one to write silently deletes whatever the
//! other changed — tabs, settings, window geometry. A named kernel mutex makes a
//! second launch hand control back to the first instead of starting beside it:
//! the second process signals "come to the front" and exits, and the first
//! raises its window.
//!
//! Enforcement is Windows-only (the product target). On other platforms
//! [`SingleInstance::acquire`] always reports the process as primary, which keeps
//! the crate building and its tests running on the macOS CI job.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// How long the listener sleeps before re-checking its stop flag, so shutdown
/// does not depend on receiving one more event.
const POLL_INTERVAL_MS: u32 = 250;

type Waker = Box<dyn Fn() + Send + Sync>;

/// A cloneable view of a running instance, for the parts of the app that must
/// not own the guard (the UI is rebuilt on every renderer attempt).
#[derive(Clone)]
pub struct InstanceHandle {
    raises: Arc<AtomicUsize>,
    waker: Arc<Mutex<Option<Waker>>>,
}

impl InstanceHandle {
    /// How many times another launch asked this instance to show itself.
    ///
    /// A counter rather than a channel because the handle is cloned into each
    /// renderer attempt: a queued message would be consumed by an attempt that
    /// never produced a window.
    pub fn raise_count(&self) -> usize {
        self.raises.load(Ordering::Relaxed)
    }

    /// Called by the listener when a raise request arrives.
    ///
    /// Needed because a window hidden to the tray may not be repainting at all:
    /// without this the request would sit until the user touched the window.
    pub fn set_waker(&self, wake: impl Fn() + Send + Sync + 'static) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(Box::new(wake));
        }
    }
}

/// Ownership of the single-instance lock, held for the life of the process.
pub struct SingleInstance {
    primary: bool,
    handle: InstanceHandle,
    stop: Arc<AtomicBool>,
    listener: Option<JoinHandle<()>>,
    /// Held only so the kernel object stays open for the process lifetime; the
    /// owning wrapper closes it on drop.
    #[cfg(windows)]
    _mutex: Option<sys::Handle>,
    /// The event a losing launch signals; kept open so the name stays resolvable.
    #[cfg(windows)]
    _event: Option<sys::Handle>,
}

impl SingleInstance {
    /// Try to become the one running client.
    ///
    /// `name` is the kernel object name; keep it stable across releases, because
    /// changing it lets an old and a new build run side by side.
    pub fn acquire(name: &str) -> Self {
        let handle = InstanceHandle {
            raises: Arc::new(AtomicUsize::new(0)),
            waker: Arc::new(Mutex::new(None)),
        };
        let stop = Arc::new(AtomicBool::new(false));

        #[cfg(windows)]
        {
            match sys::acquire_mutex_and_event(name) {
                sys::Acquired::Primary(mutex, event) => {
                    let listener =
                        spawn_listener(name.to_string(), handle.clone(), Arc::clone(&stop));
                    Self {
                        primary: true,
                        handle,
                        stop,
                        listener,
                        _mutex: Some(mutex),
                        _event: Some(event),
                    }
                }
                sys::Acquired::AlreadyRunning => Self {
                    primary: false,
                    handle,
                    stop,
                    listener: None,
                    _mutex: None,
                    _event: None,
                },
                sys::Acquired::Unavailable => {
                    // The kernel refused to create the objects; refusing to start
                    // would be worse than running unprotected.
                    tracing::warn!("single-instance mutex unavailable; continuing unguarded");
                    Self {
                        primary: true,
                        handle,
                        stop,
                        listener: None,
                        _mutex: None,
                        _event: None,
                    }
                }
            }
        }

        #[cfg(not(windows))]
        {
            let _ = name;
            Self {
                primary: true,
                handle,
                stop,
                listener: None,
            }
        }
    }

    /// Run without taking the lock. Automation (`--bench-frames`, the soak
    /// script) may legitimately run beside a normal client.
    pub fn standalone() -> Self {
        Self {
            primary: true,
            handle: InstanceHandle {
                raises: Arc::new(AtomicUsize::new(0)),
                waker: Arc::new(Mutex::new(None)),
            },
            stop: Arc::new(AtomicBool::new(false)),
            listener: None,
            #[cfg(windows)]
            _mutex: None,
            #[cfg(windows)]
            _event: None,
        }
    }

    /// `false` when another process already holds the lock.
    pub fn is_primary(&self) -> bool {
        self.primary
    }

    pub fn handle(&self) -> InstanceHandle {
        self.handle.clone()
    }
}

/// Ask the running instance to come to the front. Returns whether one heard.
///
/// Called by the losing process just before it exits; it has no window of its
/// own to show and no way to tell the user anything otherwise.
pub fn signal_existing(name: &str) -> bool {
    #[cfg(windows)]
    {
        sys::signal(name)
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

fn spawn_listener(
    name: String,
    handle: InstanceHandle,
    stop: Arc<AtomicBool>,
) -> Option<JoinHandle<()>> {
    #[cfg(windows)]
    {
        std::thread::Builder::new()
            .name("rustvnc-single-instance".to_string())
            .spawn(move || {
                // The listener opens its own handle to the same named event: a
                // raw HANDLE is not `Send`, and the name is the shared identity.
                let Some(event) = sys::open_for_waiting(&name) else {
                    tracing::warn!("single-instance listener could not open its event");
                    return;
                };
                while !stop.load(Ordering::Relaxed) {
                    if sys::wait(&event, POLL_INTERVAL_MS) {
                        handle.raises.fetch_add(1, Ordering::Relaxed);
                        if let Ok(slot) = handle.waker.lock()
                            && let Some(wake) = slot.as_ref()
                        {
                            wake();
                        }
                    }
                }
            })
            .map_err(|err| tracing::warn!("spawn single-instance listener: {err}"))
            .ok()
    }
    #[cfg(not(windows))]
    {
        let _ = (name, handle, stop);
        None
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        #[cfg(windows)]
        if let Some(event) = &self._event {
            // Wake the listener instead of waiting out its poll interval.
            sys::set(event);
        }
        if let Some(listener) = self.listener.take() {
            let _ = listener.join();
        }
        // Handles (and with them the mutex) close in `sys::Handle`'s Drop.
    }
}

#[cfg(windows)]
mod sys {
    use std::ffi::OsStr;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr;

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
    };

    /// Owning wrapper so the handles close exactly once, on drop.
    pub struct Handle(pub HANDLE);

    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: the handle came from this module and is dropped once.
                unsafe { CloseHandle(self.0) };
            }
        }
    }

    /// Mutexes and events live in the per-session namespace: the app's files are
    /// per-user anyway, and `Global\` would let one logged-in user block another.
    fn object_name(name: &str, suffix: &str) -> Vec<u16> {
        OsStr::new(&format!("Local\\{name}{suffix}"))
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    }

    /// What happened when trying to take the session-wide name.
    pub enum Acquired {
        /// We own it; both handles must stay open for the process lifetime.
        Primary(Handle, Handle),
        /// Another process already owns it.
        AlreadyRunning,
        /// The kernel refused to create the objects (resource exhaustion).
        Unavailable,
    }

    pub fn acquire_mutex_and_event(name: &str) -> Acquired {
        let mutex_name = object_name(name, "");
        // SAFETY: `ptr::null()` is the documented "default security" argument and
        // the name is a NUL-terminated wide string that outlives the call.
        let mutex = unsafe { CreateMutexW(ptr::null(), 0, mutex_name.as_ptr()) };
        if mutex.is_null() {
            return Acquired::Unavailable;
        }
        // SAFETY: reading the thread-local last error immediately after the call.
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            drop(Handle(mutex));
            return Acquired::AlreadyRunning;
        }

        let event_name = object_name(name, ".Activate");
        // SAFETY: as above; auto-reset, initially unsignalled.
        let event = unsafe { CreateEventW(ptr::null(), 0, 0, event_name.as_ptr()) };
        if event.is_null() {
            drop(Handle(mutex));
            return Acquired::Unavailable;
        }
        Acquired::Primary(Handle(mutex), Handle(event))
    }

    /// Handle the listener waits on.
    ///
    /// `CreateEventW` rather than `OpenEventW`: opening with `EVENT_MODIFY_STATE`
    /// alone does not grant `SYNCHRONIZE`, so `WaitForSingleObject` would fail on
    /// that handle. The object already exists (the primary created it before
    /// spawning the listener), so this only opens it, with full access.
    pub fn open_for_waiting(name: &str) -> Option<Handle> {
        let event_name = object_name(name, ".Activate");
        // SAFETY: null attributes, NUL-terminated name that outlives the call.
        let event = unsafe { CreateEventW(ptr::null(), 0, 0, event_name.as_ptr()) };
        (!event.is_null()).then_some(Handle(event))
    }

    /// Handle used to set the event; only needs `EVENT_MODIFY_STATE`.
    fn open_for_signal(name: &str) -> Option<Handle> {
        let event_name = object_name(name, ".Activate");
        // SAFETY: same contract as above.
        let event = unsafe { OpenEventW(EVENT_MODIFY_STATE, 0, event_name.as_ptr()) };
        (!event.is_null()).then_some(Handle(event))
    }

    /// `true` when the wait was satisfied, i.e. another launch asked for us.
    pub fn wait(handle: &Handle, millis: u32) -> bool {
        // SAFETY: the handle is open for the lifetime of the borrow.
        unsafe { WaitForSingleObject(handle.0, millis) == WAIT_OBJECT_0 }
    }

    pub fn set(handle: &Handle) {
        // SAFETY: as above; the return value only reports whether it was open.
        unsafe {
            SetEvent(handle.0);
        }
    }

    /// Signal the running instance, if one is listening.
    pub fn signal(name: &str) -> bool {
        let Some(event) = open_for_signal(name) else {
            return false;
        };
        // SAFETY: `event` is an open handle for this statement.
        unsafe { SetEvent(event.0) != 0 }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    /// Names must be unique per test: the kernel objects are machine-wide within
    /// the session, and the test harness runs these in parallel.
    fn unique(test: &str) -> String {
        format!("RustVNC.Test.{}.{test}", std::process::id())
    }

    #[test]
    fn a_second_acquire_reports_already_running() {
        let name = unique("second");
        let _primary = SingleInstance::acquire(&name);
        assert!(_primary.is_primary());
        let second = SingleInstance::acquire(&name);
        assert!(!second.is_primary(), "the name is still held");
    }

    #[test]
    fn dropping_the_guard_frees_the_name() {
        let name = unique("release");
        {
            let primary = SingleInstance::acquire(&name);
            assert!(primary.is_primary());
        }
        let again = SingleInstance::acquire(&name);
        assert!(again.is_primary(), "the previous owner has gone away");
    }

    #[test]
    fn signalling_raises_the_primary() {
        let name = unique("raise");
        let primary = SingleInstance::acquire(&name);
        let handle = primary.handle();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        handle.set_waker(move || {
            counter.fetch_add(1, Ordering::Relaxed);
        });

        assert!(signal_existing(&name), "a primary exists");
        // The wake callback runs on the listener thread.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while seen.load(Ordering::Relaxed) == 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(seen.load(Ordering::Relaxed), 1, "the waker must run once");
        assert_eq!(handle.raise_count(), 1);
    }

    #[test]
    fn signalling_without_a_primary_is_harmless() {
        let name = unique("nobody");
        assert!(!signal_existing(&name));
    }
}
