use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;

const CHALLENGE: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
];
const EXPECTED_AUTH: [u8; 16] = [
    0xf1, 0x9b, 0x50, 0x47, 0x1f, 0x60, 0xf4, 0x22, 0x98, 0xe5, 0xc0, 0x14, 0x7d, 0xb5, 0x0e, 0x1e,
];

#[tokio::test]
async fn probe_authenticates_captures_and_sends_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let events = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_events = Arc::clone(&events);

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        run_mock_server(stream, server_events)
            .await
            .expect("server");
    });

    let dir = tempfile::tempdir().expect("tempdir");
    let png = dir.path().join("frame.png");
    let output = Command::new(env!("CARGO_BIN_EXE_rfb-probe"))
        .args([
            "--host",
            "127.0.0.1",
            "--port",
            &addr.port().to_string(),
            "--auth",
            "vnc",
            "--password",
            "secret",
            "--seconds",
            "1",
            "--save",
            png.to_str().expect("png path"),
            "--input-test",
        ])
        .output()
        .await
        .expect("run probe");

    assert!(
        output.status.success(),
        "probe failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(png.exists(), "PNG was not written");
    assert!(png.metadata().expect("metadata").len() > 100);

    server.await.expect("server task");
    let events = events.lock().expect("events");
    assert!(
        events.iter().any(|event| event == "pointer"),
        "no pointer: {events:?}"
    );
    assert!(
        events.iter().any(|event| event == "key"),
        "no key: {events:?}"
    );
}

async fn run_mock_server(
    mut stream: TcpStream,
    events: Arc<Mutex<Vec<String>>>,
) -> anyhow::Result<()> {
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;

    stream.write_all(&[1, 2]).await?; // one security type: VNC auth
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen).await?;
    anyhow::ensure!(chosen[0] == 2, "unexpected security type {}", chosen[0]);

    stream.write_all(&CHALLENGE).await?;
    let mut response = [0u8; 16];
    stream.read_exact(&mut response).await?;
    if response != EXPECTED_AUTH {
        stream.write_all(&1u32.to_be_bytes()).await?;
        anyhow::bail!("wrong VNC auth response");
    }
    stream.write_all(&0u32.to_be_bytes()).await?;

    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared).await?;

    stream.write_all(&64u16.to_be_bytes()).await?;
    stream.write_all(&64u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;

    let mut sent_full_frame = false;
    loop {
        let mut kind = [0u8; 1];
        if stream.read_exact(&mut kind).await.is_err() {
            break;
        }
        match kind[0] {
            0 => {
                // SetPixelFormat: 3 padding + 16 byte format
                let mut rest = [0u8; 19];
                stream.read_exact(&mut rest).await?;
            }
            2 => {
                // SetEncodings: 1 padding + u16 count + count * i32
                let mut header = [0u8; 3];
                stream.read_exact(&mut header).await?;
                let count = u16::from_be_bytes([header[1], header[2]]) as usize;
                let mut encodings = vec![0u8; count * 4];
                stream.read_exact(&mut encodings).await?;
            }
            3 => {
                // FramebufferUpdateRequest: incremental + x/y/w/h
                let mut rest = [0u8; 9];
                stream.read_exact(&mut rest).await?;
                if sent_full_frame {
                    stream.write_all(&[0, 0, 0, 0]).await?;
                } else {
                    send_full_frame(&mut stream).await?;
                    sent_full_frame = true;
                }
            }
            4 => {
                // KeyEvent: down + 2 padding + keysym
                let mut rest = [0u8; 7];
                stream.read_exact(&mut rest).await?;
                events.lock().expect("events").push("key".to_string());
            }
            5 => {
                // PointerEvent: button mask + x + y
                let mut rest = [0u8; 5];
                stream.read_exact(&mut rest).await?;
                events.lock().expect("events").push("pointer".to_string());
            }
            6 => {
                // ClientCutText: 3 padding + u32 length + text
                let mut header = [0u8; 3];
                stream.read_exact(&mut header).await?;
                let mut length = [0u8; 4];
                stream.read_exact(&mut length).await?;
                let mut text = vec![0u8; u32::from_be_bytes(length) as usize];
                stream.read_exact(&mut text).await?;
            }
            other => anyhow::bail!("unexpected client message {other}"),
        }
    }
    Ok(())
}

async fn send_full_frame(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream.write_all(&[0, 0]).await?; // FramebufferUpdate, padding
    stream.write_all(&1u16.to_be_bytes()).await?; // one rect
    stream.write_all(&0u16.to_be_bytes()).await?; // x
    stream.write_all(&0u16.to_be_bytes()).await?; // y
    stream.write_all(&64u16.to_be_bytes()).await?; // w
    stream.write_all(&64u16.to_be_bytes()).await?; // h
    stream.write_all(&0i32.to_be_bytes()).await?; // Raw

    let mut pixels = Vec::with_capacity(64 * 64 * 4);
    for y in 0..64u8 {
        for x in 0..64u8 {
            pixels.push(x.wrapping_mul(4)); // B
            pixels.push(y.wrapping_mul(4)); // G
            pixels.push(128); // R
            pixels.push(255); // A
        }
    }
    stream.write_all(&pixels).await?;
    Ok(())
}

fn pixel_format() -> [u8; 16] {
    [
        32, 24, 0, 1, // bpp, depth, big-endian, true-colour
        0, 255, // red-max
        0, 255, // green-max
        0, 255, // blue-max
        16, 8, 0, // shifts
        0, 0, 0, // padding
    ]
}
