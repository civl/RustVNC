//! Agent pairing and the long-lived control/audio session.
//!
//! The agent is a *sidecar*: it never carries video. RFB keeps drawing the
//! screen and this channel adds clipboard, audio and authenticated control, so a
//! failure here degrades those features instead of killing the session —
//! exactly the B1 model described in `agent-design.md`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tracing::{info, warn};

use crate::audio::{Decoder, Player};
use crate::protocol::{self, ControlMsg};
use crate::state;
use crate::tls;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(20);

/// TLS server name used for QUIC.
///
/// The agent issues its certificate for `localhost` / `127.0.0.1` / `::1` only,
/// but clients connect over whatever LAN or Tailscale address the Mac happens to
/// have. Rather than chase a moving IP in the certificate, the client pins the
/// exact certificate fingerprint at pairing time (see `state::AgentState`) and
/// therefore only needs a name the certificate actually carries.
const SERVER_NAME: &str = "localhost";

#[derive(Debug, Clone)]
pub struct PairConfig {
    /// `host:port`; the agent listens on 8443 unless it was reconfigured.
    pub addr: String,
    /// One-time code shown in the Mac menu bar.
    pub code: String,
    pub client_name: String,
    pub state_dir: PathBuf,
    /// Refuse to pin when the agent reports a different fingerprint.
    pub expect_fingerprint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PairOutcome {
    /// Fingerprint the agent announced over the control stream.
    pub server_fingerprint: String,
    /// Fingerprint of the certificate the TLS handshake actually presented.
    pub tls_fingerprint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub addr: String,
    pub client_name: String,
    pub state_dir: PathBuf,
    /// Ask the agent to start capture and play it locally.
    pub audio: bool,
}

#[derive(Debug)]
pub enum AgentCommand {
    ClipboardText(String),
    SetDisplayMode { width: u16, height: u16 },
    Stop,
}

#[derive(Debug, Clone)]
pub enum AgentEvent {
    Connected {
        agent: String,
        capabilities: u32,
    },
    /// Clipboard text produced on the Mac.
    Clipboard(String),
    /// Result of a display-mode request.
    DisplayModeResult {
        width: u16,
        height: u16,
        applied: bool,
        message: String,
    },
    AudioStarted,
    AudioStopped,
    Error(String),
    Closed,
}

pub struct AgentSession {
    commands: UnboundedSender<AgentCommand>,
    events: Receiver<AgentEvent>,
    thread: Option<thread::JoinHandle<()>>,
}

impl AgentSession {
    pub fn start(config: SessionConfig) -> Result<Self> {
        if !state::is_paired(&config.state_dir) {
            bail!(
                "尚未与 Agent 配对：{} 下没有证书（先用配对码配对一次）",
                config.state_dir.display()
            );
        }
        let (command_tx, command_rx) = unbounded_channel();
        let (event_tx, event_rx) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("rustvnc-agent-client".to_string())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(err) => {
                        let _ = event_tx
                            .send(AgentEvent::Error(format!("create tokio runtime: {err}")));
                        return;
                    }
                };
                runtime.block_on(run(config, command_rx, event_tx));
            })
            .context("spawn agent session thread")?;

        Ok(Self {
            commands: command_tx,
            events: event_rx,
            thread: Some(thread),
        })
    }

    pub fn try_recv(&self) -> Option<AgentEvent> {
        self.events.try_recv().ok()
    }

    pub fn send_clipboard(&self, text: String) {
        let _ = self.commands.send(AgentCommand::ClipboardText(text));
    }

    pub fn set_display_mode(&self, width: u16, height: u16) {
        let _ = self
            .commands
            .send(AgentCommand::SetDisplayMode { width, height });
    }

    pub fn stop(&mut self) {
        let _ = self.commands.send(AgentCommand::Stop);
        if let Some(thread) = self.thread.take() {
            // Detached: the worker may be inside a QUIC await.
            drop(thread);
        }
    }
}

impl Drop for AgentSession {
    fn drop(&mut self) {
        let _ = self.commands.send(AgentCommand::Stop);
    }
}

/// Pair with the agent and store the CA + client certificate it issues.
///
/// The client key never leaves this machine: only a CSR crosses the wire.
pub async fn pair(config: PairConfig) -> Result<PairOutcome> {
    tls::install_provider();

    let key = KeyPair::generate().context("generate client key")?;
    let mut params =
        CertificateParams::new(Vec::<String>::new()).context("client certificate params")?;
    params
        .distinguished_name
        .push(DnType::CommonName, config.client_name.clone());
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ClientAuth);
    let csr = params.serialize_request(&key).context("create CSR")?;
    let csr_pem = csr.pem().context("encode CSR")?;

    let remote = resolve(&config.addr).await?;
    let server_name = SERVER_NAME;
    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse()?).context("create QUIC endpoint")?;
    endpoint.set_default_client_config(tls::pairing_client_config()?);

    let connecting = endpoint
        .connect(remote, server_name)
        .with_context(|| format!("connect to {remote}"))?;
    let connection = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| anyhow!("连接 {remote} 超时（{CONNECT_TIMEOUT:?}）"))?
        .context("QUIC/TLS 握手失败")?;

    // The certificate actually presented by the peer, not the one it claims.
    let tls_fingerprint = tls::peer_fingerprint(&connection);

    let (mut send, mut recv) = connection.open_bi().await.context("open control stream")?;
    protocol::write_msg(
        &mut send,
        &ControlMsg::PairRequest {
            code: config.code.clone(),
            client_name: config.client_name.clone(),
            csr_pem,
        },
    )
    .await?;

    let response = tokio::time::timeout(HANDSHAKE_TIMEOUT, protocol::read_msg(&mut recv))
        .await
        .map_err(|_| anyhow!("等待配对响应超时"))??;

    match response {
        ControlMsg::PairResponse {
            client_cert_pem,
            ca_cert_pem,
            server_fingerprint,
        } => {
            if let Some(expected) = &config.expect_fingerprint
                && !server_fingerprint.eq_ignore_ascii_case(expected)
            {
                bail!("Agent 指纹不匹配：期望 {expected}，实际 {server_fingerprint}");
            }
            // A MITM could announce one fingerprint and present another
            // certificate; pin only when both agree.
            if let Some(actual) = &tls_fingerprint
                && !actual.eq_ignore_ascii_case(&server_fingerprint)
            {
                bail!(
                    "Agent 声明的指纹与 TLS 证书不一致，可能被中间人劫持：声明 {server_fingerprint}，实际 {actual}"
                );
            }
            state::save(
                &config.state_dir,
                &ca_cert_pem,
                &client_cert_pem,
                &key.serialize_pem(),
                &server_fingerprint,
            )?;
            info!(fingerprint = %server_fingerprint, "paired with agent");
            let _ = send.finish();
            connection.close(0u32.into(), b"paired");
            endpoint.wait_idle().await;
            Ok(PairOutcome {
                server_fingerprint,
                tls_fingerprint,
            })
        }
        ControlMsg::Error { code, message } => bail!(
            "配对被拒绝（{}）：{message}",
            protocol::error_code::describe(code)
        ),
        other => bail!("配对响应异常：{other:?}"),
    }
}

async fn run(
    config: SessionConfig,
    mut commands: UnboundedReceiver<AgentCommand>,
    events: Sender<AgentEvent>,
) {
    let mut attempt = 0u32;
    loop {
        match connect_and_run(&config, &mut commands, &events).await {
            Ok(()) => {
                let _ = events.send(AgentEvent::Closed);
                break;
            }
            Err(err) => {
                let message = format!("{err:#}");
                let _ = events.send(AgentEvent::Error(message.clone()));
                attempt = attempt.saturating_add(1);
                let delay = Duration::from_secs((1u64 << (attempt - 1).min(4)).min(15));
                warn!("agent channel: {message} (retry in {delay:?})");
                tokio::select! {
                    _ = tokio::time::sleep(delay) => {}
                    command = commands.recv() => {
                        if matches!(command, Some(AgentCommand::Stop) | None) {
                            let _ = events.send(AgentEvent::Closed);
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// Decode + play, created lazily on the first datagram so a microphone-less or
/// device-less machine only fails the audio feature, not the channel.
struct AudioPath {
    decoder: Decoder,
    player: Option<Player>,
    disabled: bool,
    scratch: Vec<f32>,
}

impl AudioPath {
    fn new() -> Result<Self> {
        Ok(Self {
            decoder: Decoder::new()?,
            player: None,
            disabled: false,
            scratch: Vec::with_capacity(5760 * 2),
        })
    }

    fn handle(&mut self, datagram: &[u8], events: &Sender<AgentEvent>) {
        let Ok(packet) = protocol::parse_audio_packet(datagram) else {
            return;
        };
        let mut scratch = std::mem::take(&mut self.scratch);
        if let Err(err) = self.decoder.decode_into(&packet.payload, &mut scratch) {
            warn!("opus decode failed: {err:#}");
            self.scratch = scratch;
            return;
        }
        if self.player.is_none() && !self.disabled {
            match Player::start() {
                Ok(player) => {
                    self.player = Some(player);
                    let _ = events.send(AgentEvent::AudioStarted);
                }
                Err(err) => {
                    self.disabled = true;
                    let _ = events.send(AgentEvent::Error(format!("音频输出不可用：{err:#}")));
                }
            }
        }
        if let Some(player) = self.player.as_mut() {
            player.push(&scratch);
        }
        self.scratch = scratch;
    }
}

async fn connect_and_run(
    config: &SessionConfig,
    commands: &mut UnboundedReceiver<AgentCommand>,
    events: &Sender<AgentEvent>,
) -> Result<()> {
    tls::install_provider();
    let state = state::load(&config.state_dir)?;
    let remote = resolve(&config.addr).await?;
    let server_name = SERVER_NAME;

    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse()?).context("create QUIC endpoint")?;
    endpoint.set_default_client_config(tls::authenticated_client_config(
        &state.ca_cert_pem,
        &state.client_cert_pem,
        &state.client_key_pem,
    )?);

    let connecting = endpoint
        .connect(remote, server_name)
        .with_context(|| format!("connect to {remote}"))?;
    let connection = tokio::time::timeout(CONNECT_TIMEOUT, connecting)
        .await
        .map_err(|_| anyhow!("连接 {remote} 超时"))?
        .context("QUIC/TLS 握手失败（客户端证书是否已被 Agent 信任？）")?;

    // The pin is the authentication here, not the certificate's name: see
    // `SERVER_NAME`. A different certificate means either a re-installed agent
    // (re-pair) or someone in the middle — either way the session stops.
    let presented = tls::peer_fingerprint(&connection)
        .ok_or_else(|| anyhow!("Agent 未出示证书，无法校验指纹"))?;
    if !presented.eq_ignore_ascii_case(&state.server_fingerprint) {
        bail!(
            "Agent 证书指纹与配对时不一致，可能被中间人劫持：期望 {}，实际 {presented}。\
             如果确实重装过 Agent，请重新配对。",
            state.server_fingerprint
        );
    }

    let (mut send, mut recv) = connection.open_bi().await.context("open control stream")?;
    let wanted = if config.audio { protocol::CAP_AUDIO } else { 0 };
    protocol::write_msg(
        &mut send,
        &ControlMsg::Hello {
            proto: protocol::PROTO_VERSION,
            client: config.client_name.clone(),
            capabilities: wanted,
        },
    )
    .await?;

    let hello = tokio::time::timeout(HANDSHAKE_TIMEOUT, protocol::read_msg(&mut recv))
        .await
        .map_err(|_| {
            // The agent answers ServerHello only after it has decided about
            // audio; on macOS that can sit behind a TCC prompt.
            if wanted & protocol::CAP_AUDIO != 0 {
                anyhow!(
                    "等待 ServerHello 超时：Agent 可能卡在“系统音频录制”权限上。\
                     请在 Mac 的「系统设置 → 隐私与安全性」里允许 RustVNC Agent 录制系统音频，\
                     或先在客户端关闭音频后重连。"
                )
            } else {
                anyhow!("等待 ServerHello 超时（Agent 未响应）")
            }
        })??;
    let capabilities = match hello {
        ControlMsg::ServerHello {
            proto,
            agent,
            capabilities,
        } => {
            if proto != protocol::PROTO_VERSION {
                bail!(
                    "Agent 协议版本 {proto} 与客户端 {} 不匹配",
                    protocol::PROTO_VERSION
                );
            }
            info!(%agent, capabilities, "agent channel connected");
            let _ = events.send(AgentEvent::Connected {
                agent: agent.clone(),
                capabilities,
            });
            capabilities
        }
        ControlMsg::Error { code, message } => bail!(
            "Agent 拒绝了会话（{}）：{message}",
            protocol::error_code::describe(code)
        ),
        other => bail!("ServerHello 异常：{other:?}"),
    };

    // The reader owns `recv`: cancelling a partially-read frame in a `select!`
    // would desynchronise the stream, so it runs on its own task.
    let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<ControlMsg>(16);
    let reader = tokio::spawn(async move {
        loop {
            match protocol::read_msg(&mut recv).await {
                Ok(message) => {
                    if msg_tx.send(message).await.is_err() {
                        break;
                    }
                }
                Err(err) => {
                    warn!("agent control stream ended: {err:#}");
                    break;
                }
            }
        }
    });

    let mut audio = if config.audio && capabilities & protocol::CAP_AUDIO != 0 {
        match AudioPath::new() {
            Ok(path) => Some(path),
            Err(err) => {
                let _ = events.send(AgentEvent::Error(format!("音频初始化失败：{err:#}")));
                None
            }
        }
    } else {
        None
    };

    let mut datagrams = 0u64;
    let mut ticker = tokio::time::interval(KEEPALIVE);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let outcome: Result<()> = loop {
        tokio::select! {
            command = commands.recv() => match command {
                Some(AgentCommand::ClipboardText(text)) => {
                    if let Err(err) = protocol::write_msg(&mut send, &ControlMsg::ClipboardText { text }).await {
                        break Err(err);
                    }
                }
                Some(AgentCommand::SetDisplayMode { width, height }) => {
                    if let Err(err) = protocol::write_msg(
                        &mut send,
                        &ControlMsg::SetDisplayMode { width, height },
                    )
                    .await
                    {
                        break Err(err);
                    }
                }
                Some(AgentCommand::Stop) | None => {
                    let _ = protocol::write_msg(&mut send, &ControlMsg::Goodbye).await;
                    break Ok(());
                }
            },
            message = msg_rx.recv() => match message {
                Some(ControlMsg::ClipboardText { text }) => {
                    let _ = events.send(AgentEvent::Clipboard(text));
                }
                Some(ControlMsg::DisplayModeResult {
                    width,
                    height,
                    applied,
                    message,
                }) => {
                    let _ = events.send(AgentEvent::DisplayModeResult {
                        width,
                        height,
                        applied,
                        message,
                    });
                }
                Some(ControlMsg::Pong { .. }) | Some(ControlMsg::Ping { .. }) => {}
                Some(ControlMsg::Error { code, message }) => {
                    break Err(anyhow!(
                        "Agent 错误（{}）：{message}",
                        protocol::error_code::describe(code)
                    ));
                }
                Some(ControlMsg::Goodbye) | None => break Ok(()),
                Some(_) => {}
            },
            datagram = connection.read_datagram(), if audio.is_some() => {
                match datagram {
                    Ok(bytes) => {
                        datagrams += 1;
                        if datagrams % 50 == 1 {
                            tracing::debug!(datagrams, bytes = bytes.len(), "audio datagram");
                        }
                        if let Some(path) = audio.as_mut() {
                            path.handle(&bytes, events);
                        }
                    }
                    Err(err) => break Err(anyhow!("音频数据通道断开：{err}")),
                }
            }
            _ = ticker.tick() => {
                let nonce = rand::random::<u64>();
                if let Err(err) = protocol::write_msg(&mut send, &ControlMsg::Ping { nonce }).await {
                    break Err(err);
                }
            }
        }
    };

    reader.abort();
    let _ = send.finish();
    connection.close(0u32.into(), b"bye");
    endpoint.wait_idle().await;
    if audio.is_some() {
        let _ = events.send(AgentEvent::AudioStopped);
    }
    outcome
}

async fn resolve(addr: &str) -> Result<SocketAddr> {
    let mut addresses = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("resolve {addr}"))?;
    addresses
        .next()
        .ok_or_else(|| anyhow!("no address for {addr}"))
}
