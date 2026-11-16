use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use rustvnc_agent::client::{self, PairOptions, PingOptions};
use rustvnc_agent::server::{self, ServeInfo, ServeOptions};
use tempfile::TempDir;
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;

struct TestServer {
    info: ServeInfo,
    shutdown: watch::Sender<bool>,
    handle: tokio::task::JoinHandle<Result<()>>,
}

impl TestServer {
    async fn start(state_dir: PathBuf, code: &str) -> Self {
        let (info_tx, info_rx) = oneshot::channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let options = ServeOptions {
            listen: "127.0.0.1:0".parse().expect("valid listen address"),
            state_dir,
            pair_code: Some(code.to_string()),
            sans: vec![],
            keychain: false,
        };
        let handle = tokio::spawn(server::run_with_shutdown(
            options,
            Some(info_tx),
            shutdown_rx,
        ));
        let info = timeout(Duration::from_secs(5), info_rx)
            .await
            .expect("server startup timed out")
            .expect("server startup failed");
        Self {
            info,
            shutdown: shutdown_tx,
            handle,
        }
    }

    fn addr(&self) -> String {
        self.info.local_addr.to_string()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(true);
        let _ = timeout(Duration::from_secs(5), self.handle).await;
    }
}

fn pair_options(addr: &str, code: &str, state_dir: &Path) -> PairOptions {
    PairOptions {
        addr: addr.to_string(),
        code: code.to_string(),
        name: "test-client".to_string(),
        state_dir: state_dir.to_path_buf(),
        expect_fingerprint: None,
        server_name: None,
        keychain: false,
    }
}

fn ping_options(addr: &str, state_dir: &Path) -> PingOptions {
    PingOptions {
        addr: addr.to_string(),
        count: 2,
        state_dir: state_dir.to_path_buf(),
        server_name: None,
        keychain: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pair_ping_and_code_reuse() -> Result<()> {
    let agent_dir = TempDir::new()?;
    let client_dir = TempDir::new()?;
    let other_client_dir = TempDir::new()?;

    let server = TestServer::start(agent_dir.path().to_path_buf(), "12345678").await;
    let addr = server.addr();

    client::pair(pair_options(&addr, "12345678", client_dir.path())).await?;
    client::ping(ping_options(&addr, client_dir.path())).await?;

    // Wrong code.
    let wrong = client::pair(pair_options(&addr, "00000000", other_client_dir.path())).await;
    assert!(wrong.is_err(), "wrong pairing code must be rejected");

    // The code is one-time.
    let reuse = client::pair(pair_options(&addr, "12345678", other_client_dir.path())).await;
    assert!(reuse.is_err(), "pairing code must not be reusable");

    server.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unpaired_client_is_rejected_after_restart() -> Result<()> {
    let agent_dir = TempDir::new()?;
    let client_dir = TempDir::new()?;

    let server = TestServer::start(agent_dir.path().to_path_buf(), "11111111").await;
    let addr = server.addr();
    client::pair(pair_options(&addr, "11111111", client_dir.path())).await?;
    client::ping(ping_options(&addr, client_dir.path())).await?;
    server.stop().await;

    // Remove the client from the allowlist, then restart on the same CA.
    std::fs::write(agent_dir.path().join("clients.txt"), b"")?;

    let server = TestServer::start(agent_dir.path().to_path_buf(), "22222222").await;
    let addr = server.addr();
    let rejected = client::ping(ping_options(&addr, client_dir.path())).await;
    assert!(rejected.is_err(), "unpaired client must be rejected");

    // Re-pairing restores access.
    client::pair(pair_options(&addr, "22222222", client_dir.path())).await?;
    client::ping(ping_options(&addr, client_dir.path())).await?;
    server.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fingerprint_mismatch_is_rejected() -> Result<()> {
    let agent_dir = TempDir::new()?;
    let client_dir = TempDir::new()?;

    let server = TestServer::start(agent_dir.path().to_path_buf(), "33333333").await;
    let addr = server.addr();

    let mut options = pair_options(&addr, "33333333", client_dir.path());
    options.expect_fingerprint = Some("deadbeef".to_string());
    let result = client::pair(options).await;
    assert!(result.is_err(), "fingerprint mismatch must be rejected");

    server.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cross_ca_client_is_rejected() -> Result<()> {
    let agent_a = TempDir::new()?;
    let agent_b = TempDir::new()?;
    let client_a = TempDir::new()?;
    let client_b = TempDir::new()?;

    let server_a = TestServer::start(agent_a.path().to_path_buf(), "44444444").await;
    let server_b = TestServer::start(agent_b.path().to_path_buf(), "55555555").await;

    client::pair(pair_options(&server_a.addr(), "44444444", client_a.path())).await?;
    client::pair(pair_options(&server_b.addr(), "55555555", client_b.path())).await?;

    // Client B trusts CA B, so server A's certificate must be rejected by TLS.
    let cross = client::ping(ping_options(&server_a.addr(), client_b.path())).await;
    assert!(cross.is_err(), "client from another CA must be rejected");

    server_a.stop().await;
    server_b.stop().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_is_rate_limited_per_ip() -> Result<()> {
    let agent_dir = TempDir::new()?;
    let client_dir = TempDir::new()?;

    let server = TestServer::start(agent_dir.path().to_path_buf(), "66666666").await;
    let addr = server.addr();

    for _ in 0..5 {
        let _ = client::pair(pair_options(&addr, "00000000", client_dir.path())).await;
    }
    let sixth = client::pair(pair_options(&addr, "00000000", client_dir.path())).await;
    let message = format!(
        "{:#}",
        sixth.expect_err("sixth attempt must be rate limited")
    );
    assert!(
        message.contains("from this address"),
        "expected per-IP rate limit, got: {message}"
    );

    server.stop().await;
    Ok(())
}
