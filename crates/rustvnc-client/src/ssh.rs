use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::SshConfig;

/// A host key fetched before the first connection, so the user can compare the
/// fingerprint and then let the client pin it.
#[derive(Debug, Clone)]
pub struct HostKeyProbe {
    /// e.g. `256 SHA256:abcd… host.example (ED25519)`
    pub fingerprint: String,
    /// The `known_hosts` lines that were scanned.
    pub known_hosts: String,
}

/// SSH tunnel that forwards a local TCP port to the VNC server.
///
/// Uses the system `ssh` client so the user's existing agent/keys work, but it
/// never falls back to "trust anything": a host key must already be in the
/// configured `known_hosts`, or the user has to fetch and confirm the
/// fingerprint first (see [`probe_host_key`] / [`trust_host_key`]).
pub struct SshTunnel {
    child: Child,
    local_addr: String,
}

impl SshTunnel {
    pub fn start(ssh: &SshConfig, vnc_host: &str, vnc_port: u16) -> Result<Self> {
        if ssh.user.trim().is_empty() {
            bail!("SSH 用户名不能为空 (SSH user is required)");
        }
        let known_hosts = known_hosts_path(ssh);
        // Fail before spawning anything when the host key is unknown: ssh would
        // exit 255 with a message the UI cannot act on.
        if !host_key_known(&known_hosts, vnc_host, ssh.port) {
            bail!(
                "主机密钥尚未信任：{vnc_host}:{} 不在 {} 中。请先点击「探测主机密钥」核对 SHA256 指纹，确认无误后写入 known_hosts。(host key not pinned yet)",
                ssh.port,
                known_hosts.display()
            );
        }

        let local_port = free_local_port()?;
        let local_addr = format!("127.0.0.1:{local_port}");
        let mut command = build_command(ssh, vnc_host, vnc_port, &known_hosts, &local_addr)?;

        let mut child = command
            .spawn()
            .with_context(|| format!("启动 {} 失败（是否安装了 OpenSSH？）", ssh_binary()))?;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect(&local_addr).is_ok() {
                return Ok(Self { child, local_addr });
            }
            if let Some(status) = child.try_wait().context("check ssh")? {
                let stderr = collect_stderr(&mut child);
                return Err(ssh_exit_error(status.to_string(), &stderr));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let stderr = collect_stderr(&mut child);
                let _ = child.wait();
                return Err(ssh_exit_error(
                    format!("转发端口 {local_addr} 在 10 秒内未就绪"),
                    &stderr,
                ));
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

fn ssh_exit_error(reason: String, stderr: &str) -> anyhow::Error {
    let stderr = stderr.trim();
    let hint = if stderr.contains("Permission denied") {
        "（认证失败：确认用户名/密钥；密码认证请改用密钥，见「生成密钥」）"
    } else if stderr.contains("Host key verification failed") {
        "（主机密钥校验失败：请重新探测并核对指纹）"
    } else {
        ""
    };
    if stderr.is_empty() {
        anyhow!("ssh 隧道启动失败：{reason}{hint}")
    } else {
        anyhow!("ssh 隧道启动失败：{reason}{hint}\nssh: {stderr}")
    }
}

fn build_command(
    ssh: &SshConfig,
    vnc_host: &str,
    vnc_port: u16,
    known_hosts: &Path,
    local_addr: &str,
) -> Result<Command> {
    let mut command = Command::new(ssh_binary());
    command
        .arg("-N")
        .arg("-o")
        .arg("StrictHostKeyChecking=yes")
        .arg("-o")
        .arg(format!("UserKnownHostsFile={}", known_hosts.display()))
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("ServerAliveInterval=15")
        .arg("-o")
        .arg("ServerAliveCountMax=3")
        .arg("-p")
        .arg(ssh.port.to_string())
        .arg("-L")
        .arg(format!("{local_addr}:{vnc_host}:{vnc_port}"))
        .arg(format!("{}@{vnc_host}", ssh.user));

    if !ssh.key_path.trim().is_empty() {
        command.arg("-i").arg(&ssh.key_path);
        // Never prompt: an unanswered prompt looks like a hang.
        command.arg("-o").arg("BatchMode=yes");
    } else if !ssh.password.is_empty() {
        // ssh cannot be fed a password on stdin. On Unix it can ask an askpass
        // helper; the Windows OpenSSH build has no askpass support at all, so
        // say that plainly instead of hanging on a prompt nobody can see.
        #[cfg(windows)]
        bail!(
            "Windows 自带 OpenSSH 不支持非交互密码认证：请改用密钥（点「生成密钥」），或先在终端手动执行一次 ssh 登录。\n\
             (Windows OpenSSH cannot take a password non-interactively; use a key instead.)"
        );
        #[cfg(not(windows))]
        {
            command.arg("-o").arg("BatchMode=no");
            command
                .arg("-o")
                .arg("PreferredAuthentications=password,keyboard-interactive");
            let helper = write_askpass_helper()?;
            command.env("SSH_ASKPASS", &helper);
            command.env("SSH_ASKPASS_REQUIRE", "force");
            command.env("DISPLAY", ":0");
            // The helper reads the secret from its own environment so it never
            // touches the filesystem.
            command.env("RUSTVNC_SSH_PASSWORD", &ssh.password);
        }
    } else {
        command.arg("-o").arg("BatchMode=yes");
    }

    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    Ok(command)
}

fn collect_stderr(child: &mut Child) -> String {
    let Some(mut stderr) = child.stderr.take() else {
        return String::new();
    };
    let mut buffer = String::new();
    let _ = stderr.read_to_string(&mut buffer);
    buffer
}

fn ssh_binary() -> String {
    // Tests point this at a stub so the failure paths can be exercised without
    // a real SSH server.
    std::env::var("RUSTVNC_SSH_BIN").unwrap_or_else(|_| "ssh".to_string())
}

fn keyscan_binary() -> String {
    std::env::var("RUSTVNC_SSH_KEYSCAN_BIN").unwrap_or_else(|_| "ssh-keyscan".to_string())
}

fn keygen_binary() -> String {
    std::env::var("RUSTVNC_SSH_KEYGEN_BIN").unwrap_or_else(|_| "ssh-keygen".to_string())
}

#[cfg(not(windows))]
fn write_askpass_helper() -> Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::temp_dir().join("rustvnc-askpass.sh");
    std::fs::write(
        &path,
        "#!/bin/sh\nprintf '%s\\n' \"$RUSTVNC_SSH_PASSWORD\"\n",
    )
    .context("write askpass helper")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
        .context("chmod askpass helper")?;
    Ok(path)
}

pub fn known_hosts_path(ssh: &SshConfig) -> PathBuf {
    if ssh.known_hosts.trim().is_empty() {
        default_known_hosts()
    } else {
        PathBuf::from(&ssh.known_hosts)
    }
}

fn default_known_hosts() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".ssh")
        .join("known_hosts")
}

/// `known_hosts` spells a non-default port as `[host]:port`.
fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 {
        host.to_string()
    } else {
        format!("[{host}]:{port}")
    }
}

/// True when `known_hosts` already has an entry for this host (and, when the
/// file uses `@revoked`, not a revoked one).
pub fn host_key_known(path: &Path, host: &str, port: u16) -> bool {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return false;
    };
    let pattern = host_pattern(host, port);
    contents.lines().any(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        let (marker, rest) = match line.strip_prefix('@') {
            Some(rest) => ("@", rest),
            None => ("", line),
        };
        let mut fields = rest.split_whitespace();
        let hosts = fields.next().unwrap_or_default();
        if marker == "@revoked" {
            return false;
        }
        hosts
            .split(',')
            .any(|candidate| candidate == pattern || (port == 22 && candidate == host))
    })
}

/// Fetch the host key over the network and render its fingerprint.
///
/// Nothing is trusted at this point: the caller shows the fingerprint to the
/// user, and only [`trust_host_key`] writes it into `known_hosts`.
pub fn probe_host_key(host: &str, port: u16) -> Result<HostKeyProbe> {
    if host.trim().is_empty() {
        bail!("主机地址为空 (host is empty)");
    }
    let output = Command::new(keyscan_binary())
        .arg("-p")
        .arg(port.to_string())
        .arg("-T")
        .arg("5")
        .arg(host)
        .output()
        .with_context(|| format!("运行 {} 失败（OpenSSH 未安装？）", keyscan_binary()))?;
    let known_hosts = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if known_hosts.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("无法从 {host}:{port} 获取主机密钥：{}", stderr.trim());
    }

    let mut child = Command::new(keygen_binary())
        .arg("-lf")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("运行 {} 失败（OpenSSH 未安装？）", keygen_binary()))?;
    if let Some(mut stdin) = child.stdin.take() {
        use std::io::Write;
        stdin
            .write_all(known_hosts.as_bytes())
            .context("feed ssh-keygen")?;
    }
    let output = child.wait_with_output().context("read ssh-keygen")?;
    if !output.status.success() {
        bail!(
            "ssh-keygen 无法解析主机密钥：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let fingerprint = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(HostKeyProbe {
        fingerprint,
        known_hosts,
    })
}

/// Append scanned host keys to `known_hosts`, skipping ones already present.
pub fn trust_host_key(path: &Path, entries: &str) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).context("create .ssh directory")?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut appended = String::new();
    for line in entries.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split_whitespace();
        let hosts = fields.next().unwrap_or_default();
        let key_type = fields.next().unwrap_or_default();
        let already = existing.lines().any(|candidate| {
            let mut f = candidate.split_whitespace();
            f.next() == Some(hosts) && f.next() == Some(key_type)
        }) || appended.lines().any(|candidate: &str| {
            let mut f = candidate.split_whitespace();
            f.next() == Some(hosts) && f.next() == Some(key_type)
        });
        if !already {
            appended.push_str(line);
            appended.push('\n');
        }
    }
    if appended.is_empty() {
        return Ok(());
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .context("open known_hosts")?;
    use std::io::Write;
    file.write_all(appended.as_bytes())
        .context("append known_hosts")?;
    Ok(())
}

/// Generate an ed25519 key pair and return the public key line, which the user
/// pastes into the Mac's `~/.ssh/authorized_keys`.
pub fn generate_key(path: &Path) -> Result<String> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).context("create key directory")?;
    }
    let output = Command::new(keygen_binary())
        .arg("-t")
        .arg("ed25519")
        .arg("-f")
        .arg(path)
        .arg("-N")
        .arg("")
        .arg("-C")
        .arg("rustvnc-client")
        .output()
        .with_context(|| format!("运行 {} 失败（OpenSSH 未安装？）", keygen_binary()))?;
    if !output.status.success() {
        bail!(
            "生成密钥失败：{}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let public = PathBuf::from(format!("{}.pub", path.display()));
    std::fs::read_to_string(&public).context("read generated public key")
}

fn free_local_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).context("find free local port")?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|err| anyhow!(err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_pattern_uses_brackets_for_non_default_port() {
        assert_eq!(host_pattern("mac.local", 22), "mac.local");
        assert_eq!(host_pattern("mac.local", 2222), "[mac.local]:2222");
    }

    #[test]
    fn known_hosts_matching_handles_ports_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(
            &path,
            "# comment\n[mac.local]:2222 ssh-ed25519 AAAAC3Nz\nother.local ssh-ed25519 AAAAC3Nz\n",
        )
        .unwrap();
        assert!(host_key_known(&path, "mac.local", 2222));
        assert!(!host_key_known(&path, "mac.local", 22));
        assert!(host_key_known(&path, "other.local", 22));
        assert!(!host_key_known(
            &dir.path().join("missing"),
            "mac.local",
            22
        ));
    }

    #[test]
    fn revoked_entry_is_not_treated_as_known() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("known_hosts");
        std::fs::write(&path, "@revoked * ssh-ed25519 AAAAC3Nz\n").unwrap();
        assert!(!host_key_known(&path, "mac.local", 22));
    }

    #[test]
    fn trust_host_key_appends_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("known_hosts");
        let entries = "[mac.local]:2222 ssh-ed25519 AAAAC3Nz\n";
        trust_host_key(&path, entries).unwrap();
        trust_host_key(&path, entries).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.matches("AAAAC3Nz").count(), 1, "{contents}");
        assert!(host_key_known(&path, "mac.local", 2222));
    }

    /// A missing host key must fail fast with an actionable message instead of
    /// spawning ssh and surfacing "exit status: 255" with no context.
    #[test]
    fn unknown_host_key_fails_before_spawning_ssh() {
        let dir = tempfile::tempdir().unwrap();
        let ssh = SshConfig {
            user: "me".to_string(),
            port: 2222,
            key_path: String::new(),
            known_hosts: dir.path().join("empty_known_hosts").display().to_string(),
            password: String::new(),
        };
        let error = match SshTunnel::start(&ssh, "127.0.0.1", 5900) {
            Ok(_) => panic!("must refuse an unpinned host key"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(message.contains("主机密钥"), "{message}");
        assert!(message.contains("known_hosts"), "{message}");
    }

    /// The tunnel must never silently accept an unfamiliar host key.
    #[test]
    fn ssh_command_pins_host_keys_and_never_prompts() {
        let ssh = SshConfig {
            user: "me".to_string(),
            port: 2222,
            key_path: "/home/me/.ssh/id_ed25519".to_string(),
            known_hosts: "/home/me/.ssh/known_hosts".to_string(),
            password: String::new(),
        };
        let command = build_command(
            &ssh,
            "mac.local",
            5900,
            Path::new("/home/me/.ssh/known_hosts"),
            "127.0.0.1:5000",
        )
        .unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(
            args.contains(&"StrictHostKeyChecking=yes".to_string()),
            "{args:?}"
        );
        assert!(
            !args.contains(&"StrictHostKeyChecking=accept-new".to_string()),
            "must not accept a new host key implicitly: {args:?}"
        );
        assert!(args.contains(&"BatchMode=yes".to_string()), "{args:?}");
        assert!(
            args.contains(&"ExitOnForwardFailure=yes".to_string()),
            "{args:?}"
        );
        assert!(
            args.iter()
                .any(|arg| arg == "127.0.0.1:5000:mac.local:5900"),
            "{args:?}"
        );
        assert!(args.contains(&"me@mac.local".to_string()), "{args:?}");
    }

    #[test]
    fn ssh_stderr_is_surfaced_with_a_hint() {
        let error = ssh_exit_error(
            "exit status: 255".to_string(),
            "me@mac.local: Permission denied (publickey).\n",
        );
        let message = format!("{error:#}");
        assert!(message.contains("Permission denied"), "{message}");
        assert!(message.contains("认证失败"), "{message}");
        assert!(
            !message.contains("\n\n"),
            "stderr should be trimmed: {message:?}"
        );
    }
}
