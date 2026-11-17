//! Regression guard for the idle-session CPU cost.
//!
//! The session loop used to call `poll_event()` — a non-blocking `try_recv` —
//! inside `tokio::select!`. Because that future was always ready, the loop
//! spun at full speed whenever the framebuffer was idle and kept a whole core
//! busy. `recv_event()` parks the task instead.
//!
//! The test measures the process CPU time consumed while the session sits
//! connected and idle, so any regression back to polling fails the build.

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
    stream.write_all(&32u16.to_be_bytes()).await?;
    stream.write_all(&16u16.to_be_bytes()).await?;
    stream.write_all(&pixel_format()).await?;
    stream.write_all(&(10u32).to_be_bytes()).await?;
    stream.write_all(b"mock-vnc!!").await?;
    Ok(())
}

/// Answers every FramebufferUpdateRequest with an empty update: the session
/// stays connected but nothing ever changes on screen.
async fn serve_idle(stream: &mut TcpStream) -> anyhow::Result<()> {
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
                stream.write_all(&[0, 0, 0, 0]).await?;
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

#[cfg(windows)]
fn process_cpu_seconds() -> f64 {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

    fn ticks(filetime: &FILETIME) -> u64 {
        ((filetime.dwHighDateTime as u64) << 32) | filetime.dwLowDateTime as u64
    }

    let mut creation = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut exit = creation;
    let mut kernel = creation;
    let mut user = creation;
    // SAFETY: the handle is the current pseudo-handle and every out-parameter
    // points at a live FILETIME.
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    assert_ne!(ok, 0, "GetProcessTimes failed");
    // FILETIME counts 100 ns intervals.
    (ticks(&kernel) + ticks(&user)) as f64 / 10_000_000.0
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_session_does_not_burn_cpu() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept");
        handshake(&mut stream).await.expect("handshake");
        serve_idle(&mut stream).await.expect("serve");
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
    let mut connected = false;
    while Instant::now() < deadline && !connected {
        while let Some(event) = session.try_recv() {
            match event {
                SessionEvent::Connected { .. } => connected = true,
                SessionEvent::Error { message, .. } => panic!("session error: {message}"),
                _ => {}
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(connected, "session never connected");

    // Let the first refresh round settle, then measure a quiet window.
    tokio::time::sleep(Duration::from_millis(250)).await;

    #[cfg(windows)]
    {
        const WINDOW: Duration = Duration::from_secs(3);
        let before = process_cpu_seconds();
        tokio::time::sleep(WINDOW).await;
        let used = process_cpu_seconds() - before;
        let budget = WINDOW.as_secs_f64() * 0.4;
        println!(
            "idle cpu: {used:.3}s over {:.1}s (budget {budget:.1}s)",
            WINDOW.as_secs_f64()
        );
        assert!(
            used < budget,
            "idle session burned {used:.2}s CPU in {:.1}s — the event loop is spinning again",
            WINDOW.as_secs_f64()
        );
    }

    session.close();
    let _ = server.await;
}
