use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use opus::{Channels, Decoder};
use rcgen::{CertificateParams, DnType, ExtendedKeyUsagePurpose, KeyPair, KeyUsagePurpose};
use tracing::{info, warn};

use crate::paths;
use crate::protocol::{self, ControlMsg};
use crate::secrets::SecretStore;
use crate::tls;

#[derive(Debug, Clone)]
pub struct PairOptions {
    pub addr: String,
    pub code: String,
    pub name: String,
    pub state_dir: PathBuf,
    pub expect_fingerprint: Option<String>,
    pub server_name: Option<String>,
    pub keychain: bool,
}

#[derive(Debug, Clone)]
pub struct AudioOptions {
    pub addr: String,
    pub seconds: u64,
    pub state_dir: PathBuf,
    pub server_name: Option<String>,
    pub keychain: bool,
}

#[derive(Debug, Clone)]
pub struct PingOptions {
    pub addr: String,
    pub count: u32,
    pub state_dir: PathBuf,
    pub server_name: Option<String>,
    pub keychain: bool,
    pub clipboard_text: Option<String>,
    pub interval_ms: u64,
}

struct ClientState {
    ca_cert_pem: String,
    client_cert_pem: String,
    client_key_pem: String,
}

pub async fn pair(options: PairOptions) -> Result<()> {
    tls::install_provider();

    // The client generates its private key locally and only sends a CSR.
    let key = KeyPair::generate().context("generate client key")?;
    let mut params =
        CertificateParams::new(Vec::<String>::new()).context("client certificate params")?;
    params
        .distinguished_name
        .push(DnType::CommonName, options.name.clone());
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ClientAuth);
    let csr = params.serialize_request(&key).context("create CSR")?;
    let csr_pem = csr.pem().context("encode CSR")?;

    let remote = resolve(&options.addr).await?;
    let server_name = options
        .server_name
        .clone()
        .unwrap_or_else(|| host_from_addr(&options.addr));
    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse()?).context("create QUIC client endpoint")?;
    endpoint.set_default_client_config(tls::pairing_client_config()?);

    let connection = endpoint
        .connect(remote, &server_name)
        .with_context(|| format!("connect to {remote} ({server_name})"))?
        .await
        .context("QUIC/TLS handshake")?;

    let (mut send, mut recv) = connection.open_bi().await.context("open control stream")?;
    protocol::write_msg(
        &mut send,
        &ControlMsg::PairRequest {
            code: options.code.clone(),
            client_name: options.name.clone(),
            csr_pem,
        },
    )
    .await?;

    let response = protocol::read_msg(&mut recv).await?;
    match response {
        ControlMsg::PairResponse {
            client_cert_pem,
            ca_cert_pem,
            server_fingerprint,
        } => {
            if let Some(expected) = &options.expect_fingerprint
                && !server_fingerprint.eq_ignore_ascii_case(expected)
            {
                bail!("server fingerprint mismatch: expected {expected}, got {server_fingerprint}");
            }

            save_state(
                &options.state_dir,
                options.keychain,
                &ca_cert_pem,
                &client_cert_pem,
                &key.serialize_pem(),
            )?;

            println!("PAIRED");
            println!("SERVER FINGERPRINT: {server_fingerprint}");
            println!("STATE DIR: {}", options.state_dir.display());
        }
        ControlMsg::Error { code, message } => {
            bail!("pairing rejected (code {code}): {message}");
        }
        other => bail!("unexpected pairing response: {other:?}"),
    }

    let _ = send.finish();
    connection.close(0u32.into(), b"pairing complete");
    endpoint.wait_idle().await;
    Ok(())
}

pub async fn ping(options: PingOptions) -> Result<()> {
    tls::install_provider();

    let state = load_state(&options.state_dir, options.keychain)?;
    let remote = resolve(&options.addr).await?;
    let server_name = options
        .server_name
        .clone()
        .unwrap_or_else(|| host_from_addr(&options.addr));

    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse()?).context("create QUIC client endpoint")?;
    endpoint.set_default_client_config(tls::authenticated_client_config(
        &state.ca_cert_pem,
        &state.client_cert_pem,
        &state.client_key_pem,
    )?);

    let connection = endpoint
        .connect(remote, &server_name)
        .with_context(|| format!("connect to {remote} ({server_name})"))?
        .await
        .context("QUIC/TLS handshake (mTLS)")?;

    let (mut send, mut recv) = connection.open_bi().await.context("open control stream")?;
    protocol::write_msg(
        &mut send,
        &ControlMsg::Hello {
            proto: protocol::PROTO_VERSION,
            client: options.name_or_default(),
            capabilities: 0,
        },
    )
    .await?;

    let hello = protocol::read_msg(&mut recv).await?;
    match hello {
        ControlMsg::ServerHello {
            proto,
            agent,
            capabilities,
        } => {
            if proto != protocol::PROTO_VERSION {
                bail!(
                    "server protocol version {proto} != {}",
                    protocol::PROTO_VERSION
                );
            }
            println!("CONNECTED");
            println!("AGENT: {agent}");
            println!("CAPABILITIES: {capabilities:#x}");
        }
        ControlMsg::Error { code, message } => {
            bail!("server rejected session (code {code}): {message}")
        }
        other => bail!("unexpected server hello: {other:?}"),
    }

    if let Some(text) = options.clipboard_text.clone() {
        protocol::write_msg(&mut send, &ControlMsg::ClipboardText { text }).await?;
    }

    for index in 0..options.count {
        let nonce = rand::random::<u64>();
        let start = Instant::now();
        protocol::write_msg(&mut send, &ControlMsg::Ping { nonce }).await?;
        loop {
            match protocol::read_msg(&mut recv).await? {
                ControlMsg::Pong { nonce: echoed } if echoed == nonce => {
                    let rtt = start.elapsed();
                    println!("pong {index}: rtt={:.3} ms", rtt.as_secs_f64() * 1000.0);
                    if options.interval_ms > 0 && index + 1 < options.count {
                        tokio::time::sleep(Duration::from_millis(options.interval_ms)).await;
                    }
                    break;
                }
                ControlMsg::Pong { nonce: echoed } => {
                    warn!("pong nonce mismatch: sent {nonce}, got {echoed}");
                    break;
                }
                ControlMsg::ClipboardText { text } => {
                    println!("clipboard: {} bytes", text.len());
                }
                ControlMsg::Error { code, message } => {
                    bail!("server error (code {code}): {message}")
                }
                other => bail!("unexpected ping response: {other:?}"),
            }
        }
    }

    let _ = protocol::write_msg(&mut send, &ControlMsg::Goodbye).await;
    let _ = send.finish();
    connection.close(0u32.into(), b"ping complete");
    endpoint.wait_idle().await;
    info!("ping session complete");
    Ok(())
}

pub async fn audio(options: AudioOptions) -> Result<()> {
    tls::install_provider();

    let state = load_state(&options.state_dir, options.keychain)?;
    let remote = resolve(&options.addr).await?;
    let server_name = options
        .server_name
        .clone()
        .unwrap_or_else(|| host_from_addr(&options.addr));

    let mut endpoint =
        quinn::Endpoint::client("0.0.0.0:0".parse()?).context("create QUIC client endpoint")?;
    endpoint.set_default_client_config(tls::authenticated_client_config(
        &state.ca_cert_pem,
        &state.client_cert_pem,
        &state.client_key_pem,
    )?);

    let connection = endpoint
        .connect(remote, &server_name)
        .with_context(|| format!("connect to {remote} ({server_name})"))?
        .await
        .context("QUIC/TLS handshake (mTLS)")?;

    let (mut send, mut recv) = connection.open_bi().await.context("open control stream")?;
    protocol::write_msg(
        &mut send,
        &ControlMsg::Hello {
            proto: protocol::PROTO_VERSION,
            client: options.name_or_default(),
            capabilities: protocol::CAP_AUDIO,
        },
    )
    .await?;

    match protocol::read_msg(&mut recv).await? {
        ControlMsg::ServerHello {
            proto,
            capabilities,
            ..
        } => {
            if proto != protocol::PROTO_VERSION {
                bail!(
                    "server protocol version {proto} != {}",
                    protocol::PROTO_VERSION
                );
            }
            if capabilities & protocol::CAP_AUDIO == 0 {
                bail!("agent did not start audio capture (permission or tap error)");
            }
        }
        ControlMsg::Error { code, message } => {
            bail!("server rejected session (code {code}): {message}")
        }
        other => bail!("unexpected server hello: {other:?}"),
    }

    let mut decoder = Decoder::new(48_000, Channels::Stereo).context("create Opus decoder")?;
    let deadline = Instant::now() + Duration::from_secs(options.seconds);
    let mut pcm = vec![0f32; 5760];
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut squared_sum = 0f64;
    let mut samples = 0u64;
    let mut last_seq: Option<u16> = None;

    while Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(500), connection.read_datagram()).await {
            Ok(Ok(datagram)) => {
                bytes += datagram.len() as u64;
                let packet = match protocol::parse_audio_packet(&datagram) {
                    Ok(packet) => packet,
                    Err(err) => {
                        warn!("bad audio packet: {err:#}");
                        continue;
                    }
                };
                if let Some(previous) = last_seq
                    && packet.seq != previous.wrapping_add(1)
                {
                    warn!("audio packet gap: {} -> {}", previous, packet.seq);
                }
                last_seq = Some(packet.seq);

                let decoded = decoder
                    .decode_float(&packet.payload, &mut pcm, false)
                    .context("Opus decode")?;
                packets += 1;
                samples += decoded as u64;
                for sample in &pcm[..decoded * 2] {
                    squared_sum += (*sample as f64) * (*sample as f64);
                }
                if packets.is_multiple_of(50) {
                    println!("audio: packets={packets} seq={} bytes={bytes}", packet.seq);
                }
            }
            Ok(Err(err)) => bail!("read audio datagram: {err}"),
            Err(_) => {}
        }
    }

    let rms = if samples == 0 {
        0.0
    } else {
        (squared_sum / (samples * 2) as f64).sqrt()
    };
    println!("audio summary: packets={packets} samples={samples} bytes={bytes} rms={rms:.5}");

    let _ = protocol::write_msg(&mut send, &ControlMsg::Goodbye).await;
    let _ = send.finish();
    connection.close(0u32.into(), b"audio complete");
    endpoint.wait_idle().await;
    Ok(())
}

impl AudioOptions {
    fn name_or_default(&self) -> String {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "rvncctl".to_string())
    }
}

impl PingOptions {
    fn name_or_default(&self) -> String {
        std::env::var("HOSTNAME").unwrap_or_else(|_| "rvncctl".to_string())
    }
}

fn save_state(
    dir: &Path,
    keychain: bool,
    ca_cert_pem: &str,
    client_cert_pem: &str,
    client_key_pem: &str,
) -> Result<()> {
    paths::ensure_private_dir(dir)?;
    let store = SecretStore::client(dir, keychain);
    store.set(
        &SecretStore::client_account(dir),
        "client-key.pem",
        client_key_pem.as_bytes(),
    )?;
    paths::write_private(&dir.join("ca.pem"), ca_cert_pem.as_bytes())?;
    paths::write_private(&dir.join("client-cert.pem"), client_cert_pem.as_bytes())?;
    Ok(())
}

fn load_state(dir: &Path, keychain: bool) -> Result<ClientState> {
    let ca_cert_pem = std::fs::read_to_string(dir.join("ca.pem"))
        .with_context(|| format!("read {}/ca.pem (run `rvncctl pair` first)", dir.display()))?;
    let client_cert_pem = std::fs::read_to_string(dir.join("client-cert.pem"))
        .with_context(|| format!("read {}/client-cert.pem", dir.display()))?;

    let store = SecretStore::client(dir, keychain);
    let client_key = store
        .get(&SecretStore::client_account(dir), "client-key.pem")?
        .ok_or_else(|| {
            anyhow!(
                "client key missing for {}; run `rvncctl pair` again",
                dir.display()
            )
        })?;
    let client_key_pem = String::from_utf8(client_key).context("client key is not UTF-8 PEM")?;

    Ok(ClientState {
        ca_cert_pem,
        client_cert_pem,
        client_key_pem,
    })
}

async fn resolve(addr: &str) -> Result<SocketAddr> {
    let mut addresses = tokio::net::lookup_host(addr)
        .await
        .with_context(|| format!("resolve {addr}"))?;
    addresses
        .next()
        .ok_or_else(|| anyhow!("no address for {addr}"))
}

fn host_from_addr(addr: &str) -> String {
    if let Some(rest) = addr.strip_prefix('[')
        && let Some(end) = rest.find(']')
    {
        return rest[..end].to_string();
    }
    addr.rsplit_once(':')
        .map(|(host, _)| host.to_string())
        .unwrap_or_else(|| addr.to_string())
}

#[cfg(test)]
mod tests {
    use super::host_from_addr;

    #[test]
    fn extracts_hosts() {
        assert_eq!(host_from_addr("127.0.0.1:8443"), "127.0.0.1");
        assert_eq!(host_from_addr("localhost:8443"), "localhost");
        assert_eq!(host_from_addr("[::1]:8443"), "::1");
    }
}
