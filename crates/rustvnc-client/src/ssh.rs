use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::config::SshConfig;

/// SSH tunnel that forwards a local TCP port to the VNC server.
///
/// Uses the system `ssh` client (OpenSSH on Windows 10+, macOS, Linux) so the
/// user's existing agent/keys work. BatchMode + StrictHostKeyChecking are
/// mandatory: key-based auth only, and host key changes are rejected via the
/// configured known_hosts file.
pub struct SshTunnel {
    child: Child,
    local_addr: String,
}

impl SshTunnel {
    pub fn start(ssh: &SshConfig, vnc_host: &str, vnc_port: u16) -> Result<Self> {
        if ssh.user.trim().is_empty() {
            bail!("SSH user is empty");
        }
        let local_port = free_local_port()?;
        let local_addr = format!("127.0.0.1:{local_port}");
        let known_hosts = if ssh.known_hosts.trim().is_empty() {
            default_known_hosts()
        } else {
            PathBuf::from(&ssh.known_hosts)
        };

        let mut command = Command::new("ssh");
        command
            .arg("-N")
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=yes")
            .arg("-o")
            .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
            .arg("-o")
            .arg("ExitOnForwardFailure=yes")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-p")
            .arg(ssh.port.to_string())
            .arg("-L")
            .arg(format!("{local_addr}:{vnc_host}:{vnc_port}"))
            .arg(format!("{}@{vnc_host}", ssh.user));
        if !ssh.key_path.trim().is_empty() {
            command.arg("-i").arg(&ssh.key_path);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let mut child = command
            .spawn()
            .context("spawn ssh (is OpenSSH installed?)")?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect(&local_addr).is_ok() {
                return Ok(Self { child, local_addr });
            }
            if let Some(status) = child.try_wait().context("check ssh")? {
                bail!("ssh exited before the tunnel was ready: {status}");
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                bail!("SSH tunnel did not open {local_addr} within 10s");
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn local_addr(&self) -> &str {
        &self.local_addr
    }
}

impl Drop for SshTunnel {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_local_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("find free local port")?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|err| anyhow::anyhow!(err))
}

fn default_known_hosts() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("known_hosts")
}
