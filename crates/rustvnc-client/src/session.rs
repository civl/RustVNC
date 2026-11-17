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
    Clipboard(String),
    Stats {
        fps: f64,
        rects: u64,
        bytes: u64,
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
    _tunnel: Option<SshTunnel>,
}

impl Session {
    pub fn start(config: ClientConfig, password: String) -> Result<Self> {
        let (target, tunnel) = match config.security {
            SecurityMode::Direct => (format!("{}:{}", config.host, config.port), None),
            SecurityMode::Ssh => {
                let tunnel = SshTunnel::start(&config.ssh, &config.host, config.port)?;
                (tunnel.local_addr().to_string(), Some(tunnel))
            }
        };

        let target_for_thread = target.clone();
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
                    target_for_thread,
                    config,
                    password,
                    command_rx,
                    event_tx,
                ));
            })
            .context("spawn session thread")?;

        Ok(Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
            local_addr: target,
            _tunnel: tunnel,
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

    /// Ask the session thread to stop.
    ///
    /// This never joins: the worker may still be blocked in a connect or a
    /// handshake, and joining from the UI thread would freeze the window for as
    /// long as that network call takes. The handle is detached instead; the
    /// worker exits on its own once its current await completes or times out.
    pub fn close(&mut self) {
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

async fn run_session(
    target: String,
    config: ClientConfig,
    password: String,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: Sender<SessionEvent>,
) {
    let mut attempt = 0u32;
    loop {
        let started = Instant::now();
        match connect_and_loop(
            target.clone(),
            config.clone(),
            password.clone(),
            &mut commands,
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

fn backoff(attempt: u32) -> Duration {
    let seconds = 1u64 << attempt.saturating_sub(1).min(4);
    Duration::from_secs(seconds.min(15))
}

async fn connect_and_loop(
    target: String,
    config: ClientConfig,
    password: String,
    commands: &mut UnboundedReceiver<SessionCommand>,
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
    let mut last_stats = Instant::now();
    let mut last_rects = 0u64;
    let interval = Duration::from_millis(1000 / u64::from(config.fps().max(1)));
    // Exactly one FramebufferUpdateRequest may be outstanding: sending the next
    // one while the previous update is still streaming makes the server interleave
    // updates, which shows up as tearing / jelly while windows move.
    let mut update_in_flight = false;
    let mut in_flight_since = Instant::now();
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

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
                    Some(SessionCommand::Input(event)) => {
                        if let Err(err) = client.input(event).await {
                            let failure = SessionFailure::from_vnc(err).after_connect();
                            let _ = events.send(SessionEvent::Error {
                                code: failure.code,
                                message: failure.message.clone(),
                                retryable: failure.retryable,
                            });
                            return Err(failure);
                        }
                    }
                    Some(SessionCommand::Close) | None => break,
                }
            }
            // Blocks until the engine has an event. `poll_event()` would return
            // `Ok(None)` immediately and turn this select into a busy loop that
            // burns a full core whenever the screen is idle.
            event = client.recv_event() => {
                match event {
                    Ok(event) => match event {
                        VncEvent::SetResolution(Screen { width: new_width, height: new_height }) => {
                            width = new_width as usize;
                            height = new_height as usize;
                            let _ = events.send(SessionEvent::Resized { width, height });
                        }
                        VncEvent::RawImage(rect, data) => {
                            rects += 1;
                            bytes += data.len() as u64;
                            let _ = events.send(SessionEvent::RawImage { rect, data });
                        }
                        VncEvent::Copy(dst, src) => {
                            rects += 1;
                            let _ = events.send(SessionEvent::Copy { dst, src });
                        }
                        VncEvent::UpdateComplete => {
                            update_in_flight = false;
                        }
                        VncEvent::SetCursor(_, _) | VncEvent::Bell => {}
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
                    },
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
            _ = ticker.tick() => {
                // A lost update must not stall the session forever: after a few
                // intervals without the marker, re-arm and try again.
                if update_in_flight && in_flight_since.elapsed() > FRAME_TIMEOUT {
                    tracing::debug!("frame did not complete in time; re-arming refresh");
                    update_in_flight = false;
                }
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
                    let fps = (rects - last_rects) as f64 / elapsed;
                    let _ = events.send(SessionEvent::Stats { fps, rects, bytes });
                    last_stats = Instant::now();
                    last_rects = rects;
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
            } => Self::new(
                SessionErrorCode::Protocol,
                format!(
                    "远端分辨率 {width}×{height}（{pixels} 像素）超过客户端上限 {max_pixels} 像素；\
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
            _ => Self::new(SessionErrorCode::Internal, err.to_string(), false),
        }
    }
}
