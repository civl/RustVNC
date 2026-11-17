use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use subtle::ConstantTimeEq;
use tokio::sync::{oneshot, watch};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::audio::AudioCapture;
use crate::clipboard;
use crate::display::DisplayGuard;
use crate::protocol::{self, ControlMsg};
use crate::tls;

const PAIRING_TTL: Duration = Duration::from_secs(300);
/// How long the control plane waits for the Core Audio tap before answering
/// ServerHello without audio. Bounded because the tap needs a TCC prompt.
const AUDIO_START_TIMEOUT: Duration = Duration::from_secs(3);
const PAIRING_GLOBAL_ATTEMPTS: u32 = 5;
const PAIRING_PER_IP_LIMIT: u32 = 5;
const PAIRING_PER_IP_WINDOW: Duration = Duration::from_secs(600);
const CONNECTION_PER_IP_LIMIT: u32 = 30;
const CONNECTION_PER_IP_WINDOW: Duration = Duration::from_secs(60);

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
    pub pair_code: Option<String>,
    pub sans: Vec<String>,
    pub keychain: bool,
    pub audio_enabled: bool,
    /// Runtime switch controlled by the menu bar; false by default.
    pub allow_resolution: Arc<AtomicBool>,
    pub events: Option<Sender<ServerEvent>>,
}

#[derive(Clone)]
struct SessionOptions {
    audio_enabled: bool,
    allow_resolution: Arc<AtomicBool>,
    state_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub enum ServerEvent {
    Started(ServeInfo),
    Paired { client: String, fingerprint: String },
    Connected { client: String, fingerprint: String },
    Disconnected,
    Stopped,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct ServeInfo {
    pub local_addr: SocketAddr,
    pub pair_code: String,
    pub server_fingerprint: String,
    pub state_dir: PathBuf,
}

pub async fn run(options: ServeOptions) -> Result<()> {
    // Keep the sender alive for the lifetime of the server so the receiver
    // only observes a change when we explicitly request shutdown.
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    run_with_shutdown(options, None, shutdown_rx).await
}

pub async fn run_with_shutdown(
    options: ServeOptions,
    ready: Option<oneshot::Sender<ServeInfo>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    tls::install_provider();

    match crate::display::restore_saved(&options.state_dir) {
        Ok(true) => info!("restored display mode left by a previous session"),
        Ok(false) => {}
        Err(err) => warn!("display mode recovery failed: {err:#}"),
    }

    let material = Arc::new(tls::ensure_material(
        &options.state_dir,
        &options.sans,
        options.keychain,
    )?);
    let allowlist = Arc::new(Allowlist::load(options.state_dir.join("clients.txt"))?);
    let code = options
        .pair_code
        .clone()
        .unwrap_or_else(generate_pairing_code);
    let pairing = Arc::new(Pairing::new(
        code.clone(),
        PAIRING_TTL,
        PAIRING_GLOBAL_ATTEMPTS,
    ));

    let endpoint = quinn::Endpoint::server(tls::server_config(&material)?, options.listen)
        .context("bind QUIC endpoint")?;
    let local = endpoint.local_addr().context("local address")?;
    let info = ServeInfo {
        local_addr: local,
        pair_code: code.clone(),
        server_fingerprint: material.server_fingerprint.clone(),
        state_dir: options.state_dir.clone(),
    };

    println!("LISTENING: {local}");
    println!("PAIRING CODE: {code}");
    println!("SERVER FINGERPRINT: {}", material.server_fingerprint);
    println!("STATE DIR: {}", options.state_dir.display());
    info!(%local, "agent control server listening");

    emit(&options.events, ServerEvent::Started(info.clone()));
    if let Some(ready) = ready {
        let _ = ready.send(info);
    }

    let pair_limiter = Arc::new(RateLimiter::new(
        PAIRING_PER_IP_LIMIT,
        PAIRING_PER_IP_WINDOW,
    ));
    let connection_limiter = Arc::new(RateLimiter::new(
        CONNECTION_PER_IP_LIMIT,
        CONNECTION_PER_IP_WINDOW,
    ));
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            _ = shutdown_requested(&mut shutdown) => break,
            incoming = endpoint.accept() => {
                let Some(incoming) = incoming else { break };
                let ip = incoming.remote_address().ip();
                if !connection_limiter.allow(ip) {
                    warn!(%ip, "connection rate limited");
                    incoming.refuse();
                    continue;
                }

                let material = Arc::clone(&material);
                let allowlist = Arc::clone(&allowlist);
                let pairing = Arc::clone(&pairing);
                let pair_limiter = Arc::clone(&pair_limiter);
                let events = options.events.clone();
                let session_options = SessionOptions {
                    audio_enabled: options.audio_enabled,
                    allow_resolution: Arc::clone(&options.allow_resolution),
                    state_dir: options.state_dir.clone(),
                };

                tasks.spawn(async move {
                    match incoming.await {
                        Ok(connection) => {
                            let remote = connection.remote_address();
                            match handle_connection(
                                connection,
                                material,
                                allowlist,
                                pairing,
                                pair_limiter,
                                events,
                                session_options,
                            )
                            .await
                            {
                                Ok(()) => info!(%remote, "connection closed"),
                                Err(err) => warn!(%remote, "connection failed: {err:#}"),
                            }
                        }
                        Err(err) => warn!("incoming QUIC connection failed: {err}"),
                    }
                });
            }
        }
    }

    endpoint.close(0u32.into(), b"shutdown");
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    emit(&options.events, ServerEvent::Stopped);
    info!("control server stopped");
    Ok(())
}

fn emit(events: &Option<Sender<ServerEvent>>, event: ServerEvent) {
    if let Some(events) = events {
        let _ = events.send(event);
    }
}

async fn shutdown_requested(shutdown: &mut watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() {
            return;
        }
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

async fn handle_connection(
    connection: quinn::Connection,
    material: Arc<tls::Material>,
    allowlist: Arc<Allowlist>,
    pairing: Arc<Pairing>,
    pair_limiter: Arc<RateLimiter>,
    events: Option<Sender<ServerEvent>>,
    session_options: SessionOptions,
) -> Result<()> {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .context("accept control stream")?;

    let first = protocol::read_msg(&mut recv).await?;
    match first {
        ControlMsg::PairRequest {
            code,
            client_name,
            csr_pem,
        } => {
            let ip = connection.remote_address().ip();
            if !pair_limiter.allow(ip) {
                warn!(%ip, "pairing rate limited");
                let _ = protocol::write_msg(
                    &mut send,
                    &protocol::error(
                        protocol::error_code::TOO_MANY_ATTEMPTS,
                        "too many pairing attempts from this address",
                    ),
                )
                .await;
                finish_and_wait(&mut send, &connection).await;
                return Ok(());
            }

            match pairing.verify(&code) {
                Ok(()) => {
                    let issued = tls::issue_client_cert(&material, &client_name, &csr_pem)?;
                    allowlist.add(&issued.fingerprint, &client_name)?;
                    emit(
                        &events,
                        ServerEvent::Paired {
                            client: client_name.clone(),
                            fingerprint: issued.fingerprint.clone(),
                        },
                    );
                    protocol::write_msg(
                        &mut send,
                        &ControlMsg::PairResponse {
                            client_cert_pem: issued.cert_pem,
                            ca_cert_pem: material.ca_cert_pem.clone(),
                            server_fingerprint: material.server_fingerprint.clone(),
                        },
                    )
                    .await?;
                    let _ = send.finish();
                    let _ = tokio::time::timeout(Duration::from_secs(5), connection.closed()).await;
                }
                Err(err) => {
                    let _ =
                        protocol::write_msg(&mut send, &protocol::error(err.code(), err.message()))
                            .await;
                    finish_and_wait(&mut send, &connection).await;
                }
            }
        }
        ControlMsg::Hello {
            proto,
            client,
            capabilities: client_capabilities,
        } => {
            if proto != protocol::PROTO_VERSION {
                let _ = protocol::write_msg(
                    &mut send,
                    &protocol::error(
                        protocol::error_code::PROTOCOL,
                        format!("unsupported protocol version {proto}"),
                    ),
                )
                .await;
                finish_and_wait(&mut send, &connection).await;
                return Ok(());
            }

            let Some(fingerprint) = peer_fingerprint(&connection) else {
                let _ = protocol::write_msg(
                    &mut send,
                    &protocol::error(
                        protocol::error_code::UNAUTHORIZED,
                        "client certificate required",
                    ),
                )
                .await;
                finish_and_wait(&mut send, &connection).await;
                return Ok(());
            };

            if !allowlist.contains(&fingerprint) {
                warn!(%fingerprint, %client, "rejecting unpaired client");
                let _ = protocol::write_msg(
                    &mut send,
                    &protocol::error(
                        protocol::error_code::UNAUTHORIZED,
                        "client is not paired with this agent",
                    ),
                )
                .await;
                finish_and_wait(&mut send, &connection).await;
                return Ok(());
            }

            info!(%client, %fingerprint, "client authenticated");
            emit(
                &events,
                ServerEvent::Connected {
                    client: client.clone(),
                    fingerprint: fingerprint.clone(),
                },
            );
            let (audio_tx, mut audio_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
            let mut audio_capture = None;
            let mut audio_forward = None;
            let mut capabilities = 0u32;

            if session_options.audio_enabled && client_capabilities & protocol::CAP_AUDIO != 0 {
                // Creating the Core Audio tap can block while macOS shows the
                // "System Audio Recording" prompt, and `ServerHello` must go out
                // regardless: the client needs it for clipboard and keepalive.
                // Run the tap setup on a blocking thread and bound the wait, so a
                // pending permission dialog costs the audio feature — not the
                // whole channel.
                let start = tokio::task::spawn_blocking(move || AudioCapture::start(audio_tx));
                match tokio::time::timeout(AUDIO_START_TIMEOUT, start).await {
                    Ok(Ok(Ok(capture))) => {
                        let connection = connection.clone();
                        audio_forward = Some(tokio::spawn(async move {
                            while let Some(packet) = audio_rx.recv().await {
                                if let Err(err) =
                                    connection.send_datagram(bytes::Bytes::from(packet))
                                {
                                    warn!("audio datagram send failed: {err}");
                                    break;
                                }
                            }
                        }));
                        audio_capture = Some(capture);
                        capabilities |= protocol::CAP_AUDIO;
                        info!("audio capture started for session");
                    }
                    Ok(Ok(Err(err))) => {
                        warn!("audio unavailable for session: {err:#}");
                    }
                    Ok(Err(join)) => {
                        warn!("audio start task failed: {join}");
                    }
                    Err(_) => {
                        warn!(
                            "audio capture did not start within {AUDIO_START_TIMEOUT:?}; \
                             continuing without audio (grant System Audio Recording permission)"
                        );
                    }
                }
            }

            capabilities |= protocol::CAP_DISPLAY;
            protocol::write_msg(
                &mut send,
                &ControlMsg::ServerHello {
                    proto: protocol::PROTO_VERSION,
                    agent: format!("RustVNC Agent {}", env!("CARGO_PKG_VERSION")),
                    capabilities,
                },
            )
            .await?;

            let (msg_tx, mut msg_rx) = tokio::sync::mpsc::channel::<ControlMsg>(16);
            let mut reader_recv = recv;
            let reader = tokio::spawn(async move {
                loop {
                    match protocol::read_msg(&mut reader_recv).await {
                        Ok(message) => {
                            if msg_tx.send(message).await.is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            warn!("control stream ended: {err:#}");
                            break;
                        }
                    }
                }
            });

            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut clipboard_enabled = true;
            // Baseline the current pasteboard so connecting does not immediately
            // upload whatever the user copied before the session.
            let mut last_sent = match clipboard::get_text().await {
                Ok(Some(text)) => text,
                Ok(None) => String::new(),
                Err(err) => {
                    warn!("clipboard read failed at session start: {err:#}");
                    clipboard_enabled = false;
                    String::new()
                }
            };
            let mut last_received = String::new();
            let mut display: Option<DisplayGuard> = None;

            loop {
                tokio::select! {
                    message = msg_rx.recv() => {
                        match message {
                            Some(ControlMsg::Ping { nonce }) => {
                                if protocol::write_msg(&mut send, &ControlMsg::Pong { nonce })
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                            Some(ControlMsg::ClipboardText { text }) => {
                                if let Some(text) = clipboard::normalize(&text) {
                                    last_received = text.clone();
                                    if let Err(err) = clipboard::set_text(&text).await {
                                        warn!("clipboard write failed: {err:#}");
                                        clipboard_enabled = false;
                                    }
                                }
                            }
                            Some(ControlMsg::SetDisplayMode { width, height }) => {
                                let result = if !session_options.allow_resolution.load(Ordering::Relaxed) {
                                    Err(anyhow::anyhow!(
                                        "远程分辨率控制已在 Mac 菜单栏关闭"
                                    ))
                                } else {
                                    if display.is_none() {
                                        match DisplayGuard::new(session_options.state_dir.clone()) {
                                            Ok(guard) => display = Some(guard),
                                            Err(err) => {
                                                let _ = protocol::write_msg(
                                                    &mut send,
                                                    &ControlMsg::DisplayModeResult {
                                                        width,
                                                        height,
                                                        applied: false,
                                                        message: format!("{err:#}"),
                                                    },
                                                )
                                                .await;
                                                continue;
                                            }
                                        }
                                    }
                                    display
                                        .as_ref()
                                        .expect("display guard")
                                        .apply(width, height)
                                };
                                let response = match result {
                                    Ok(mode) => protocol::ControlMsg::DisplayModeResult {
                                        width: mode.width,
                                        height: mode.height,
                                        applied: true,
                                        message: format!(
                                            "{}x{} ({}x{} px)",
                                            mode.width, mode.height, mode.pixel_width, mode.pixel_height
                                        ),
                                    },
                                    Err(err) => protocol::ControlMsg::DisplayModeResult {
                                        width,
                                        height,
                                        applied: false,
                                        message: format!("{err:#}"),
                                    },
                                };
                                if protocol::write_msg(&mut send, &response).await.is_err() {
                                    break;
                                }
                            }
                            Some(ControlMsg::Goodbye) | None => break,
                            Some(other) => {
                                let _ = protocol::write_msg(
                                    &mut send,
                                    &protocol::error(
                                        protocol::error_code::PROTOCOL,
                                        format!("unexpected message in control stream: {other:?}"),
                                    ),
                                )
                                .await;
                                break;
                            }
                        }
                    }
                    _ = ticker.tick() => {
                        if !clipboard_enabled {
                            continue;
                        }
                        match clipboard::get_text().await {
                            Ok(Some(text)) => {
                                if let Some(text) = clipboard::normalize(&text)
                                    && text != last_sent
                                    && text != last_received
                                {
                                    if protocol::write_msg(
                                        &mut send,
                                        &ControlMsg::ClipboardText { text: text.clone() },
                                    )
                                    .await
                                    .is_err()
                                    {
                                        break;
                                    }
                                    last_sent = text;
                                }
                            }
                            Ok(None) => {}
                            Err(err) => {
                                warn!("clipboard read failed: {err:#}");
                                clipboard_enabled = false;
                            }
                        }
                    }
                }
            }
            drop(display);
            reader.abort();
            if let Some(mut capture) = audio_capture.take() {
                capture.stop();
            }
            if let Some(forward) = audio_forward.take() {
                forward.abort();
            }
            let _ = send.finish();
            emit(&events, ServerEvent::Disconnected);
        }
        other => {
            let _ = protocol::write_msg(
                &mut send,
                &protocol::error(
                    protocol::error_code::PROTOCOL,
                    format!("expected PairRequest or Hello, got {other:?}"),
                ),
            )
            .await;
            finish_and_wait(&mut send, &connection).await;
        }
    }

    Ok(())
}

async fn finish_and_wait(send: &mut quinn::SendStream, connection: &quinn::Connection) {
    let _ = send.finish();
    let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
}

fn peer_fingerprint(connection: &quinn::Connection) -> Option<String> {
    let identity = connection.peer_identity()?;
    let certs = identity
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok()?;
    let leaf = certs.first()?;
    Some(tls::fingerprint_der(leaf))
}

fn generate_pairing_code() -> String {
    format!("{:08}", rand::random_range(0..100_000_000u32))
}

struct Pairing {
    code: String,
    expires_at: Instant,
    used: AtomicBool,
    attempts: AtomicU32,
    max_attempts: u32,
}

#[derive(Debug, Clone, Copy)]
enum PairingError {
    BadCode,
    Expired,
    AlreadyPaired,
    TooManyAttempts,
}

impl Pairing {
    fn new(code: String, ttl: Duration, max_attempts: u32) -> Self {
        Self {
            code,
            expires_at: Instant::now() + ttl,
            used: AtomicBool::new(false),
            attempts: AtomicU32::new(0),
            max_attempts,
        }
    }

    fn verify(&self, candidate: &str) -> std::result::Result<(), PairingError> {
        if self.used.load(Ordering::SeqCst) {
            return Err(PairingError::AlreadyPaired);
        }
        if Instant::now() >= self.expires_at {
            return Err(PairingError::Expired);
        }

        let attempts = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
        if attempts > self.max_attempts {
            return Err(PairingError::TooManyAttempts);
        }

        if candidate.as_bytes().ct_eq(self.code.as_bytes()).unwrap_u8() != 1 {
            return Err(PairingError::BadCode);
        }

        if self
            .used
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(PairingError::AlreadyPaired);
        }
        Ok(())
    }
}

impl PairingError {
    fn code(self) -> u16 {
        match self {
            PairingError::BadCode => protocol::error_code::BAD_CODE,
            PairingError::Expired => protocol::error_code::PAIRING_EXPIRED,
            PairingError::AlreadyPaired => protocol::error_code::ALREADY_PAIRED,
            PairingError::TooManyAttempts => protocol::error_code::TOO_MANY_ATTEMPTS,
        }
    }

    fn message(self) -> &'static str {
        match self {
            PairingError::BadCode => "invalid pairing code",
            PairingError::Expired => "pairing code expired",
            PairingError::AlreadyPaired => "pairing code already used",
            PairingError::TooManyAttempts => "too many pairing attempts",
        }
    }
}

struct RateLimiter {
    limit: u32,
    window: Duration,
    entries: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl RateLimiter {
    fn new(limit: u32, window: Duration) -> Self {
        Self {
            limit,
            window,
            entries: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut entries = self.entries.lock().expect("rate limiter mutex poisoned");
        let queue = entries.entry(ip).or_default();

        while let Some(front) = queue.front() {
            if now.duration_since(*front) > self.window {
                queue.pop_front();
            } else {
                break;
            }
        }

        let allowed = (queue.len() as u32) < self.limit;
        if allowed {
            queue.push_back(now);
        }

        if entries.len() > 4096 {
            entries.retain(|_, queue| !queue.is_empty());
        }

        allowed
    }
}

struct Allowlist {
    path: PathBuf,
    entries: Mutex<HashSet<String>>,
}

impl Allowlist {
    fn load(path: PathBuf) -> Result<Self> {
        let entries = if path.exists() {
            fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?
                .lines()
                .filter_map(|line| line.split_whitespace().next())
                .map(str::to_string)
                .collect()
        } else {
            HashSet::new()
        };
        Ok(Self {
            path,
            entries: Mutex::new(entries),
        })
    }

    fn contains(&self, fingerprint: &str) -> bool {
        self.entries
            .lock()
            .expect("allowlist mutex poisoned")
            .contains(fingerprint)
    }

    fn add(&self, fingerprint: &str, client_name: &str) -> Result<()> {
        let mut entries = self.entries.lock().expect("allowlist mutex poisoned");
        if !entries.insert(fingerprint.to_string()) {
            return Ok(());
        }

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let safe_name = client_name.replace(['\n', '\r'], " ");
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open {}", self.path.display()))?;
        writeln!(file, "{fingerprint} {safe_name}")?;
        let _ = fs::set_permissions(&self.path, fs::Permissions::from_mode(0o600));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::RateLimiter;
    use std::net::IpAddr;
    use std::time::Duration;

    #[test]
    fn rate_limiter_enforces_window() {
        let limiter = RateLimiter::new(2, Duration::from_secs(60));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(limiter.allow(ip));
        assert!(limiter.allow(ip));
        assert!(!limiter.allow(ip));
    }
}
