//! Clipboard over RFB: the wire format is documented but was never asserted.
//!
//! `ClientCutText` used to be read-and-discarded by every mock server in the
//! suite, and no test ever sent `ServerCutText`, so the two paths that make
//! "剪贴板双向" true for third-party servers had no automated evidence at all.
//! These tests pin the framing (message type, padding, `u32` length, payload
//! bytes in order) in both directions.

use std::time::{Duration, Instant};

use rustvnc_client::config::{AuthMode, ClientConfig, QualityPreset};
use rustvnc_client::session::{Session, SessionEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vnc::X11Event;

/// Text that is not ASCII: it exercises the byte-length header rather than a
/// character count, and the fact that the client sends UTF-8 bytes verbatim.
const SAMPLE: &str = "剪贴板 ✓ clipboard";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn client_cut_text_uses_the_documented_wire_format() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        serve_until_clipboard(stream, tx).await;
    });

    let mut session = Session::start(mock_config(addr.port()), String::new()).expect("start");
    wait_for_connected(&session);
    session.send_input(X11Event::CopyText(SAMPLE.to_string()));

    let received = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("server never saw ClientCutText")
        .expect("server task dropped the channel");
    // Byte count, not character count: the header is a byte length and the
    // payload is the UTF-8 encoding of the string.
    assert_eq!(
        received,
        SAMPLE.as_bytes(),
        "payload must be the UTF-8 bytes of the clipboard text"
    );
    session.close();
    server.await.expect("server");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_cut_text_arrives_as_a_clipboard_event() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");

    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        // ServerCutText: type 3, three padding bytes, u32 byte length, text.
        stream.write_all(&[3, 0, 0, 0]).await.expect("cut type");
        stream
            .write_all(&(SAMPLE.len() as u32).to_be_bytes())
            .await
            .expect("cut length");
        stream.write_all(SAMPLE.as_bytes()).await.expect("cut body");
        // Keep answering update requests so the session stays healthy.
        serve_empty_updates(&mut stream).await;
    });

    let mut session = Session::start(mock_config(addr.port()), String::new()).expect("start");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut clipboard = None;
    while Instant::now() < deadline && clipboard.is_none() {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::Clipboard(text) => clipboard = Some(text),
                SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        clipboard.as_deref(),
        Some(SAMPLE),
        "ServerCutText must surface as SessionEvent::Clipboard"
    );
    session.close();
    server.await.expect("server");
}

fn mock_config(port: u16) -> ClientConfig {
    ClientConfig {
        host: "127.0.0.1".to_string(),
        port,
        quality: QualityPreset::High,
        bpp: 32,
        auth: AuthMode::Auto,
        ..ClientConfig::default()
    }
}

fn wait_for_connected(session: &Session) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::Connected { .. } => return,
                SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("no Connected event");
}

async fn serve_until_clipboard(
    mut stream: TcpStream,
    clipboard: tokio::sync::oneshot::Sender<Vec<u8>>,
) {
    handshake(&mut stream).await.expect("handshake");
    loop {
        let mut kind = [0u8; 1];
        if stream.read_exact(&mut kind).await.is_err() {
            return;
        }
        match kind[0] {
            0 => {
                let mut rest = [0u8; 19];
                stream.read_exact(&mut rest).await.expect("pixel format");
            }
            2 => {
                let mut header = [0u8; 3];
                stream.read_exact(&mut header).await.expect("encodings");
                let count = u16::from_be_bytes([header[1], header[2]]) as usize;
                let mut encodings = vec![0u8; count * 4];
                stream
                    .read_exact(&mut encodings)
                    .await
                    .expect("encoding list");
            }
            3 => {
                let mut rest = [0u8; 9];
                stream.read_exact(&mut rest).await.expect("update request");
                // Empty update keeps the request loop moving.
                stream.write_all(&[0, 0, 0, 0]).await.expect("empty update");
            }
            4 => {
                let mut rest = [0u8; 7];
                stream.read_exact(&mut rest).await.expect("key event");
            }
            5 => {
                let mut rest = [0u8; 5];
                stream.read_exact(&mut rest).await.expect("pointer event");
            }
            6 => {
                let mut padding = [0u8; 3];
                stream
                    .read_exact(&mut padding)
                    .await
                    .expect("cut text padding");
                let mut length = [0u8; 4];
                stream
                    .read_exact(&mut length)
                    .await
                    .expect("cut text length");
                let size = u32::from_be_bytes(length) as usize;
                let mut text = vec![0u8; size];
                stream.read_exact(&mut text).await.expect("cut text body");
                let _ = clipboard.send(text);
                return;
            }
            other => panic!("unexpected client message {other}"),
        }
    }
}

async fn serve_empty_updates(stream: &mut TcpStream) {
    loop {
        let mut kind = [0u8; 1];
        if stream.read_exact(&mut kind).await.is_err() {
            return;
        }
        match kind[0] {
            0 => {
                let mut rest = [0u8; 19];
                if stream.read_exact(&mut rest).await.is_err() {
                    return;
                }
            }
            2 => {
                let mut header = [0u8; 3];
                if stream.read_exact(&mut header).await.is_err() {
                    return;
                }
                let count = u16::from_be_bytes([header[1], header[2]]) as usize;
                let mut encodings = vec![0u8; count * 4];
                if stream.read_exact(&mut encodings).await.is_err() {
                    return;
                }
            }
            3 => {
                let mut rest = [0u8; 9];
                if stream.read_exact(&mut rest).await.is_err() {
                    return;
                }
                if stream.write_all(&[0, 0, 0, 0]).await.is_err() {
                    return;
                }
            }
            4 => {
                let mut rest = [0u8; 7];
                if stream.read_exact(&mut rest).await.is_err() {
                    return;
                }
            }
            5 => {
                let mut rest = [0u8; 5];
                if stream.read_exact(&mut rest).await.is_err() {
                    return;
                }
            }
            6 => {
                let mut header = [0u8; 3];
                if stream.read_exact(&mut header).await.is_err() {
                    return;
                }
                let mut length = [0u8; 4];
                if stream.read_exact(&mut length).await.is_err() {
                    return;
                }
                let size = u32::from_be_bytes(length) as usize;
                let mut text = vec![0u8; size];
                if stream.read_exact(&mut text).await.is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

async fn handshake(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;
    stream.write_all(&[1, 1]).await?; // one security type: None
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen).await?;
    stream.write_all(&0u32.to_be_bytes()).await?; // SecurityResult OK
    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared).await?;
    stream.write_all(&32u16.to_be_bytes()).await?;
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

fn pixel_format() -> [u8; 16] {
    [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]
}
