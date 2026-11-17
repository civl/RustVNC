//! Adaptive request cadence.
//!
//! The client polls the server at the configured rate. On a 5K screen that is
//! expensive for the server even when nothing moves, so the session backs off to
//! a slow trickle after a few pixel-less frames and snaps back as soon as a
//! frame carries pixels.
//!
//! These tests count the requests the server actually receives; the difference
//! between the two scenarios is what tells a working controller from a fixed
//! interval.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustvnc_client::config::{ClientConfig, QualityPreset};
use rustvnc_client::session::Session;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Window long enough for the backoff to engage (3 quiet frames) and settle.
const WINDOW: Duration = Duration::from_millis(1200);

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
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

/// An update with one 16x4 Raw rectangle, i.e. pixels that really changed.
fn content_update() -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&0u16.to_be_bytes()); // x
    bytes.extend_from_slice(&0u16.to_be_bytes()); // y
    bytes.extend_from_slice(&16u16.to_be_bytes());
    bytes.extend_from_slice(&4u16.to_be_bytes());
    bytes.extend_from_slice(&0i32.to_be_bytes()); // Raw
    bytes.extend(std::iter::repeat_n(0x20u8, 16 * 4 * 4));
    bytes
}

/// Runs a session against a mock server for `WINDOW` and returns how many
/// FramebufferUpdateRequests it received.
async fn count_requests(respond_with_content: bool) -> usize {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let count = Arc::new(Mutex::new(0usize));
    let server_count = Arc::clone(&count);

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
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
                    let n = u16::from_be_bytes([header[1], header[2]]) as usize;
                    let mut encodings = vec![0u8; n * 4];
                    stream.read_exact(&mut encodings).await?;
                }
                3 => {
                    let mut rest = [0u8; 9];
                    stream.read_exact(&mut rest).await?;
                    *server_count.lock().unwrap() += 1;
                    let reply = if respond_with_content {
                        content_update()
                    } else {
                        vec![0, 0, 0, 0]
                    };
                    stream.write_all(&reply).await?;
                    stream.flush().await?;
                }
                4 => {
                    let mut rest = [0u8; 7];
                    stream.read_exact(&mut rest).await?;
                }
                5 => {
                    let mut rest = [0u8; 5];
                    stream.read_exact(&mut rest).await?;
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
        Ok::<_, anyhow::Error>(())
    });

    // 30 fps cap: without backoff this is ~36 requests per second.
    let config = ClientConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        bpp: 32,
        quality: QualityPreset::Custom,
        custom_fps: 30,
        auto_reconnect: false,
        ..ClientConfig::default()
    };
    let mut session = Session::start(config, String::new()).expect("start");
    let deadline = Instant::now() + WINDOW;
    while Instant::now() < deadline {
        while session.try_recv().is_some() {}
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    session.close();
    let _ = server.await;
    *count.lock().unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_static_screen_is_polled_slowly() {
    let requests = count_requests(false).await;
    // Active rate would be ~36 in this window; the idle cadence is 4 fps, so a
    // working controller lands around 6-10 including the initial frames.
    assert!(
        requests < 20,
        "{requests} requests in {WINDOW:?} on a static screen: no backoff happened"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_changing_screen_keeps_the_configured_rate() {
    let requests = count_requests(true).await;
    // Every reply carries pixels, so the controller must stay at 30 fps.
    assert!(
        requests > 25,
        "{requests} requests in {WINDOW:?} while the screen was changing: \
         the backoff never released"
    );
}
