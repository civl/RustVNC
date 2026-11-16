use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use subtle::ConstantTimeEq;
use tracing::{info, warn};

use crate::protocol::{self, ControlMsg};
use crate::tls;

#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub listen: SocketAddr,
    pub state_dir: PathBuf,
    pub pair_code: Option<String>,
    pub sans: Vec<String>,
}

pub async fn run(options: ServeOptions) -> Result<()> {
    tls::install_provider();

    let material = Arc::new(tls::ensure_material(&options.state_dir, &options.sans)?);
    let allowlist = Arc::new(Allowlist::load(options.state_dir.join("clients.txt"))?);
    let code = options
        .pair_code
        .clone()
        .unwrap_or_else(generate_pairing_code);
    let pairing = Arc::new(Pairing::new(code.clone(), Duration::from_secs(300)));

    let endpoint = quinn::Endpoint::server(tls::server_config(&material)?, options.listen)
        .context("bind QUIC endpoint")?;
    let local = endpoint.local_addr().context("local address")?;

    println!("LISTENING: {local}");
    println!("PAIRING CODE: {code}");
    println!("SERVER FINGERPRINT: {}", material.server_fingerprint);
    println!("STATE DIR: {}", options.state_dir.display());
    info!(%local, "agent control server listening");

    while let Some(incoming) = endpoint.accept().await {
        let material = Arc::clone(&material);
        let allowlist = Arc::clone(&allowlist);
        let pairing = Arc::clone(&pairing);

        tokio::spawn(async move {
            match incoming.await {
                Ok(connection) => {
                    let remote = connection.remote_address();
                    match handle_connection(connection, material, allowlist, pairing).await {
                        Ok(()) => info!(%remote, "connection closed"),
                        Err(err) => warn!(%remote, "connection failed: {err:#}"),
                    }
                }
                Err(err) => warn!("incoming QUIC connection failed: {err}"),
            }
        });
    }

    Ok(())
}

async fn handle_connection(
    connection: quinn::Connection,
    material: Arc<tls::Material>,
    allowlist: Arc<Allowlist>,
    pairing: Arc<Pairing>,
) -> Result<()> {
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .context("accept control stream")?;

    let first = protocol::read_msg(&mut recv).await?;
    match first {
        ControlMsg::PairRequest { code, client_name } => match pairing.verify(&code) {
            Ok(()) => {
                let issued = tls::issue_client_cert(&material, &client_name)?;
                allowlist.add(&issued.fingerprint, &client_name)?;
                protocol::write_msg(
                    &mut send,
                    &ControlMsg::PairResponse {
                        client_cert_pem: issued.cert_pem,
                        client_key_pem: issued.key_pem,
                        ca_cert_pem: material.ca_cert_pem.clone(),
                        server_fingerprint: material.server_fingerprint.clone(),
                    },
                )
                .await?;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(5), connection.closed()).await;
            }
            Err(err) => {
                let _ = protocol::write_msg(&mut send, &protocol::error(err.code(), err.message()))
                    .await;
                let _ = send.finish();
                let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
            }
        },
        ControlMsg::Hello {
            proto,
            client,
            capabilities: _,
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
            protocol::write_msg(
                &mut send,
                &ControlMsg::ServerHello {
                    proto: protocol::PROTO_VERSION,
                    agent: format!("RustVNC Agent {}", env!("CARGO_PKG_VERSION")),
                    capabilities: 0,
                },
            )
            .await?;

            loop {
                match protocol::read_msg(&mut recv).await {
                    Ok(ControlMsg::Ping { nonce }) => {
                        protocol::write_msg(&mut send, &ControlMsg::Pong { nonce }).await?;
                    }
                    Ok(ControlMsg::Goodbye) => break,
                    Ok(other) => {
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
                    Err(err) => {
                        warn!("control stream ended: {err:#}");
                        break;
                    }
                }
            }
            let _ = send.finish();
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
}

#[derive(Debug, Clone, Copy)]
enum PairingError {
    BadCode,
    Expired,
    AlreadyPaired,
    TooManyAttempts,
}

impl Pairing {
    fn new(code: String, ttl: Duration) -> Self {
        Self {
            code,
            expires_at: Instant::now() + ttl,
            used: AtomicBool::new(false),
            attempts: AtomicU32::new(0),
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
        if attempts > 5 {
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
