//! End-to-end loopback: a stub agent built from the same primitives as
//! `src/server.rs` (QUIC + TLS 1.3 + mTLS, postcard control frames, Opus
//! datagrams) talks to the real client over a real socket.
//!
//! This is the acceptance harness for the agent channel: it exercises pairing
//! (CSR → issued certificate → pinned CA), the control plane, both clipboard
//! directions, keepalive and the audio datagram path without needing a Mac.

use std::sync::Arc;

use anyhow::{Context, Result};
use quinn::crypto::rustls::QuicServerConfig;
use rcgen::{
    BasicConstraints, CertificateParams, CertificateSigningRequestParams, DnType,
    ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
};
use rustls::RootCertStore;
use rustvnc_agent_client::protocol::{self, ControlMsg};
use rustvnc_agent_client::session::{AgentEvent, AgentSession, PairConfig, SessionConfig, pair};
use rustvnc_agent_client::state;
use rustvnc_agent_client::tls as client_tls;

const PAIR_CODE: &str = "123456";

#[derive(Clone)]
struct StubAgent {
    ca_cert_pem: String,
    ca_key_pem: String,
    server_cert_pem: String,
    server_key_pem: String,
    fingerprint: String,
}

fn make_agent() -> Result<StubAgent> {
    let ca_key = KeyPair::generate()?;
    let mut ca_params = CertificateParams::new(Vec::<String>::new())?;
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "RustVNC Test CA");
    ca_params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::CrlSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let ca_cert = ca_params.self_signed(&ca_key)?;
    let ca_cert_pem = ca_cert.pem();
    let ca_key_pem = ca_key.serialize_pem();

    let issuer = Issuer::from_ca_cert_pem(&ca_cert_pem, ca_key)?;
    let server_key = KeyPair::generate()?;
    let mut params = CertificateParams::new(vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ])?;
    params
        .distinguished_name
        .push(DnType::CommonName, "RustVNC Agent");
    params.key_usages.push(KeyUsagePurpose::DigitalSignature);
    params
        .extended_key_usages
        .push(ExtendedKeyUsagePurpose::ServerAuth);
    let server_cert = params.signed_by(&server_key, &issuer)?;
    let fingerprint = client_tls::fingerprint_der(server_cert.der());

    Ok(StubAgent {
        ca_cert_pem,
        ca_key_pem,
        server_cert_pem: server_cert.pem(),
        server_key_pem: server_key.serialize_pem(),
        fingerprint,
    })
}

fn issue_client_cert(agent: &StubAgent, client_name: &str, csr_pem: &str) -> Result<String> {
    let csr = CertificateSigningRequestParams::from_pem(csr_pem).context("parse CSR")?;
    let mut params = csr.params.clone();
    params.distinguished_name = Default::default();
    params
        .distinguished_name
        .push(DnType::CommonName, client_name);
    params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let issuer =
        Issuer::from_ca_cert_pem(&agent.ca_cert_pem, KeyPair::from_pem(&agent.ca_key_pem)?)?;
    let cert = params.signed_by(&csr.public_key, &issuer)?;
    Ok(cert.pem())
}

fn server_config(agent: &StubAgent) -> Result<quinn::ServerConfig> {
    client_tls::install_provider();

    let ca_certs = client_tls::certs_from_pem(&agent.ca_cert_pem)?;
    let mut roots = RootCertStore::empty();
    for cert in &ca_certs {
        roots.add(cert.clone())?;
    }
    // The real agent does the same: the TLS layer tolerates an unknown client,
    // and the allowlist check happens in the control plane.
    let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
        .allow_unauthenticated()
        .build()?;

    let mut chain = client_tls::certs_from_pem(&agent.server_cert_pem)?;
    chain.extend(ca_certs);
    let key = rustls_pemfile::private_key(&mut agent.server_key_pem.as_bytes())?
        .context("parse server key")?;

    let mut crypto =
        rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain, key)?;
    crypto.alpn_protocols = vec![protocol::ALPN.to_vec()];

    let quic = QuicServerConfig::try_from(crypto)?;
    Ok(quinn::ServerConfig::with_crypto(Arc::new(quic)))
}

async fn run_stub(endpoint: quinn::Endpoint, agent: StubAgent, audio: bool) {
    while let Some(incoming) = endpoint.accept().await {
        let agent = agent.clone();
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else {
                return;
            };
            let _ = handle(connection, agent, audio).await;
        });
    }
}

async fn handle(connection: quinn::Connection, agent: StubAgent, audio: bool) -> Result<()> {
    let (mut send, mut recv) = connection.accept_bi().await.context("accept stream")?;
    match protocol::read_msg(&mut recv).await? {
        ControlMsg::PairRequest {
            code,
            client_name,
            csr_pem,
        } => {
            if code != PAIR_CODE {
                protocol::write_msg(
                    &mut send,
                    &protocol::error(protocol::error_code::BAD_CODE, "bad pairing code"),
                )
                .await?;
            } else {
                let client_cert_pem = issue_client_cert(&agent, &client_name, &csr_pem)?;
                protocol::write_msg(
                    &mut send,
                    &ControlMsg::PairResponse {
                        client_cert_pem,
                        ca_cert_pem: agent.ca_cert_pem.clone(),
                        server_fingerprint: agent.fingerprint.clone(),
                    },
                )
                .await?;
            }
            let _ = send.finish();
        }
        ControlMsg::Hello { capabilities, .. } => {
            let mut granted = 0u32;
            if audio && capabilities & protocol::CAP_AUDIO != 0 {
                granted |= protocol::CAP_AUDIO;
                let sender = connection.clone();
                tokio::spawn(async move {
                    let Ok(mut encoder) = opus::Encoder::new(
                        48_000,
                        opus::Channels::Stereo,
                        opus::Application::Audio,
                    ) else {
                        return;
                    };
                    let silence = vec![0i16; 960 * 2];
                    let mut packet = vec![0u8; 4000];
                    for seq in 0..12u16 {
                        let Ok(len) = encoder.encode(&silence, &mut packet) else {
                            return;
                        };
                        let bytes =
                            protocol::encode_audio_packet(seq, u32::from(seq) * 20, &packet[..len]);
                        if sender.send_datagram(bytes::Bytes::from(bytes)).is_err() {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    }
                });
            }
            protocol::write_msg(
                &mut send,
                &ControlMsg::ServerHello {
                    proto: protocol::PROTO_VERSION,
                    agent: "RustVNC Test Agent".to_string(),
                    capabilities: granted,
                },
            )
            .await?;

            loop {
                match protocol::read_msg(&mut recv).await {
                    Ok(ControlMsg::Ping { nonce }) => {
                        protocol::write_msg(&mut send, &ControlMsg::Pong { nonce }).await?;
                    }
                    Ok(ControlMsg::ClipboardText { text }) => {
                        protocol::write_msg(
                            &mut send,
                            &ControlMsg::ClipboardText {
                                text: format!("mac:{text}"),
                            },
                        )
                        .await?;
                    }
                    Ok(ControlMsg::Goodbye) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            let _ = send.finish();
        }
        other => anyhow::bail!("unexpected first message: {other:?}"),
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.closed()).await;
    Ok(())
}

fn bind(agent: &StubAgent) -> Result<quinn::Endpoint> {
    Ok(quinn::Endpoint::server(
        server_config(agent)?,
        "127.0.0.1:0".parse()?,
    )?)
}

/// Wait up to `timeout` for an event matching `want`.
fn wait_for<T>(
    session: &AgentSession,
    timeout: std::time::Duration,
    mut want: impl FnMut(&AgentEvent) -> Option<T>,
) -> Option<T> {
    let deadline = std::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    while std::time::Instant::now() < deadline {
        while let Some(event) = session.try_recv() {
            if let Some(value) = want(&event) {
                return Some(value);
            }
            seen.push(format!("{event:?}"));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    eprintln!("timed out waiting; events seen: {seen:?}");
    None
}

#[test]
fn pairing_pins_the_agent_certificate_and_clipboard_round_trips() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let agent = make_agent().expect("agent material");
    // The QUIC endpoint spawns its driver on the current runtime, so it must be
    // constructed inside the runtime context.
    let _enter = runtime.enter();
    let endpoint = bind(&agent).expect("bind");
    let addr = endpoint.local_addr().unwrap().to_string();
    let agent_for_stub = agent.clone();
    let _stub = runtime.spawn(run_stub(endpoint, agent_for_stub, false));

    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("agent");

    // --- pairing -----------------------------------------------------------
    let outcome = runtime
        .block_on(pair(PairConfig {
            addr: addr.clone(),
            code: PAIR_CODE.to_string(),
            client_name: "loopback-test".to_string(),
            state_dir: state_dir.clone(),
            expect_fingerprint: None,
        }))
        .expect("pair");

    assert_eq!(outcome.server_fingerprint, agent.fingerprint);
    assert_eq!(
        outcome.tls_fingerprint.as_deref(),
        Some(agent.fingerprint.as_str()),
        "the announced fingerprint must match the certificate actually presented"
    );
    assert!(
        state::is_paired(&state_dir),
        "pairing must persist material"
    );

    // --- control plane -----------------------------------------------------
    let mut session = AgentSession::start(SessionConfig {
        addr,
        client_name: "loopback-test".to_string(),
        state_dir,
        audio: false,
    })
    .expect("start session");

    let connected = wait_for(&session, std::time::Duration::from_secs(10), |event| {
        matches!(event, AgentEvent::Connected { .. }).then_some(())
    });
    assert!(connected.is_some(), "no Connected event from the agent");

    session.send_clipboard("hello".to_string());
    let clipboard = wait_for(
        &session,
        std::time::Duration::from_secs(10),
        |event| match event {
            AgentEvent::Clipboard(text) => Some(text.clone()),
            _ => None,
        },
    );
    assert_eq!(clipboard.as_deref(), Some("mac:hello"));

    session.stop();
}

#[test]
fn wrong_pairing_code_is_reported_clearly() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let agent = make_agent().expect("agent material");
    // The QUIC endpoint spawns its driver on the current runtime, so it must be
    // constructed inside the runtime context.
    let _enter = runtime.enter();
    let endpoint = bind(&agent).expect("bind");
    let addr = endpoint.local_addr().unwrap().to_string();
    let _stub = runtime.spawn(run_stub(endpoint, agent, false));

    let dir = tempfile::tempdir().expect("tempdir");
    let error = match runtime.block_on(pair(PairConfig {
        addr,
        code: "000000".to_string(),
        client_name: "loopback-test".to_string(),
        state_dir: dir.path().join("agent"),
        expect_fingerprint: None,
    })) {
        Ok(_) => panic!("pairing must fail with a wrong code"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(message.contains("配对码错误"), "{message}");
    assert!(!state::is_paired(&dir.path().join("agent")));
}

#[test]
fn audio_datagrams_are_consumed_without_dropping_the_channel() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let agent = make_agent().expect("agent material");
    // The QUIC endpoint spawns its driver on the current runtime, so it must be
    // constructed inside the runtime context.
    let _enter = runtime.enter();
    let endpoint = bind(&agent).expect("bind");
    let addr = endpoint.local_addr().unwrap().to_string();
    let _stub = runtime.spawn(run_stub(endpoint, agent.clone(), true));

    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("agent");
    runtime
        .block_on(pair(PairConfig {
            addr: addr.clone(),
            code: PAIR_CODE.to_string(),
            client_name: "audio-test".to_string(),
            state_dir: state_dir.clone(),
            expect_fingerprint: Some(agent.fingerprint.clone()),
        }))
        .expect("pair");

    let mut session = AgentSession::start(SessionConfig {
        addr,
        client_name: "audio-test".to_string(),
        state_dir,
        audio: true,
    })
    .expect("start session");

    let connected = wait_for(&session, std::time::Duration::from_secs(10), |event| {
        matches!(event, AgentEvent::Connected { .. }).then_some(())
    });
    assert!(connected.is_some(), "no Connected event");

    // Either a device is available and playback starts, or the device is
    // missing and the failure is reported as a *degraded feature*, not as a
    // dead channel. Both are acceptable; a transport error is not.
    let outcome = wait_for(
        &session,
        std::time::Duration::from_secs(10),
        |event| match event {
            AgentEvent::AudioStarted => Some(Ok(())),
            AgentEvent::Error(message) if message.contains("音频输出不可用") => {
                Some(Err(message.clone()))
            }
            AgentEvent::Error(message) => Some(Err(format!("unexpected error: {message}"))),
            _ => None,
        },
    );
    match outcome {
        Some(Ok(())) => println!("audio playback started"),
        Some(Err(message)) if message.contains("音频输出不可用") => {
            println!("no audio device on this host ({message}); channel stayed up")
        }
        Some(Err(message)) => panic!("{message}"),
        None => panic!("audio datagrams were never processed"),
    }

    // The channel must still be alive after the audio burst.
    session.send_clipboard("still-here".to_string());
    let echo = wait_for(
        &session,
        std::time::Duration::from_secs(10),
        |event| match event {
            AgentEvent::Clipboard(text) => Some(text.clone()),
            _ => None,
        },
    );
    assert_eq!(echo.as_deref(), Some("mac:still-here"));
    session.stop();
}

/// A certificate that is not the one pinned at pairing time must be refused,
/// even though TLS name verification is bypassed (the agent certificate only
/// lists localhost while clients connect over LAN/Tailscale addresses).
#[test]
fn a_changed_agent_certificate_is_refused() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let agent = make_agent().expect("agent material");
    let _enter = runtime.enter();
    let endpoint = bind(&agent).expect("bind");
    let addr = endpoint.local_addr().unwrap().to_string();
    let _stub = runtime.spawn(run_stub(endpoint, agent.clone(), false));

    let dir = tempfile::tempdir().expect("tempdir");
    let state_dir = dir.path().join("agent");
    runtime
        .block_on(pair(PairConfig {
            addr: addr.clone(),
            code: PAIR_CODE.to_string(),
            client_name: "pin-test".to_string(),
            state_dir: state_dir.clone(),
            expect_fingerprint: None,
        }))
        .expect("pair");

    // Simulate the agent's certificate having changed under us.
    std::fs::write(
        state_dir.join("server-fingerprint.txt"),
        "0000000000000000000000000000000000000000000000000000000000000000",
    )
    .expect("rewrite pin");

    let mut session = AgentSession::start(SessionConfig {
        addr,
        client_name: "pin-test".to_string(),
        state_dir,
        audio: false,
    })
    .expect("start session");

    let error = wait_for(
        &session,
        std::time::Duration::from_secs(10),
        |event| match event {
            AgentEvent::Error(message) => Some(message.clone()),
            _ => None,
        },
    );
    let message = error.expect("expected an error for the changed certificate");
    assert!(
        message.contains("指纹") && message.contains("中间人"),
        "error should name the fingerprint mismatch: {message}"
    );
    session.stop();
}
