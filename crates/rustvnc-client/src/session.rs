use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use vnc::{Rect, Screen, VncConnector, VncEvent, X11Event};

use crate::config::{AuthMode, ClientConfig, SecurityMode};
use crate::ssh::SshTunnel;

#[derive(Debug)]
pub enum SessionCommand {
    Input(X11Event),
    /// Keep the connection alive but stop polling for frames and drop updates.
    /// Used when a tab is in the background so inactive sessions do not queue
    /// megabytes of frame data.
    Pause(bool),
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionErrorCode {
    Auth,
    Network,
    Protocol,
    Config,
    Ssh,
    Internal,
}

impl SessionErrorCode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Auth => "AUTH",
            Self::Network => "NETWORK",
            Self::Protocol => "PROTOCOL",
            Self::Config => "CONFIG",
            Self::Ssh => "SSH",
            Self::Internal => "INTERNAL",
        }
    }
}

impl std::fmt::Display for SessionErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Debug)]
struct SessionFailure {
    code: SessionErrorCode,
    message: String,
    retryable: bool,
    connected: bool,
}

impl SessionFailure {
    fn new(code: SessionErrorCode, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            code,
            message: message.into(),
            retryable,
            connected: false,
        }
    }

    fn after_connect(mut self) -> Self {
        self.connected = true;
        self
    }
}

#[derive(Debug, Clone)]
pub enum SessionEvent {
    Connected {
        name: String,
        width: usize,
        height: usize,
        bpp: u8,
    },
    Resized {
        width: usize,
        height: usize,
    },
    RawImage {
        rect: Rect,
        data: Vec<u8>,
    },
    Copy {
        dst: Rect,
        src: Rect,
    },
    /// One `FramebufferUpdate` has been fully decoded: the UI may present.
    FrameComplete,
    /// The remote pointer shape. `size` is zero when the server clears it, in
    /// which case `rgba` is empty.
    Cursor {
        hotspot: (u16, u16),
        size: (u16, u16),
        /// BGRA with an alpha byte, matching the negotiated 32 bpp format.
        rgba: Vec<u8>,
    },
    Clipboard(String),
    Stats {
        fps: f64,
        rects: u64,
        bytes: u64,
        /// True while the request cadence is backed off because the screen is
        /// static (the HUD shows it so a low fps is not mistaken for a problem).
        backed_off: bool,
        /// Measured request → fully-decoded-frame latency.
        latency_ms: f64,
        /// Current request interval after adaptive pacing.
        adaptive_interval_ms: u64,
        /// True when the controller has slowed down because latency is high.
        degraded: bool,
    },
    Error {
        code: SessionErrorCode,
        message: String,
        retryable: bool,
    },
    Reconnecting {
        attempt: u32,
        delay_ms: u64,
    },
    Closed,
}

pub struct Session {
    commands: UnboundedSender<SessionCommand>,
    events: Receiver<SessionEvent>,
    thread: Option<JoinHandle<()>>,
    local_addr: String,
    /// Set by [`Session::close`] so a worker blocked in SSH startup (up to ten
    /// seconds of polling) abandons it instead of outliving the closed tab.
    cancel: Arc<AtomicBool>,
}

impl Session {
    pub fn start(config: ClientConfig, password: String) -> Result<Self> {
        // Nothing here may touch the network. Starting an SSH tunnel spawns
        // `ssh` and waits for the forwarded port, and this runs on the UI
        // thread: doing that work here is what froze the window on Connect.
        let local_addr = format!("{}:{}", config.host, config.port);
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = Arc::clone(&cancel);

        let (command_tx, command_rx) = unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("rustvnc-client-session".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let _ = event_tx.send(SessionEvent::Error {
                            code: SessionErrorCode::Internal,
                            message: format!("create tokio runtime: {err}"),
                            retryable: false,
                        });
                        return;
                    }
                };
                runtime.block_on(run_session(
                    config,
                    password,
                    command_rx,
                    event_tx,
                    cancel_for_thread,
                ));
            })
            .context("spawn session thread")?;

        Ok(Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
            local_addr,
            cancel,
        })
    }

    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }

    pub fn try_recv(&self) -> Option<SessionEvent> {
        self.events.try_recv().ok()
    }

    pub fn send_input(&self, event: X11Event) {
        let _ = self.commands.send(SessionCommand::Input(event));
    }

    pub fn set_paused(&self, paused: bool) {
        let _ = self.commands.send(SessionCommand::Pause(paused));
    }

    /// Ask the session thread to stop.
    ///
    /// This never joins: the worker may still be blocked in a connect or a
    /// handshake, and joining from the UI thread would freeze the window for as
    /// long as that network call takes. The handle is detached instead; the
    /// worker exits on its own once its current await completes or times out.
    pub fn close(&mut self) {
        // Release the worker before the Close command can be observed: it may be
        // sitting in the SSH startup poll, which only checks this flag.
        self.cancel.store(true, Ordering::Relaxed);
        let _ = self.commands.send(SessionCommand::Close);
        if let Some(thread) = self.thread.take() {
            drop(thread);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.commands.send(SessionCommand::Close);
    }
}

/// How long a TCP connect may take before the attempt is abandoned.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// How long the RFB handshake (version, security, init) may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// A session that streamed for at least this long counts as healthy, so the
/// next failure starts the backoff over. Shorter-lived connections keep
/// climbing the backoff instead of retrying once per second forever.
const STABLE_SESSION: Duration = Duration::from_secs(10);
/// Give up waiting for an in-flight frame after this long and request again.
///
/// Deliberately not derived from the frame interval: a 5K ZRLE frame over a slow
/// link can legitimately take far longer than a few frames, and re-arming early
/// would silently restore the overlapping-request behaviour this guards against.
const FRAME_TIMEOUT: Duration = Duration::from_secs(1);

/// Keeps the request cadence sensible: the configured rate while the screen is
/// changing, a slow trickle while it is static.
///
/// A 5K desktop that never changes still costs the server a full render per
/// request, so polling it 30 times a second is wasted work on both ends. Any
/// frame that carried pixels snaps straight back to the active rate, so the
/// backoff is invisible when the user is actually doing something.
struct FramePacer {
    base: Duration,
    idle: Duration,
    current: Duration,
    quiet_frames: u8,
    adaptive: bool,
    adaptive_interval: Duration,
    latency_ema: Option<Duration>,
    high_streak: u8,
    low_streak: u8,
}

/// Consecutive pixel-less frames before backing off. Three frames is enough to
/// tell "nothing is happening" from "a single redraw finished".
const QUIET_FRAMES_BEFORE_BACKOFF: u8 = 3;
/// Slowest the backoff goes, regardless of how high the configured rate is.
const MAX_IDLE_INTERVAL: Duration = Duration::from_millis(250);
/// Sustained latency above this reduces the request rate.
const HIGH_LATENCY: Duration = Duration::from_millis(200);
/// Sustained latency below this restores it.
const LOW_LATENCY: Duration = Duration::from_millis(80);
/// Hard ceiling for adaptive slowdown (still yields 2 fps).
const MAX_ADAPTIVE_INTERVAL: Duration = Duration::from_millis(500);
/// Frames above/below the threshold before acting; hysteresis keeps the
/// controller from oscillating around a single boundary.
const HIGH_STREAK: u8 = 3;
const LOW_STREAK: u8 = 60;

impl FramePacer {
    fn new(fps: u32, adaptive: bool) -> Self {
        let base = Duration::from_millis(1000 / u64::from(fps.max(1)));
        Self {
            base,
            idle: base.max(MAX_IDLE_INTERVAL),
            current: base,
            quiet_frames: 0,
            adaptive,
            adaptive_interval: base,
            latency_ema: None,
            high_streak: 0,
            low_streak: 0,
        }
    }

    /// Snap back to the active cadence — the adaptive interval, not the
    /// configured one, so a measured slowdown is kept.
    fn note_activity(&mut self) {
        self.quiet_frames = 0;
        self.current = self.adaptive_interval.max(self.base);
    }

    fn on_frame(&mut self, had_content: bool, latency: Duration) {
        self.observe_latency(latency);
        if had_content {
            self.note_activity();
            return;
        }
        self.quiet_frames = self.quiet_frames.saturating_add(1);
        if self.quiet_frames >= QUIET_FRAMES_BEFORE_BACKOFF {
            self.current = self.idle.max(self.adaptive_interval);
        }
    }

    fn observe_latency(&mut self, latency: Duration) {
        let ema = match self.latency_ema {
            Some(previous) => previous.mul_f64(0.8) + latency.mul_f64(0.2),
            None => latency,
        };
        self.latency_ema = Some(ema);
        if !self.adaptive {
            return;
        }
        if ema >= HIGH_LATENCY {
            self.low_streak = 0;
            self.high_streak = self.high_streak.saturating_add(1);
            if self.high_streak >= HIGH_STREAK {
                let next = self
                    .adaptive_interval
                    .mul_f64(1.25)
                    .min(MAX_ADAPTIVE_INTERVAL);
                if next > self.adaptive_interval {
                    self.adaptive_interval = next.max(self.base);
                }
                self.high_streak = 0;
            }
        } else if ema <= LOW_LATENCY {
            self.high_streak = 0;
            self.low_streak = self.low_streak.saturating_add(1);
            if self.low_streak >= LOW_STREAK {
                let next = self.adaptive_interval.mul_f64(0.9).max(self.base);
                if next < self.adaptive_interval {
                    self.adaptive_interval = next;
                }
                self.low_streak = 0;
            }
        } else {
            self.high_streak = 0;
            self.low_streak = 0;
        }
    }

    fn delay(&self) -> Duration {
        self.current
    }

    fn latency(&self) -> Option<Duration> {
        self.latency_ema
    }

    fn adaptive_interval(&self) -> Duration {
        self.adaptive_interval
    }

    fn is_degraded(&self) -> bool {
        self.adaptive_interval > self.base
    }

    fn is_backed_off(&self) -> bool {
        self.current > self.base
    }
}
async fn run_session(
    config: ClientConfig,
    password: String,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: Sender<SessionEvent>,
    cancel: Arc<AtomicBool>,
) {
    // Opening the tunnel spawns `ssh` and polls for the forwarded port, so it
    // belongs on this worker thread: the caller only spawns a thread and
    // returns, which is what keeps Connect from freezing the window. The
    // binding lives for the whole session — dropping it kills the ssh child.
    let tunnel = match open_tunnel(&config, &cancel) {
        Ok(tunnel) => tunnel,
        // Closed while `ssh` was still starting: not an error the user asked to
        // see, and there is nobody left to show it to.
        Err(_) if cancel.load(Ordering::Relaxed) => return,
        Err(failure) => {
            let _ = events.send(SessionEvent::Error {
                code: failure.code,
                message: failure.message,
                retryable: false,
            });
            let _ = events.send(SessionEvent::Closed);
            return;
        }
    };
    let target = match &tunnel {
        Some(tunnel) => tunnel.local_addr().to_string(),
        None => format!("{}:{}", config.host, config.port),
    };

    let mut attempt = 0u32;
    let mut paused = false;
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = events.send(SessionEvent::Closed);
            break;
        }
        let started = Instant::now();
        match connect_and_loop(
            target.clone(),
            config.clone(),
            password.clone(),
            &mut commands,
            &mut paused,
            &events,
        )
        .await
        {
            Ok(()) => {
                let _ = events.send(SessionEvent::Closed);
                break;
            }
            Err(failure) => {
                let streamed = started.elapsed();
                let _ = events.send(SessionEvent::Error {
                    code: failure.code,
                    message: failure.message.clone(),
                    retryable: failure.retryable,
                });
                if !config.auto_reconnect || !failure.retryable {
                    let _ = events.send(SessionEvent::Closed);
                    break;
                }
                if failure.connected && streamed >= STABLE_SESSION {
                    attempt = 0;
                }
                attempt = attempt.saturating_add(1);
                let delay = backoff(attempt);
                let _ = events.send(SessionEvent::Reconnecting {
                    attempt,
                    delay_ms: delay.as_millis() as u64,
                });
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    command = commands.recv() => {
                        if matches!(command, Some(SessionCommand::Close) | None) {
                            let _ = events.send(SessionEvent::Closed);
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Open the SSH tunnel when the profile asks for one.
///
/// Called from [`run_session`], i.e. on the worker thread. A tunnel failure is
/// not retryable: the usual causes (missing host key, wrong user, no `ssh`) do
/// not fix themselves, and retrying would hide the message behind a backoff.
fn open_tunnel(
    config: &ClientConfig,
    cancel: &AtomicBool,
) -> std::result::Result<Option<SshTunnel>, SessionFailure> {
    if !matches!(config.security, SecurityMode::Ssh) {
        return Ok(None);
    }
    SshTunnel::start_with_cancel(&config.ssh, &config.host, config.port, cancel)
        .map(Some)
        .map_err(|err| SessionFailure::new(SessionErrorCode::Network, format!("{err:#}"), false))
}

fn backoff(attempt: u32) -> Duration {
    let seconds = 1u64 << attempt.saturating_sub(1).min(4);
    Duration::from_secs(seconds.min(15))
}

async fn connect_and_loop(
    target: String,
    config: ClientConfig,
    password: String,
    commands: &mut UnboundedReceiver<SessionCommand>,
    paused: &mut bool,
    events: &Sender<SessionEvent>,
) -> std::result::Result<(), SessionFailure> {
    if config.auth == AuthMode::Ard && config.user.trim().is_empty() {
        return Err(SessionFailure::new(
            SessionErrorCode::Config,
            "ARD auth requires a macOS user name",
            false,
        ));
    }
    if config.auth == AuthMode::Vnc && password.is_empty() {
        return Err(SessionFailure::new(
            SessionErrorCode::Auth,
            "VNC auth requires a password",
            false,
        ));
    }

    let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(&target))
        .await
        .map_err(|_| {
            SessionFailure::new(
                SessionErrorCode::Network,
                format!(
                    "connect to {target} timed out after {}s (firewall, wrong port, or host asleep)",
                    CONNECT_TIMEOUT.as_secs()
                ),
                true,
            )
        })?
        .map_err(|err| {
            SessionFailure::new(
                SessionErrorCode::Network,
                format!("connect to {target}: {err}"),
                true,
            )
        })?;
    let mut connector = VncConnector::new(tcp)
        .allow_shared(config.shared)
        .set_pixel_format(config.pixel_format());

    for encoding in config.encodings() {
        connector = connector.add_encoding(encoding);
    }

    if config.auth != AuthMode::Vnc && !config.user.trim().is_empty() {
        connector = connector.set_ard_credentials(config.user.clone(), password.clone());
    }
    if config.auth != AuthMode::Ard && !password.is_empty() {
        let password = password.clone();
        connector = connector.set_auth_method(async move { Ok(password) });
    }

    let connector = connector.build().map_err(|err| {
        SessionFailure::new(
            SessionErrorCode::Config,
            format!("build RFB connector: {err}"),
            false,
        )
    })?;

    // The handshake can hang forever if the peer accepts TCP and then stays
    // silent (wrong port behind a firewall, wedged screensharingd). Bound it.
    let client = tokio::time::timeout(HANDSHAKE_TIMEOUT, async move {
        let started = connector
            .try_start()
            .await
            .map_err(SessionFailure::from_vnc)?;
        started.finish().map_err(|err| {
            SessionFailure::new(
                SessionErrorCode::Config,
                format!("finish RFB client: {err}"),
                false,
            )
        })
    })
    .await
    .map_err(|_| {
        SessionFailure::new(
            SessionErrorCode::Network,
            format!(
                "RFB handshake with {target} timed out after {}s",
                HANDSHAKE_TIMEOUT.as_secs()
            ),
            true,
        )
    })??;

    let mut width = 0usize;
    let mut height = 0usize;
    let mut rects = 0u64;
    let mut bytes = 0u64;
    // Frames, not rectangles: with 5K ZRLE a single update contains dozens of
    // rectangles, so counting them reported absurd numbers like "4619 fps".
    let mut frames = 0u64;
    let mut last_frames = 0u64;
    let mut last_stats = Instant::now();
    let mut pacer = FramePacer::new(config.fps(), config.adaptive);
    // Exactly one FramebufferUpdateRequest may be outstanding: sending the next
    // one while the previous update is still streaming makes the server interleave
    // updates, which shows up as tearing / jelly while windows move.
    let mut update_in_flight = false;
    let mut in_flight_since = Instant::now();
    // Whether the frame currently being decoded carried any pixels; used to
    // decide between the active and the idle cadence.
    let mut frame_had_content = false;
    // Input arrived while a frame was in flight: request the next one the moment
    // the channel frees up instead of waiting for the tick.
    let mut refresh_after_input = false;
    let mut next_request = tokio::time::Instant::now();

    let _ = events.send(SessionEvent::Connected {
        name: client.server_name().to_string(),
        width,
        height,
        bpp: config.bpp,
    });

    loop {
        tokio::select! {
            command = commands.recv() => {
                match command {
                    Some(SessionCommand::Pause(value)) => {
                        *paused = value;
                        if !*paused {
                            update_in_flight = false;
                            next_request = tokio::time::Instant::now();
                            if let Err(err) = client.input(X11Event::FullRefresh).await {
                                let failure = SessionFailure::from_vnc(err).after_connect();
                                return Err(failure);
                            }
                        }
                    }
                    Some(SessionCommand::Input(event)) => {
                        if *paused {
                            continue;
                        }
                        if let Err(err) = client.input(event).await {
                            let failure = SessionFailure::from_vnc(err).after_connect();
                            let _ = events.send(SessionEvent::Error {
                                code: failure.code,
                                message: failure.message.clone(),
                                retryable: failure.retryable,
                            });
                            return Err(failure);
                        }
                        // Typing or clicking almost always repaints the remote
                        // screen, so ask for the result immediately instead of
                        // waiting for the next scheduled poll (up to a quarter of
                        // a second when idle, or a full interval while streaming).
                        // The single-in-flight guard keeps this from flooding.
                        pacer.note_activity();
                        if update_in_flight {
                            refresh_after_input = true;
                        } else {
                            update_in_flight = true;
                            in_flight_since = Instant::now();
                            let _ = client.input(X11Event::Refresh).await;
                        }
                        next_request = tokio::time::Instant::now() + pacer.delay();
                    }
                    Some(SessionCommand::Close) | None => break,
                }
            }
            // Blocks until the engine has an event. `poll_event()` would return
            // `Ok(None)` immediately and turn this select into a busy loop that
            // burns a full core whenever the screen is idle.
            event = client.recv_event() => {
                match event {
                    Ok(event) => {
                        if *paused && !matches!(event, VncEvent::Error(_)) {
                            continue;
                        }
                        match event {
                        VncEvent::SetResolution(Screen { width: new_width, height: new_height }) => {
                            width = new_width as usize;
                            height = new_height as usize;
                            let _ = events.send(SessionEvent::Resized { width, height });
                        }
                        VncEvent::RawImage(rect, data) => {
                            frame_had_content = true;
                            rects += 1;
                            bytes += data.len() as u64;
                            let _ = events.send(SessionEvent::RawImage { rect, data });
                        }
                        VncEvent::Copy(dst, src) => {
                            frame_had_content = true;
                            rects += 1;
                            let _ = events.send(SessionEvent::Copy { dst, src });
                        }
                        VncEvent::UpdateComplete => {
                            update_in_flight = false;
                            // Measure before anything below can reset the clock.
                            let latency = in_flight_since.elapsed();
                            // An input that arrived while this frame was in
                            // flight could not be reflected in it, so ask for the
                            // next one right now instead of waiting for the tick.
                            if refresh_after_input {
                                refresh_after_input = false;
                                update_in_flight = true;
                                in_flight_since = Instant::now();
                                let _ = client.input(X11Event::Refresh).await;
                                next_request = tokio::time::Instant::now() + pacer.delay();
                            }
                            pacer.on_frame(frame_had_content, latency);
                            frame_had_content = false;
                            frames += 1;
                            let _ = events.send(SessionEvent::FrameComplete);
                        }
                        VncEvent::SetCursor(rect, rgba) => {
                            let _ = events.send(SessionEvent::Cursor {
                                hotspot: (rect.x, rect.y),
                                size: (rect.width, rect.height),
                                rgba,
                            });
                        }
                        VncEvent::Bell => {}
                        VncEvent::AppleEncoding { code, rect, body } => {
                            // Parsed and bounded by the engine. Surfaced in the
                            // diagnostics log: the layout/machine are exactly what
                            // a support ticket needs, and neither is predictable
                            // from the host name.
                            match vnc::apple::parse(code, &body) {
                                vnc::apple::AppleInfo::Machine(model) => {
                                    tracing::info!(%model, "remote Mac model");
                                }
                                vnc::apple::AppleInfo::Keyboard(layout) => {
                                    tracing::info!(%layout, "remote keyboard layout");
                                }
                                vnc::apple::AppleInfo::Capabilities(entries) => {
                                    tracing::debug!(count = entries.len(), "apple capabilities");
                                }
                                vnc::apple::AppleInfo::Cursor => {
                                    tracing::info!(
                                        hotspot_x = rect.x,
                                        hotspot_y = rect.y,
                                        width = rect.width,
                                        height = rect.height,
                                        "remote cursor geometry"
                                    );
                                }
                                vnc::apple::AppleInfo::Display {
                                    pixel_width,
                                    pixel_height,
                                    scale,
                                    display_id,
                                    ..
                                } => {
                                    tracing::info!(
                                        pixel_width,
                                        pixel_height,
                                        scale,
                                        display_id,
                                        "remote display geometry"
                                    );
                                }
                                other => tracing::debug!(
                                    code,
                                    width = rect.width,
                                    height = rect.height,
                                    bytes = body.len(),
                                    ?other,
                                    "apple pseudo-encoding"
                                ),
                            }
                        }
                        VncEvent::Text(text) => {
                            let _ = events.send(SessionEvent::Clipboard(text));
                        }
                        VncEvent::Error(err) => {
                            return Err(SessionFailure::from_message(err).after_connect());
                        }
                        _ => {}
                        }
                    }
                    Err(vnc::VncError::ClientNotRunning) => {
                        // The engine stopped: the peer closed the connection or
                        // the decoder task ended. Treat it as a recoverable drop
                        // so auto-reconnect actually reconnects.
                        let failure = SessionFailure::new(
                            SessionErrorCode::Network,
                            "connection closed by peer",
                            true,
                        )
                        .after_connect();
                        let _ = events.send(SessionEvent::Error {
                            code: failure.code,
                            message: failure.message.clone(),
                            retryable: failure.retryable,
                        });
                        return Err(failure);
                    }
                    Err(err) => {
                        let failure = SessionFailure::from_vnc(err).after_connect();
                        let _ = events.send(SessionEvent::Error {
                            code: failure.code,
                            message: failure.message.clone(),
                            retryable: failure.retryable,
                        });
                        return Err(failure);
                    }
                }
            }
            _ = tokio::time::sleep_until(next_request) => {
                if *paused {
                    next_request = tokio::time::Instant::now() + Duration::from_millis(250);
                    continue;
                }
                // A lost update must not stall the session forever: after a few
                // intervals without the marker, re-arm and try again.
                if update_in_flight && in_flight_since.elapsed() > FRAME_TIMEOUT {
                    tracing::debug!("frame did not complete in time; re-arming refresh");
                    update_in_flight = false;
                }
                next_request = tokio::time::Instant::now() + pacer.delay();
                if !update_in_flight {
                    update_in_flight = true;
                    in_flight_since = Instant::now();
                    if let Err(err) = client.input(X11Event::Refresh).await {
                        let failure = SessionFailure::from_vnc(err).after_connect();
                        let _ = events.send(SessionEvent::Error {
                            code: failure.code,
                            message: failure.message.clone(),
                            retryable: failure.retryable,
                        });
                        return Err(failure);
                    }
                }
                if last_stats.elapsed() >= Duration::from_secs(1) {
                    let elapsed = last_stats.elapsed().as_secs_f64().max(0.001);
                    let fps = (frames - last_frames) as f64 / elapsed;
                    let _ = events.send(SessionEvent::Stats {
                        fps,
                        rects,
                        bytes,
                        backed_off: pacer.is_backed_off(),
                        latency_ms: pacer
                            .latency()
                            .map(|latency| latency.as_secs_f64() * 1000.0)
                            .unwrap_or(0.0),
                        adaptive_interval_ms: pacer.adaptive_interval().as_millis() as u64,
                        degraded: pacer.is_degraded(),
                    });
                    last_stats = Instant::now();
                    last_frames = frames;
                }
            }
        }
    }

    let _ = client.close().await;
    Ok(())
}

impl SessionFailure {
    fn from_message(message: String) -> Self {
        // The engine's decoder reports unsupported encodings as text; that is a
        // permanent condition, so do not let auto-reconnect spin on it.
        if message.contains("does not implement") {
            return Self::new(SessionErrorCode::Protocol, message, false);
        }
        let retryable = message.contains("server message")
            || message.contains("Image data")
            || message.contains("image data");
        Self::new(
            if retryable {
                SessionErrorCode::Protocol
            } else {
                SessionErrorCode::Internal
            },
            message,
            retryable,
        )
    }

    fn from_vnc(err: vnc::VncError) -> Self {
        use vnc::VncError;
        match err {
            VncError::NoPassword
            | VncError::WrongPassword
            | VncError::ArdAuthFailed(_)
            | VncError::ArdNotOffered
            | VncError::ArdVersionConflict(_)
            | VncError::ArdProtocol(_)
            | VncError::InvalidSecurityTyep(_) => {
                Self::new(SessionErrorCode::Auth, err.to_string(), false)
            }
            VncError::IoError(_) => Self::new(SessionErrorCode::Network, err.to_string(), true),
            VncError::ConnectError => Self::new(SessionErrorCode::Network, err.to_string(), true),
            VncError::WrongServerMessage | VncError::InvalidImageData => {
                Self::new(SessionErrorCode::Protocol, err.to_string(), true)
            }
            VncError::NoEncoding | VncError::WrongPixelFormat => {
                Self::new(SessionErrorCode::Protocol, err.to_string(), false)
            }
            // Retrying cannot help: the Mac's resolution is simply above what
            // this client will allocate. Say so, with the numbers.
            VncError::FramebufferTooLarge {
                width,
                height,
                pixels,
                max_pixels,
                max_dimension,
            } => Self::new(
                SessionErrorCode::Protocol,
                format!(
                    "远端分辨率 {width}×{height}（{pixels} 像素）超过客户端上限 \
                     （{max_pixels} 像素 / 单边 {max_dimension}）；\
                     请在 Mac 上把「显示器 → 分辨率」调低（或使用缩放分辨率）后重连"
                ),
                false,
            ),
            VncError::UnsupportedEncoding(code) => Self::new(
                SessionErrorCode::Protocol,
                format!("服务端使用了不支持的编码 {code} (unsupported encoding)"),
                false,
            ),
            // Not "the client is broken": either the peer went away or the
            // engine was shut down. Reconnecting is the right response.
            VncError::ClientNotRunning => {
                Self::new(SessionErrorCode::Network, err.to_string(), true)
            }
            VncError::General(message)
                if message.contains("Channel closed") || message.contains("channel closed") =>
            {
                Self::new(SessionErrorCode::Network, message, true)
            }
            _ => Self::new(SessionErrorCode::Internal, err.to_string(), false),
        }
    }
}

#[cfg(test)]
mod pacer_tests {
    use super::*;

    #[test]
    fn high_latency_backs_off_and_low_latency_recovers() {
        let mut pacer = FramePacer::new(30, true);
        let base = pacer.delay();
        for _ in 0..6 {
            pacer.on_frame(true, Duration::from_millis(400));
        }
        assert!(pacer.delay() > base, "high latency must slow requests");
        assert!(pacer.is_degraded());

        for _ in 0..600 {
            pacer.on_frame(true, Duration::from_millis(20));
        }
        assert_eq!(pacer.delay(), base, "sustained low latency must recover");
        assert!(!pacer.is_degraded());
    }

    #[test]
    fn hysteresis_does_not_oscillate_near_the_threshold() {
        let mut pacer = FramePacer::new(30, true);
        // Repeatedly hover between the low and high thresholds.
        for _ in 0..120 {
            pacer.on_frame(true, Duration::from_millis(120));
        }
        assert_eq!(pacer.delay(), Duration::from_millis(33));
    }

    #[test]
    fn disabling_adaptive_keeps_the_configured_rate() {
        let mut pacer = FramePacer::new(30, false);
        for _ in 0..120 {
            pacer.on_frame(true, Duration::from_millis(1000));
        }
        assert_eq!(pacer.delay(), Duration::from_millis(33));
        assert!(!pacer.is_degraded());
    }

    #[test]
    fn static_frames_still_use_the_idle_cadence() {
        let mut pacer = FramePacer::new(30, true);
        for _ in 0..QUIET_FRAMES_BEFORE_BACKOFF {
            pacer.on_frame(false, Duration::from_millis(20));
        }
        assert_eq!(pacer.delay(), MAX_IDLE_INTERVAL);
    }
}
