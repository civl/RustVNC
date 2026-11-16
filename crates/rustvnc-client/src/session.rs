use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
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
    Error(String),
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
                        let _ = event_tx
                            .send(SessionEvent::Error(format!("create tokio runtime: {err}")));
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
    if let Err(err) = connect_and_loop(target, config, password, &mut commands, &events).await {
        let _ = events.send(SessionEvent::Error(format!("{err:#}")));
    }
    let _ = events.send(SessionEvent::Closed);
}

async fn connect_and_loop(
    target: String,
    config: ClientConfig,
    password: String,
    commands: &mut UnboundedReceiver<SessionCommand>,
    events: &Sender<SessionEvent>,
) -> Result<()> {
    if config.auth == AuthMode::Ard && config.user.trim().is_empty() {
        bail!("ARD auth requires a macOS user name");
    }
    if config.auth == AuthMode::Vnc && password.is_empty() {
        bail!("VNC auth requires a password");
    }

    let tcp = TcpStream::connect(&target)
        .await
        .with_context(|| format!("connect to {target}"))?;
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
        .context("build RFB connector")?
        .try_start()
        .await
        .context("RFB handshake/authentication")?
        .finish()
        .context("finish RFB client")?;

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
                            let _ = events.send(SessionEvent::Error(err));
                        }
                        _ => {}
                    },
                    Ok(None) => {}
                    Err(err) => {
                        let _ = events.send(SessionEvent::Error(format!("VNC session: {err}")));
                        break;
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
