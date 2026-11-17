//! The remote cursor shape must reach the UI.
//!
//! The engine decodes `CursorPseudo` (-239) into an RGBA-ish buffer with the
//! server's mask applied to the alpha byte; the session used to drop it, so the
//! client could only ever show the local Windows arrow (no I-beam, no resize
//! cursor). These tests pin the plumbing and the mask handling.

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
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

/// A `FramebufferUpdate` carrying one cursor pseudo-rectangle.
///
/// `mask_rows` has one bit-per-pixel byte per row (bit 7 = x 0), as RFB defines.
fn cursor_update(width: u16, height: u16, hotspot: (u16, u16), mask_rows: &[u8]) -> Vec<u8> {
    let mut bytes = vec![0, 0];
    bytes.extend_from_slice(&1u16.to_be_bytes()); // one rectangle
    bytes.extend_from_slice(&hotspot.0.to_be_bytes());
    bytes.extend_from_slice(&hotspot.1.to_be_bytes());
    bytes.extend_from_slice(&width.to_be_bytes());
    bytes.extend_from_slice(&height.to_be_bytes());
    bytes.extend_from_slice(&(-239i32).to_be_bytes()); // CursorPseudo
    let pixels = width as usize * height as usize * 4;
    bytes.extend(std::iter::repeat_n(0u8, pixels));
    bytes.extend_from_slice(mask_rows);
    bytes
}

async fn run_session(responses: Vec<Vec<u8>>) -> Vec<SessionEvent> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        let mut index = 0;
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
                    let reply = responses
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| vec![0, 0, 0, 0]);
                    index += 1;
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

    let config = ClientConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        bpp: 32,
        auto_reconnect: false,
        ..ClientConfig::default()
    };
    let mut session = Session::start(config, String::new()).expect("start");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    while Instant::now() < deadline
        && seen
            .iter()
            .filter(|event: &&SessionEvent| matches!(event, SessionEvent::Cursor { .. }))
            .count()
            < 2
    {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                other => seen.push(other),
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    session.close();
    let _ = server.await;
    seen
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cursor_shape_and_mask_reach_the_ui() {
    // 2x2 cursor at hotspot (1,1). Mask 0xC0 marks both pixels of row 0 visible,
    // row 1 is fully transparent.
    let shape = cursor_update(2, 2, (1, 1), &[0xC0, 0x00]);
    let cleared = cursor_update(0, 0, (0, 0), &[]);
    let events = run_session(vec![shape, cleared]).await;

    let cursors: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            SessionEvent::Cursor {
                hotspot,
                size,
                rgba,
            } => Some((*hotspot, *size, rgba.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(cursors.len(), 2, "expected a shape and a clear");

    let (hotspot, size, rgba) = &cursors[0];
    assert_eq!(*hotspot, (1, 1), "hotspot comes from the rectangle origin");
    assert_eq!(*size, (2, 2));
    assert_eq!(rgba.len(), 2 * 2 * 4);
    // Alpha follows the mask: row 0 visible, row 1 not.
    assert_eq!([rgba[3], rgba[7], rgba[11], rgba[15]], [255, 255, 0, 0]);

    let (_, size, rgba) = &cursors[1];
    assert_eq!(*size, (0, 0), "a zero-size cursor means 'no shape'");
    assert!(rgba.is_empty());
}
