//! Diagnostic for the agent channel.
//!
//! ```text
//! cargo run -p rustvnc-agent-client --example agent_probe -- <host:port> [pairing-code] [seconds] [noaudio] [WxH]
//! ```
//!
//! * with no code: only the QUIC/TLS handshake is attempted (pairing AcceptAny),
//!   which tells you whether the agent is listening and speaks our ALPN;
//! * with a code: full pairing, then a short session that echoes a clipboard
//!   round trip and reports whether audio started.
//!
//! Exit code 0 means the agent answered; 1 means it did not.

use std::time::Duration;

use anyhow::Result;
use rustvnc_agent_client::{
    AgentEvent, AgentSession, PairConfig, SessionConfig, default_state_dir, pair,
};

#[tokio::main]
async fn main() -> Result<()> {
    // RUST_LOG=rustvnc_agent_client=debug surfaces datagram/decode diagnostics.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    let addr = args
        .first()
        .cloned()
        .unwrap_or_else(|| "127.0.0.1:8443".to_string());
    let code = args.get(1).cloned().unwrap_or_default();
    let seconds: u64 = args
        .get(2)
        .and_then(|value| value.parse().ok())
        .unwrap_or(6);
    // `noaudio` as the 4th argument asks the agent not to start capture, which
    // isolates audio-init stalls (TCC prompts) from control-plane problems.
    let audio = args.get(3).map(|value| value != "noaudio").unwrap_or(true);
    // The 5th argument is either a resolution ("2560x1440") or a layout
    // ("single" / "multi"), so one probe run can exercise either feature.
    let mode = args.get(4).cloned();
    let display = mode.as_ref().and_then(|value| {
        let (width, height) = value.split_once('x')?;
        Some((width.parse::<u16>().ok()?, height.parse::<u16>().ok()?))
    });
    let layout = mode
        .as_deref()
        .filter(|value| *value == "single" || *value == "multi")
        .map(str::to_string);
    let state_dir = default_state_dir();
    println!("addr      {addr}");
    println!("state dir {}", state_dir.display());
    println!(
        "paired    {}",
        rustvnc_agent_client::state::is_paired(&state_dir)
    );

    let already_paired = rustvnc_agent_client::state::is_paired(&state_dir);
    if code.is_empty() && !already_paired {
        // No code and no stored material: probe reachability only. QUIC runs over
        // UDP, so a TCP "port closed" result says nothing — the handshake is the
        // real test.
        println!("no pairing code and not paired: handshake-only probe");
        let (host, port) = rustvnc_agent_client::tls::split_addr(&addr)?;
        let remote = tokio::net::lookup_host((host.as_str(), port))
            .await?
            .next()
            .ok_or_else(|| anyhow::anyhow!("cannot resolve {host}"))?;
        rustvnc_agent_client::tls::install_provider();
        let mut endpoint = quinn::Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(rustvnc_agent_client::tls::pairing_client_config()?);
        let connecting = endpoint.connect(remote, &host)?;
        match tokio::time::timeout(Duration::from_secs(10), connecting).await {
            Ok(Ok(connection)) => {
                println!("QUIC/TLS HANDSHAKE OK");
                println!(
                    "  presented fingerprint {:?}",
                    rustvnc_agent_client::tls::peer_fingerprint(&connection)
                );
                connection.close(0u32.into(), b"probe");
                endpoint.wait_idle().await;
            }
            Ok(Err(err)) => println!("HANDSHAKE FAILED: {err}"),
            Err(_) => println!("HANDSHAKE TIMED OUT (10s) — nothing is listening on UDP {port}"),
        }
        return Ok(());
    }

    if !code.is_empty() {
        println!("pairing …");
        match pair(PairConfig {
            addr: addr.clone(),
            code: code.clone(),
            client_name: "agent_probe".to_string(),
            state_dir: state_dir.clone(),
            expect_fingerprint: None,
        })
        .await
        {
            Ok(outcome) => {
                println!("PAIRED");
                println!("  announced fingerprint {}", outcome.server_fingerprint);
                println!(
                    "  presented fingerprint {:?}",
                    outcome.tls_fingerprint.as_deref().unwrap_or("<none>")
                );
            }
            Err(err) => {
                println!("PAIRING FAILED: {err:#}");
                return Ok(());
            }
        }
    }

    let mut session = AgentSession::start(SessionConfig {
        addr,
        client_name: "agent_probe".to_string(),
        state_dir,
        audio,
    })?;

    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    let mut last_beat = 0u64;
    let mut sent_clipboard = false;
    let mut sent_display = false;
    let mut sent_layout = false;
    let mut connected = false;
    while std::time::Instant::now() < deadline {
        while let Some(event) = session.try_recv() {
            match event {
                AgentEvent::Connected {
                    agent,
                    capabilities,
                } => {
                    println!("CONNECTED agent={agent} capabilities={capabilities:#x}");
                    connected = true;
                }
                AgentEvent::Clipboard(text) => {
                    println!("CLIPBOARD from agent: {} bytes", text.len())
                }
                AgentEvent::DisplayLayoutResult {
                    single,
                    applied,
                    message,
                } => println!("LAYOUT single={single} applied={applied}: {message}"),
                AgentEvent::DisplayModeResult {
                    width,
                    height,
                    applied,
                    message,
                } => println!("DISPLAY {width}x{height} applied={applied}: {message}"),
                AgentEvent::AudioStarted => println!("AUDIO started"),
                AgentEvent::AudioStopped => println!("AUDIO stopped"),
                AgentEvent::Error(message) => println!("ERROR {message}"),
                AgentEvent::Closed => println!("CLOSED"),
            }
        }
        if connected && !sent_clipboard {
            session.send_clipboard("rustvnc-agent-probe".to_string());
            sent_clipboard = true;
        }
        if connected
            && !sent_display
            && let Some((width, height)) = display
        {
            session.set_display_mode(width, height);
            sent_display = true;
        }
        if connected
            && !sent_layout
            && let Some(layout) = &layout
        {
            session.set_display_layout(layout == "single");
            println!("requesting layout: {layout}");
            sent_layout = true;
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed >= last_beat + 10 {
            last_beat = elapsed;
            println!("… {elapsed}s connected={connected}");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    session.stop();
    println!("done (connected={connected})");
    Ok(())
}
