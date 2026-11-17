//! Encoding-negotiation regressions.
//!
//! Before the vendored engine patches, *any* encoding outside the built-in list
//! (Apple's 1104/1105/1109/1110, and even the standard zlib(6)) aborted the
//! session with the unhelpful "Image data cannot be decoded correctly" and,
//! with auto-reconnect on, produced a reconnect every second. These tests pin
//! the three behaviours that matter:
//!
//! 1. standard zlib rectangles are decoded and delivered as pixels;
//! 2. Apple pseudo-encodings are skipped without killing the session;
//! 3. a genuinely unknown encoding fails once, with the code in the message,
//!    and is not retried.

use std::io::Write;
use std::time::{Duration, Instant};

use rustvnc_client::config::ClientConfig;
use rustvnc_client::session::{Session, SessionErrorCode, SessionEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const WIDTH: u16 = 32;
const HEIGHT: u16 = 16;

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
    stream.write_all(&WIDTH.to_be_bytes()).await?;
    stream.write_all(&HEIGHT.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

fn update_header(encoding: i32) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&[0, 0]); // FramebufferUpdate
    buf.extend_from_slice(&1u16.to_be_bytes()); // one rectangle
    buf.extend_from_slice(&0u16.to_be_bytes()); // x
    buf.extend_from_slice(&0u16.to_be_bytes()); // y
    buf.extend_from_slice(&WIDTH.to_be_bytes());
    buf.extend_from_slice(&HEIGHT.to_be_bytes());
    buf.extend_from_slice(&encoding.to_be_bytes());
    buf
}

/// Serves requests; the closure returns the reply bytes for each update request.
async fn serve(
    stream: &mut TcpStream,
    mut on_request: impl FnMut(usize) -> Vec<u8>,
) -> anyhow::Result<()> {
    let mut seen = 0usize;
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
                let reply = on_request(seen);
                seen += 1;
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
    Ok(())
}

fn zlib_reply(pixels: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(pixels).expect("encode");
    let compressed = encoder.finish().expect("finish");
    let mut reply = update_header(6); // VncEncoding::Zlib
    reply.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
    reply.extend_from_slice(&compressed);
    reply
}

fn raw_reply() -> Vec<u8> {
    let mut reply = update_header(0);
    reply.extend(std::iter::repeat_n(
        0x7fu8,
        WIDTH as usize * HEIGHT as usize * 4,
    ));
    reply
}

async fn config_for(port: u16, auto_reconnect: bool) -> ClientConfig {
    ClientConfig {
        host: "127.0.0.1".to_string(),
        port,
        bpp: 32,
        auto_reconnect,
        ..ClientConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zlib_rectangles_are_decoded() {
    let pixels: Vec<u8> = (0..WIDTH as usize * HEIGHT as usize * 4)
        .map(|i| (i % 251) as u8)
        .collect();
    let expected = pixels.clone();
    let reply = zlib_reply(&pixels);

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        // The zlib stream is shared by the whole connection, so re-sending the
        // same compressed chunk would be invalid: send it once, then idle.
        serve(&mut stream, move |seen| {
            if seen == 0 {
                reply.clone()
            } else {
                vec![0, 0, 0, 0]
            }
        })
        .await
        .expect("serve");
    });

    let mut session =
        Session::start(config_for(addr.port(), false).await, String::new()).expect("start");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got = None;
    while Instant::now() < deadline && got.is_none() {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::RawImage { data, .. } => got = Some(data),
                SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    session.close();
    let _ = server.await;

    let data = got.expect("no frame decoded from a zlib rectangle");
    assert_eq!(data.len(), expected.len(), "decoded size");
    assert_eq!(data, expected, "decoded pixels");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn apple_pseudo_encodings_do_not_kill_the_session() {
    for code in [1104i32, 1105, 1107, 1109, 1110] {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            handshake(&mut stream).await.expect("handshake");
            serve(&mut stream, move |seen| {
                if seen == 0 {
                    // Apple pseudo-encoding: u16 length + opaque body.
                    let body = b"apple-meta";
                    let mut reply = update_header(code);
                    reply.extend_from_slice(&(body.len() as u16).to_be_bytes());
                    reply.extend_from_slice(body);
                    reply
                } else {
                    raw_reply()
                }
            })
            .await
            .expect("serve");
        });

        // `apple_encodings` stays false: macOS is known to send these even when
        // they were never advertised, and that must not be fatal.
        let mut session =
            Session::start(config_for(addr.port(), false).await, String::new()).expect("start");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut got_frame = false;
        while Instant::now() < deadline && !got_frame {
            while let Some(event) = session.try_recv() {
                match event {
                    SessionEvent::RawImage { .. } => got_frame = true,
                    SessionEvent::Error { message, .. } => {
                        panic!("encoding {code} aborted the session: {message}")
                    }
                    _ => {}
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        session.close();
        let _ = server.await;
        assert!(got_frame, "encoding {code} left the session unable to draw");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_encoding_fails_once_without_reconnect_loop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        serve(&mut stream, move |_| {
            let mut reply = update_header(1234);
            reply.extend_from_slice(&[0u8; 64]);
            reply
        })
        .await
        .expect("serve");
    });

    let mut session =
        Session::start(config_for(addr.port(), true).await, String::new()).expect("start");
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut errors = Vec::new();
    let mut reconnects = 0usize;
    let mut closed = false;
    while Instant::now() < deadline && !closed {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::Error {
                    code,
                    message,
                    retryable,
                } => {
                    assert_eq!(code, SessionErrorCode::Protocol);
                    assert!(!retryable, "unsupported encoding must not be retried");
                    assert!(
                        message.contains("1234"),
                        "error should name the encoding: {message}"
                    );
                    errors.push(message);
                }
                SessionEvent::Reconnecting { .. } => reconnects += 1,
                SessionEvent::Closed => closed = true,
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    session.close();
    let _ = server.await;

    assert_eq!(
        errors.len(),
        1,
        "expected exactly one error, got {errors:?}"
    );
    assert_eq!(reconnects, 0, "unsupported encoding must not reconnect");
    assert!(closed, "session should close after a permanent error");
}
