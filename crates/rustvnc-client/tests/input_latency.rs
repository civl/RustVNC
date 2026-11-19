//! Input-to-pixel latency benchmark.
//!
//! Real RFB servers only send pixels when asked, so the client's own request
//! pacing is part of the latency budget: a click whose repaint waits for the
//! next idle poll can be a quarter of a second late even on a perfect network.
//!
//! The mock server here mirrors that: it records a pointer event as "dirty" and
//! answers the next `FramebufferUpdateRequest` with pixels. Each iteration starts
//! from a backed-off (idle) client, so this measures the worst case rather than
//! the streaming one.
//!
//! Run with `--nocapture` to see the percentiles:
//! `cargo test -p rustvnc-client --test input_latency -- --nocapture`

use std::time::{Duration, Instant};

use rustvnc_client::config::ClientConfig;
use rustvnc_client::session::{Session, SessionEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vnc::X11Event;

const FRAMEBUFFER: (u16, u16) = (800, 600);
const ITERATIONS: usize = 12;
/// Idle gaps between probes, long enough for the controller to back off.
const IDLE_GAP: Duration = Duration::from_millis(120);
/// Budget for the 95th percentile. Measured values: ~1 ms when input wakes the
/// request loop, ~47 ms (one poll interval) when it does not, so this separates
/// the two cases with a wide margin.
const P95_BUDGET_MS: f64 = 20.0;

/// Linear-interpolated percentile. Nearest-rank makes p95 equal to the maximum
/// for a 12-sample run, so a single scheduler hiccup (one 23 ms sample on a
/// shared CI machine) fails a test whose median is 1.2 ms.
fn percentile(sorted: &[f64], percent: f64) -> f64 {
    let rank = (percent / 100.0) * (sorted.len().saturating_sub(1)) as f64;
    let low = rank.floor() as usize;
    let high = rank.ceil() as usize;
    if low == high {
        return sorted[low];
    }
    sorted[low] + (sorted[high] - sorted[low]) * (rank - low as f64)
}

fn pixel_format() -> [u8; 16] {
    [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]
}

async fn handshake(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;
    stream.write_all(&[1, 1]).await?; // security type None
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen).await?;
    stream.write_all(&0u32.to_be_bytes()).await?;
    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared).await?;
    stream.write_all(&FRAMEBUFFER.0.to_be_bytes()).await?;
    stream.write_all(&FRAMEBUFFER.1.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

/// One 4x4 Raw rectangle whose colour changes every time, so the client always
/// decodes real pixels (an identical rect would be a valid but empty-looking
/// update on some paths).
fn dirty_update(seed: u8) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes());
    bytes.extend_from_slice(&4u16.to_be_bytes());
    bytes.extend_from_slice(&4u16.to_be_bytes());
    bytes.extend_from_slice(&0i32.to_be_bytes()); // Raw
    for _ in 0..4 * 4 {
        bytes.extend_from_slice(&[seed, 0xff - seed, 0x40, 0xff]);
    }
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn input_after_an_idle_stretch_refreshes_immediately() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        let mut dirty = false;
        let mut seed = 0u8;
        let mut pointer_at: Option<Instant> = None;
        let mut gaps_ms: Vec<f64> = Vec::new();
        loop {
            let mut kind = [0u8; 1];
            if stream.read_exact(&mut kind).await.is_err() {
                break;
            }
            match kind[0] {
                0 => {
                    let mut rest = [0u8; 19];
                    stream.read_exact(&mut rest).await?;
                }
                2 => {
                    let mut header = [0u8; 3];
                    stream.read_exact(&mut header).await?;
                    let count = u16::from_be_bytes([header[1], header[2]]) as usize;
                    let mut encodings = vec![0u8; count * 4];
                    stream.read_exact(&mut encodings).await?;
                }
                3 => {
                    let mut rest = [0u8; 9];
                    stream.read_exact(&mut rest).await?;
                    // Only here may pixels be sent, exactly like a real server.
                    if dirty {
                        dirty = false;
                        if let Some(at) = pointer_at.take() {
                            gaps_ms.push(at.elapsed().as_secs_f64() * 1000.0);
                        }
                        seed = seed.wrapping_add(17);
                        stream.write_all(&dirty_update(seed)).await?;
                    } else {
                        stream.write_all(&[0, 0, 0, 0]).await?;
                    }
                    stream.flush().await?;
                }
                4 => {
                    let mut rest = [0u8; 7];
                    stream.read_exact(&mut rest).await?;
                }
                5 => {
                    let mut rest = [0u8; 5];
                    stream.read_exact(&mut rest).await?;
                    pointer_at = Some(Instant::now());
                    dirty = true;
                }
                6 => {
                    let mut header = [0u8; 3];
                    stream.read_exact(&mut header).await?;
                    let mut length = [0u8; 4];
                    stream.read_exact(&mut length).await?;
                    let mut text = vec![0u8; u32::from_be_bytes(length) as usize];
                    stream.read_exact(&mut text).await?;
                }
                _ => break,
            }
        }
        Ok::<_, anyhow::Error>(gaps_ms)
    });

    let config = ClientConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        bpp: 32,
        auto_reconnect: false,
        ..ClientConfig::default()
    };
    let mut session = Session::start(config, String::new()).expect("start");

    // Let the controller settle into its idle cadence before probing.
    tokio::time::sleep(Duration::from_millis(600)).await;
    while session.try_recv().is_some() {}

    let mut samples = Vec::with_capacity(ITERATIONS);
    for index in 0..ITERATIONS {
        let start = Instant::now();
        session.send_input(X11Event::PointerEvent(
            ((index as u16 % 100) * 4, 8, 0).into(),
        ));
        let deadline = start + Duration::from_secs(3);
        let mut seen = false;
        while Instant::now() < deadline {
            while let Some(event) = session.try_recv() {
                match event {
                    SessionEvent::RawImage { .. } if !seen => {
                        samples.push(start.elapsed());
                        seen = true;
                    }
                    SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                    _ => {}
                }
            }
            if seen {
                break;
            }
            // A `sleep(1ms)` would round up to the 15.6 ms Windows timer tick and
            // pollute the measurement; yield instead.
            tokio::task::yield_now().await;
        }
        assert!(seen, "no pixels arrived after input {index}");
        tokio::time::sleep(IDLE_GAP).await;
    }
    session.close();
    let _ = server.await;

    let mut millis: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1000.0).collect();
    millis.sort_by(f64::total_cmp);
    let p50 = percentile(&millis, 50.0);
    let p95 = percentile(&millis, 95.0);
    let max = *millis.last().expect("samples");
    println!(
        "input->pixels: p50 {p50:.1} ms · p95 {p95:.1} ms · max {max:.1} ms · n={}",
        millis.len()
    );
    println!(
        "samples: {}",
        millis
            .iter()
            .map(|m| format!("{m:.0}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert!(
        p95 < P95_BUDGET_MS,
        "p95 {p95:.1} ms exceeds the {P95_BUDGET_MS} ms budget: input is waiting for the idle \
         poll instead of requesting a frame (p50 {p50:.1} ms, max {max:.1} ms)"
    );
}
