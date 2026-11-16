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

    pub fn close(&mut self) {
        let _ = self.commands.send(SessionCommand::Close);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.commands.send(SessionCommand::Close);
    }
}

async fn run_session(
    target: String,
    config: ClientConfig,
    password: String,
    mut commands: UnboundedReceiver<SessionCommand>,
    events: Sender<SessionEvent>,
) {
    let mut attempt = 0u32;
    loop {
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
                let _ = events.send(SessionEvent::Error {
                    code: failure.code,
                    message: failure.message.clone(),
                    retryable: failure.retryable,
                });
                if failure.connected {
                    attempt = 0;
                }
                if !config.auto_reconnect || !failure.retryable {
                    let _ = events.send(SessionEvent::Closed);
                    break;
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

    let tcp = TcpStream::connect(&target).await.map_err(|err| {
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

    let client = connector
        .build()
        .map_err(|err| {
            SessionFailure::new(
                SessionErrorCode::Config,
                format!("build RFB connector: {err}"),
                false,
            )
        })?
        .try_start()
        .await
        .map_err(SessionFailure::from_vnc)?
        .finish()
        .map_err(|err| {
            SessionFailure::new(
                SessionErrorCode::Config,
                format!("finish RFB client: {err}"),
                false,
            )
        })?;

    let mut width = 0usize;
    let mut height = 0usize;
    let mut rects = 0u64;
    let mut bytes = 0u64;
    let mut last_stats = Instant::now();
    let mut last_rects = 0u64;
    let interval = Duration::from_millis(1000 / u64::from(config.fps().max(1)));
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
                        if client.input(event).await.is_err() {
                            break;
                        }
                    }
                    Some(SessionCommand::Close) | None => break,
                }
            }
            event = client.poll_event() => {
                match event {
                    Ok(Some(event)) => match event {
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
                        VncEvent::SetCursor(_, _) | VncEvent::Bell => {}
                        VncEvent::Text(text) => {
                            let _ = events.send(SessionEvent::Clipboard(text));
                        }
                        VncEvent::Error(err) => {
                            return Err(SessionFailure::from_message(err).after_connect());
                        }
                        _ => {}
                    },
                    Ok(None) => {}
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
                if client.input(X11Event::Refresh).await.is_err() {
                    break;
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
            VncError::NoEncoding | VncError::WrongPixelFormat | VncError::ClientNotRunning => {
                Self::new(SessionErrorCode::Protocol, err.to_string(), false)
            }
            _ => Self::new(SessionErrorCode::Internal, err.to_string(), false),
        }
    }
}
