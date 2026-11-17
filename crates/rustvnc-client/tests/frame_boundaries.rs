//! The UI presents only when `FrameComplete` arrives, so that marker must come
//! exactly once per update and only after every rectangle of that update.
//!
//! Without it the client paints rectangles as they are decoded, which on a 5K
//! screen means the picture is drawn top to bottom in bands ("scanning").

use std::time::{Duration, Instant};

use rustvnc_client::config::ClientConfig;
use rustvnc_client::session::{Session, SessionEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn pixel_format() -> [u8; 16] {
    [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]
}

async fn handshake(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;
    stream.write_all(&[1, 1]).await?; // one security type: None
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen).await?;
    stream.write_all(&0u32.to_be_bytes()).await?;
    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared).await?;
    stream.write_all(&64u16.to_be_bytes()).await?;
    stream.write_all(&64u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

/// One update carrying `rects` horizontal bands of 64x4 pixels.
fn update_with_bands(rects: u16) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&rects.to_be_bytes());
    for band in 0..rects {
        bytes.extend_from_slice(&0u16.to_be_bytes()); // x
        bytes.extend_from_slice(&(band * 4).to_be_bytes()); // y
        bytes.extend_from_slice(&64u16.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&0i32.to_be_bytes()); // Raw
        bytes.extend(std::iter::repeat_n(0x40u8, 64 * 4 * 4));
    }
    bytes
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frame_marker_follows_every_rectangle_of_its_update() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        let mut update = 0;
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
                    // Two updates: 3 bands, then 1 band.
                    let rects = if update % 2 == 0 { 3 } else { 1 };
                    if update < 2 {
                        stream.write_all(&update_with_bands(rects)).await?;
                        stream.flush().await?;
                    } else {
                        stream.write_all(&[0, 0, 0, 0]).await?;
                        stream.flush().await?;
                    }
                    update += 1;
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

    let config = ClientConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        bpp: 32,
        auto_reconnect: false,
        ..ClientConfig::default()
    };
    let mut session = Session::start(config, String::new()).expect("start");

    let mut order = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && order.iter().filter(|e| **e == "C").count() < 2 {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::RawImage { .. } => order.push("R"),
                SessionEvent::FrameComplete => order.push("C"),
                SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    session.close();
    let _ = server.await;

    let observed: String = order.concat();
    assert!(
        observed.starts_with("RRRC"),
        "first update should be 3 rectangles then a marker, saw {observed}"
    );
    assert!(
        observed.contains("RC"),
        "second update should end with a marker, saw {observed}"
    );
    assert_eq!(
        observed.matches('C').count(),
        2,
        "exactly one marker per update: {observed}"
    );
}
