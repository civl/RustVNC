//! Frame pacing: no more than one *session* request may be outstanding.
//!
//! The refresh ticker used to fire every `1000 / fps` ms regardless of whether
//! the previous update had arrived. On a slow 5K link that queues several
//! overlapping updates, and since each one is decoded and painted as its
//! rectangles land, the screen visibly tears while windows move.
//!
//! The mock server answers every request after a delay and records how many
//! requests were still unanswered at the moment each new one arrived. A correct
//! client keeps that number at one (plus the single request the engine itself
//! sends at connect, which this layer cannot see).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustvnc_client::config::{ClientConfig, QualityPreset};
use rustvnc_client::session::Session;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Delay before every reply, comfortably longer than one 30 fps tick.
const REPLY_DELAY: Duration = Duration::from_millis(150);
/// Sampling window: a few ticks long.
const WINDOW: Duration = Duration::from_millis(500);

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_do_not_pile_up() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let received = Arc::new(AtomicUsize::new(0));
    let replied = Arc::new(AtomicUsize::new(0));
    let max_outstanding = Arc::new(Mutex::new(0usize));

    let (received_srv, replied_srv, max_srv) = (
        Arc::clone(&received),
        Arc::clone(&replied),
        Arc::clone(&max_outstanding),
    );
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        // Split so a delayed reply never stops the read loop from timestamping
        // the next request.
        let (mut reader, mut writer) = stream.into_split();
        let (write_tx, mut write_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let replied_writer = Arc::clone(&replied_srv);
        let writer_task = tokio::spawn(async move {
            while let Some(bytes) = write_rx.recv().await {
                if writer.write_all(&bytes).await.is_err() {
                    break;
                }
                let _ = writer.flush().await;
                replied_writer.fetch_add(1, Ordering::SeqCst);
            }
        });

        loop {
            let mut kind = [0u8; 1];
            if reader.read_exact(&mut kind).await.is_err() {
                break;
            }
            match kind[0] {
                0 => {
                    let mut rest = [0u8; 19];
                    reader.read_exact(&mut rest).await?;
                }
                2 => {
                    let mut header = [0u8; 3];
                    reader.read_exact(&mut header).await?;
                    let count = u16::from_be_bytes([header[1], header[2]]) as usize;
                    let mut encodings = vec![0u8; count * 4];
                    reader.read_exact(&mut encodings).await?;
                }
                3 => {
                    let mut rest = [0u8; 9];
                    reader.read_exact(&mut rest).await?;
                    let outstanding = received_srv.fetch_add(1, Ordering::SeqCst) + 1
                        - replied_srv.load(Ordering::SeqCst);
                    {
                        let mut peak = max_srv.lock().unwrap();
                        *peak = (*peak).max(outstanding);
                    }
                    let reply = write_tx.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(REPLY_DELAY).await;
                        let _ = reply.send(vec![0, 0, 0, 0]);
                    });
                }
                4 => {
                    let mut rest = [0u8; 7];
                    reader.read_exact(&mut rest).await?;
                }
                5 => {
                    let mut rest = [0u8; 5];
                    reader.read_exact(&mut rest).await?;
                }
                6 => {
                    let mut header = [0u8; 3];
                    reader.read_exact(&mut header).await?;
                    let mut length = [0u8; 4];
                    reader.read_exact(&mut length).await?;
                    let mut text = vec![0u8; u32::from_be_bytes(length) as usize];
                    reader.read_exact(&mut text).await?;
                }
                _ => break,
            }
        }
        drop(write_tx);
        let _ = writer_task.await;
        Ok::<_, anyhow::Error>(())
    });

    // 30 fps: without pacing this sends a request every 33 ms while replies take
    // 150 ms, so four or five pile up.
    let config = ClientConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        bpp: 16,
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

    let peak = *max_outstanding.lock().unwrap();
    // One for the engine's own initial request (sent inside `try_start`, before
    // this layer can observe it) plus one for the session ticker.
    assert!(
        peak <= 2,
        "up to {peak} requests were unanswered at once ({} sent, {} answered); \
         the ticker is not waiting for the previous update and the picture tears",
        received.load(Ordering::SeqCst),
        replied.load(Ordering::SeqCst),
    );
}
