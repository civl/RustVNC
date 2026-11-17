use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustvnc_client::config::{ClientConfig, QualityPreset};
use rustvnc_client::session::{Session, SessionEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use vnc::{ClientKeyEvent, ClientMouseEvent, X11Event};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_receives_frame_and_sends_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let inputs = Arc::new(Mutex::new(Vec::<String>::new()));
    let server_inputs = Arc::clone(&inputs);

    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        mock_server(stream, server_inputs).await.expect("server");
    });

    let mut config = ClientConfig::default();
    config.host = "127.0.0.1".to_string();
    config.port = addr.port();
    config.quality = QualityPreset::High;
    config.bpp = 32;
    config.auth = rustvnc_client::config::AuthMode::Auto;

    let mut session = Session::start(config, String::new()).expect("start session");
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut got_connected = false;
    let mut got_raw = false;
    while Instant::now() < deadline && !(got_connected && got_raw) {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::Connected { .. } => got_connected = true,
                SessionEvent::Resized { width, height } => {
                    assert_eq!((width, height), (32, 16));
                }
                SessionEvent::RawImage { rect, data } => {
                    assert_eq!((rect.width, rect.height), (32, 16));
                    assert_eq!(data.len(), 32 * 16 * 4);
                    got_raw = true;
                }
                SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(got_connected, "no connected event");
    assert!(got_raw, "no raw image event");

    session.send_input(X11Event::PointerEvent(ClientMouseEvent {
        position_x: 10,
        position_y: 10,
        bottons: 0,
    }));
    session.send_input(X11Event::KeyEvent(ClientKeyEvent {
        keycode: 0x41,
        down: true,
    }));
    session.send_input(X11Event::KeyEvent(ClientKeyEvent {
        keycode: 0x41,
        down: false,
    }));
    tokio::time::sleep(Duration::from_millis(200)).await;
    session.close();
    server.await.expect("server task");

    let inputs = inputs.lock().expect("inputs");
    assert!(inputs.iter().any(|input| input == "pointer"), "{inputs:?}");
    assert!(inputs.iter().any(|input| input == "key"), "{inputs:?}");
}

async fn mock_server(mut stream: TcpStream, inputs: Arc<Mutex<Vec<String>>>) -> anyhow::Result<()> {
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;

    stream.write_all(&[1, 1]).await?; // one security type: None
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen).await?;
    anyhow::ensure!(chosen[0] == 1);
    stream.write_all(&0u32.to_be_bytes()).await?; // SecurityResult OK

    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared).await?;

    stream.write_all(&32u16.to_be_bytes()).await?;
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;

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
                send_frame(&mut stream).await?;
            }
            4 => {
                let mut rest = [0u8; 7];
                stream.read_exact(&mut rest).await?;
                inputs.lock().expect("inputs").push("key".to_string());
            }
            5 => {
                let mut rest = [0u8; 5];
                stream.read_exact(&mut rest).await?;
                inputs.lock().expect("inputs").push("pointer".to_string());
            }
            6 => {
                let mut header = [0u8; 3];
                stream.read_exact(&mut header).await?;
                let mut length = [0u8; 4];
                stream.read_exact(&mut length).await?;
                let mut text = vec![0u8; u32::from_be_bytes(length) as usize];
                stream.read_exact(&mut text).await?;
            }
            other => anyhow::bail!("unexpected message {other}"),
        }
    }
    Ok(())
}

async fn send_frame(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream.write_all(&[0, 0]).await?;
    stream.write_all(&1u16.to_be_bytes()).await?;
    stream.write_all(&0u16.to_be_bytes()).await?;
    stream.write_all(&0u16.to_be_bytes()).await?;
    stream.write_all(&32u16.to_be_bytes()).await?;
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&0i32.to_be_bytes()).await?;
    let mut pixels = Vec::with_capacity(32 * 16 * 4);
    for y in 0..16u8 {
        for x in 0..32u8 {
            pixels.extend_from_slice(&[x.wrapping_mul(4), y.wrapping_mul(8), 128, 255]);
        }
    }
    stream.write_all(&pixels).await?;
    Ok(())
}

fn pixel_format() -> [u8; 16] {
    [32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 16, 8, 0, 0, 0, 0]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_reconnects_after_remote_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.expect("first accept");
        handshake(&mut first).await.expect("first handshake");
        // Force a protocol error so the client definitely reports the drop;
        // a plain FIN is not always surfaced by the upstream decoder.
        let _ = first.write_all(&[99]).await;
        let _ = first.flush().await;
        tokio::time::sleep(Duration::from_millis(150)).await;
        drop(first);
        let (mut second, _) = listener.accept().await.expect("second accept");
        handshake(&mut second).await.expect("second handshake");
        serve_one_frame(&mut second).await.expect("second frame");
    });

    let config = ClientConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        bpp: 32,
        auto_reconnect: true,
        ..ClientConfig::default()
    };

    let mut session = Session::start(config, String::new()).expect("start");
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut connected = 0;
    let mut reconnecting = false;
    let mut raw = false;
    let mut seen = Vec::new();
    while Instant::now() < deadline && !raw {
        while let Some(event) = session.try_recv() {
            seen.push(format!("{event:?}"));
            match event {
                SessionEvent::Connected { .. } => connected += 1,
                SessionEvent::Reconnecting { .. } => reconnecting = true,
                SessionEvent::RawImage { .. } => raw = true,
                SessionEvent::Error { .. } => {}
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(connected >= 1, "no connected event: {seen:?}");
    assert!(reconnecting, "no reconnecting event: {seen:?}");
    assert!(raw, "no frame after reconnect: {seen:?}");
    session.close();
    server.await.expect("server");
}

async fn handshake(stream: &mut TcpStream) -> anyhow::Result<()> {
    stream.write_all(b"RFB 003.008\n").await?;
    let mut version = [0u8; 12];
    stream.read_exact(&mut version).await?;
    stream.write_all(&[1, 1]).await?;
    let mut chosen = [0u8; 1];
    stream.read_exact(&mut chosen).await?;
    stream.write_all(&0u32.to_be_bytes()).await?;
    let mut shared = [0u8; 1];
    stream.read_exact(&mut shared).await?;
    stream.write_all(&32u16.to_be_bytes()).await?;
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

async fn serve_one_frame(stream: &mut TcpStream) -> anyhow::Result<()> {
    let mut sent = false;
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
                if !sent {
                    send_frame(stream).await?;
                    sent = true;
                } else {
                    stream.write_all(&[0, 0, 0, 0]).await?;
                }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_session_stops_frames_and_resumes_with_a_refresh() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let inputs = Arc::new(Mutex::new(Vec::<String>::new()));
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept");
        mock_server(stream, inputs).await.expect("server");
    });

    let config = ClientConfig {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        bpp: 32,
        auth: rustvnc_client::config::AuthMode::Auto,
        ..ClientConfig::default()
    };
    let mut session = Session::start(config, String::new()).expect("start");
    let wait_raw = |session: &Session| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            while let Some(event) = session.try_recv() {
                if matches!(event, SessionEvent::RawImage { .. }) {
                    return true;
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    };
    assert!(wait_raw(&session), "no initial frame");

    session.set_paused(true);
    tokio::time::sleep(Duration::from_millis(300)).await;
    while session.try_recv().is_some() {}
    let paused_deadline = Instant::now() + Duration::from_millis(400);
    let mut frames_while_paused = 0;
    while Instant::now() < paused_deadline {
        while let Some(event) = session.try_recv() {
            if matches!(event, SessionEvent::RawImage { .. }) {
                frames_while_paused += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(frames_while_paused, 0, "paused tab must not receive frames");

    session.set_paused(false);
    assert!(wait_raw(&session), "resume did not request a fresh frame");
    session.close();
    server.await.expect("server");
}
