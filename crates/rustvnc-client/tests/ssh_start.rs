//! Starting a session must not do network work on the caller's thread.
//!
//! The SSH tunnel used to be opened inside `Session::start`: it spawned `ssh`,
//! polled for the forwarded port for up to ten seconds, and checked the host key
//! there too — all while the UI thread waited, so pressing Connect froze the
//! window. These tests pin the new contract: `start` returns immediately and
//! every tunnel problem arrives as an event.

use std::time::{Duration, Instant};

use rustvnc_client::config::{AuthMode, ClientConfig, SecurityMode};
use rustvnc_client::session::{Session, SessionErrorCode, SessionEvent};

#[test]
fn an_untrusted_host_key_is_reported_as_an_event_not_a_hang() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = ClientConfig {
        host: "192.0.2.1".to_string(), // TEST-NET-1: never routable
        port: 5900,
        auth: AuthMode::Auto,
        security: SecurityMode::Ssh,
        ..ClientConfig::default()
    };
    config.ssh.user = "tester".to_string();
    config.ssh.known_hosts = dir.path().join("known_hosts").display().to_string();

    let started = Instant::now();
    let session = Session::start(config, String::new())
        .expect("starting a session must not fail on the tunnel");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "Session::start must not wait for ssh, took {:?}",
        started.elapsed()
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut reported = None;
    while Instant::now() < deadline && reported.is_none() {
        while let Some(event) = session.try_recv() {
            if let SessionEvent::Error {
                code,
                message,
                retryable,
            } = event
            {
                reported = Some((code, message, retryable));
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let (code, message, retryable) = reported.expect("no error event was emitted");
    assert_eq!(code, SessionErrorCode::Network);
    assert!(
        !retryable,
        "a missing host key must not be retried behind the user's back"
    );
    assert!(
        message.contains("主机密钥"),
        "the message must name the host-key problem: {message}"
    );
}
