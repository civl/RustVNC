#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::time::{Duration, Instant};

use eframe::egui;
use rustvnc_agent_client::{
    AgentEvent, AgentSession, PairConfig, SessionConfig as AgentSessionConfig, pair,
};
use rustvnc_client::config::{
    AuthMode, ClientConfig, EncodingPreference, QualityPreset, SecurityMode,
};
use rustvnc_client::credentials::CredentialStore;
use rustvnc_client::input::{key_to_keysym, text_to_keysyms};
use rustvnc_client::profiles::{Profile, ProfileStore};
use rustvnc_client::session::{Session, SessionEvent};
use rustvnc_client::ssh::{self, HostKeyProbe};
use vnc::{ClientKeyEvent, ClientMouseEvent, Rect, X11Event};

fn main() -> eframe::Result {
    let _log_guard = rustvnc_client::diag::init_tracing();

    // wgpu (D3D12/Vulkan) is the default backend: the OpenGL path does not
    // present on headless or indirectly attached displays — with the desktop
    // viewed through RustDesk on a machine without a monitor, every glow window
    // rendered correctly into its framebuffer and still appeared blank white.
    // `RUSTVNC_RENDERER=glow` forces the old backend for troubleshooting.
    let preferred = match std::env::var("RUSTVNC_RENDERER").as_deref() {
        Ok("glow") => eframe::Renderer::Glow,
        _ => eframe::Renderer::Wgpu,
    };
    match run_native(preferred) {
        Err(err) if preferred == eframe::Renderer::Wgpu => {
            tracing::warn!("wgpu renderer unavailable ({err}); falling back to glow/OpenGL");
            run_native(eframe::Renderer::Glow)
        }
        other => other,
    }
}

fn run_native(renderer: eframe::Renderer) -> eframe::Result {
    tracing::info!("starting UI with the {renderer:?} renderer");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1120.0, 720.0]),
        renderer,
        ..Default::default()
    };
    eframe::run_native(
        "RustVNC",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lang {
    Zh,
    En,
}

impl Lang {
    fn t(self, zh: &'static str, en: &'static str) -> &'static str {
        match self {
            Self::Zh => zh,
            Self::En => en,
        }
    }
}

struct App {
    lang: Lang,
    profiles: ProfileStore,
    selected_id: Option<String>,
    draft: Profile,
    password: String,
    remember: bool,
    credentials: CredentialStore,
    session: Option<Session>,
    frame: Frame,
    texture: Option<egui::TextureHandle>,
    frame_dirty: bool,
    pending_rects: Vec<Rect>,
    image_rect: Option<egui::Rect>,
    status: String,
    stats: String,
    error: Option<String>,
    search: String,
    last_pointer: Option<(u16, u16, u8)>,
    last_texture_size: (usize, usize),
    clipboard_sync: bool,
    last_local_clipboard: Option<String>,
    last_remote_clipboard: Option<String>,
    last_clipboard_poll: Instant,
    /// Which modifiers the remote currently has held down. Tracked so the
    /// client can send real transitions instead of re-deriving them per key,
    /// which is what makes Ctrl+click and Shift+drag work.
    sent_modifiers: ModifierState,
    /// Host key fetched for the current form, waiting for the user to compare
    /// the fingerprint and pin it.
    host_key_probe: Option<HostKeyProbe>,
    /// Multi-line, user-facing note that is not an error (e.g. the public key
    /// to install on the Mac).
    info: Option<String>,
    /// Enhanced channel to the macOS agent: clipboard + audio. Optional, and a
    /// failure here never tears down the RFB session.
    agent: Option<AgentSession>,
    agent_status: String,
    agent_pair_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
}

/// The four modifiers RFB can express, as remote state rather than per-event
/// flags.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ModifierState {
    shift: bool,
    ctrl: bool,
    alt: bool,
    /// The Windows key, which macOS receives as Command. Deliberately not
    /// egui's `Modifiers::command`: that field aliases Ctrl on Windows, so
    /// using it would emit Ctrl+Super for every Ctrl shortcut.
    command: bool,
}

impl ModifierState {
    fn from_egui(modifiers: egui::Modifiers) -> Self {
        Self {
            shift: modifiers.shift,
            ctrl: modifiers.ctrl,
            alt: modifiers.alt,
            command: modifiers.mac_cmd,
        }
    }

    /// (keysym, down) transitions that turn `from` into `to`, in send order.
    fn transitions(from: Self, to: Self) -> Vec<(u32, bool)> {
        let mut out = Vec::new();
        for key in Self::PRESS_ORDER {
            if (key.held)(&to) && !(key.held)(&from) {
                out.push((key.keysym, true));
            }
        }
        for key in Self::PRESS_ORDER.iter().rev() {
            if (key.held)(&from) && !(key.held)(&to) {
                out.push((key.keysym, false));
            }
        }
        out
    }

    /// Press order; releases run backwards.
    const PRESS_ORDER: [ModifierKey; 4] = [
        ModifierKey {
            keysym: 0xffe1,
            held: |m| m.shift,
        }, // Shift_L
        ModifierKey {
            keysym: 0xffe3,
            held: |m| m.ctrl,
        }, // Control_L
        ModifierKey {
            keysym: 0xffe9,
            held: |m| m.alt,
        }, // Alt_L
        ModifierKey {
            keysym: 0xffeb,
            held: |m| m.command,
        }, // Super_L
    ];
}

/// One RFB keysym plus a predicate reading that modifier out of a snapshot.
#[derive(Clone, Copy)]
struct ModifierKey {
    keysym: u32,
    held: fn(&ModifierState) -> bool,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        rustvnc_client::fonts::install_cjk_font(&cc.egui_ctx);
        let path = ProfileStore::default_path();
        let (profiles, error) = match ProfileStore::load(&path) {
            Ok(profiles) => (profiles, None),
            Err(err) => (
                ProfileStore::empty(path),
                Some(format!("load profiles: {err:#}")),
            ),
        };
        let draft = profiles
            .profiles()
            .first()
            .cloned()
            .unwrap_or_else(|| Profile::new("New Mac", ClientConfig::default()));
        let selected_id = profiles
            .profiles()
            .first()
            .map(|profile| profile.id.clone());

        Self {
            lang: Lang::Zh,
            profiles,
            selected_id,
            draft,
            password: String::new(),
            remember: true,
            credentials: CredentialStore::system(),
            session: None,
            frame: Frame::default(),
            texture: None,
            frame_dirty: false,
            pending_rects: Vec::new(),
            image_rect: None,
            status: "Ready".to_string(),
            stats: String::new(),
            error,
            search: String::new(),
            last_pointer: None,
            last_texture_size: (0, 0),
            clipboard_sync: true,
            last_local_clipboard: None,
            last_remote_clipboard: None,
            last_clipboard_poll: Instant::now(),
            sent_modifiers: ModifierState::default(),
            host_key_probe: None,
            info: None,
            agent: None,
            agent_status: String::new(),
            agent_pair_rx: None,
        }
    }

    fn select_profile(&mut self, id: &str) {
        if let Some(profile) = self.profiles.get(id).cloned() {
            self.password = self
                .credentials
                .get(&profile.id)
                .ok()
                .flatten()
                .unwrap_or_default();
            self.selected_id = Some(profile.id.clone());
            self.draft = profile;
            self.draft.config.ssh.password = self
                .credentials
                .get(&ssh_credential_key(&self.draft.id))
                .ok()
                .flatten()
                .unwrap_or_default();
            self.host_key_probe = None;
            self.error = None;
        }
    }

    /// Fetch the host key and show its SHA256 fingerprint. Nothing is trusted
    /// until the user confirms it and [`Self::trust_host_key`] runs.
    fn probe_host_key(&mut self) {
        let host = self.draft.config.host.clone();
        let port = self.draft.config.ssh.port;
        match ssh::probe_host_key(&host, port) {
            Ok(probe) => {
                self.status = format!("已获取 {host}:{port} 的主机密钥，请核对指纹");
                self.host_key_probe = Some(probe);
                self.error = None;
            }
            Err(err) => {
                self.host_key_probe = None;
                self.error = Some(format!("{err:#}"));
            }
        }
    }

    fn trust_host_key(&mut self) {
        let Some(probe) = self.host_key_probe.clone() else {
            return;
        };
        let path = ssh::known_hosts_path(&self.draft.config.ssh);
        match ssh::trust_host_key(&path, &probe.known_hosts) {
            Ok(()) => {
                self.status = format!("已写入 {}", path.display());
                self.host_key_probe = None;
                self.error = None;
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn generate_ssh_key(&mut self) {
        let path = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".ssh")
            .join("rustvnc_ed25519");
        match ssh::generate_key(&path) {
            Ok(public) => {
                self.draft.config.ssh.key_path = path.display().to_string();
                self.status = format!("已生成 {}", path.display());
                self.error = None;
                // The private key never leaves this machine; the user has to
                // install the public half once on the Mac.
                self.info = Some(format!(
                    "把下面这行追加到 Mac 的 ~/.ssh/authorized_keys（一次性）：\n{public}"
                ));
            }
            Err(err) => self.error = Some(format!("{err:#}")),
        }
    }

    fn new_profile(&mut self) {
        self.draft = Profile::new("New Mac", ClientConfig::default());
        self.selected_id = None;
        self.password.clear();
        self.error = None;
    }

    fn save_profile(&mut self) {
        if self.draft.name.trim().is_empty() {
            self.draft.name = if self.draft.config.host.trim().is_empty() {
                "New Mac".to_string()
            } else {
                self.draft.config.host.clone()
            };
        }
        if let Err(err) = self.profiles.update(self.draft.clone()) {
            self.error = Some(format!("save profile: {err:#}"));
        } else {
            self.selected_id = Some(self.draft.id.clone());
        }
    }

    fn delete_profile(&mut self) {
        if let Some(id) = self.selected_id.clone() {
            if let Err(err) = self.profiles.remove(&id) {
                self.error = Some(format!("delete profile: {err:#}"));
                return;
            }
            let _ = self.credentials.delete(&id);
            self.new_profile();
        }
    }

    fn connect(&mut self) {
        self.save_profile();
        if self.draft.config.security == SecurityMode::Ssh {
            if self.draft.config.ssh.user.trim().is_empty() {
                self.error = Some("SSH 用户名不能为空 (SSH user is required)".to_string());
                return;
            }
            // `SshConfig::password` is `#[serde(skip)]`: keep the secret in the
            // credential store, never in profiles.json.
            if self.remember && !self.draft.config.ssh.password.is_empty() {
                let _ = self.credentials.set(
                    &ssh_credential_key(&self.draft.id),
                    &self.draft.config.ssh.password,
                );
            }
        }
        if self.draft.config.host.trim().is_empty() {
            self.error = Some("host is required".to_string());
            return;
        }
        if self.remember && !self.password.is_empty() {
            let _ = self.credentials.set(&self.draft.id, &self.password);
        } else if !self.remember {
            let _ = self.credentials.delete(&self.draft.id);
        }

        match Session::start(self.draft.config.clone(), self.password.clone()) {
            Ok(session) => {
                self.frame = Frame::default();
                self.texture = None;
                self.frame_dirty = false;
                self.pending_rects.clear();
                self.last_pointer = None;
                self.image_rect = None;
                self.clipboard_sync = true;
                self.last_local_clipboard = read_local_clipboard();
                self.last_remote_clipboard = None;
                self.last_clipboard_poll = Instant::now();
                self.sent_modifiers = ModifierState::default();
                self.status = format!("Connecting to {}", session.local_addr());
                self.stats.clear();
                self.error = None;
                self.session = Some(session);
                self.start_agent();
            }
            Err(err) => {
                self.error = Some(format!("connect: {err:#}"));
                self.status = "Disconnected".to_string();
            }
        }
    }

    /// Start the optional agent channel. Any failure is reported in
    /// `agent_status` and leaves the RFB session untouched — the agent only
    /// adds clipboard and audio.
    fn start_agent(&mut self) {
        if !self.draft.config.agent.enabled {
            return;
        }
        let addr = format!(
            "{}:{}",
            self.draft.config.host, self.draft.config.agent.port
        );
        match AgentSession::start(AgentSessionConfig {
            addr,
            client_name: local_client_name(),
            state_dir: rustvnc_agent_client::default_state_dir(),
            audio: self.draft.config.agent.audio,
        }) {
            Ok(session) => {
                self.agent_status = "连接 Agent…".to_string();
                self.agent = Some(session);
            }
            Err(err) => {
                self.agent_status = format!("{err:#}");
                self.agent = None;
            }
        }
    }

    fn poll_agent(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.agent_pair_rx {
            match rx.try_recv() {
                Ok(Ok(fingerprint)) => {
                    self.agent_status = format!("已配对，Agent 指纹 {fingerprint}");
                    self.agent_pair_rx = None;
                }
                Ok(Err(message)) => {
                    self.agent_status = message;
                    self.agent_pair_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.agent_pair_rx = None;
                }
            }
        }

        let mut events = Vec::new();
        if let Some(agent) = &self.agent {
            while let Some(event) = agent.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                AgentEvent::Connected {
                    agent,
                    capabilities,
                } => {
                    let audio = if capabilities & rustvnc_agent_client::CAP_AUDIO != 0 {
                        "音频可用"
                    } else {
                        "音频未启用"
                    };
                    self.agent_status = format!("{agent} · {audio}");
                }
                AgentEvent::Clipboard(text) => {
                    if self.clipboard_sync {
                        self.last_remote_clipboard = Some(text.clone());
                        // Remember it as the local value too, so the poller does
                        // not bounce it straight back to the Mac.
                        self.last_local_clipboard = Some(text.clone());
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            let _ = clipboard.set_text(text.clone());
                        }
                        ctx.copy_text(text);
                    }
                }
                AgentEvent::AudioStarted => {
                    if !self.agent_status.contains("音频播放中") {
                        self.agent_status.push_str(" · 音频播放中");
                    }
                }
                AgentEvent::AudioStopped => {}
                AgentEvent::Error(message) => self.agent_status = message,
                AgentEvent::Closed => {
                    self.agent_status = "Agent 通道已断开".to_string();
                    self.agent = None;
                }
            }
        }
    }

    /// Pair with the agent in the background: `pair()` needs a Tokio runtime and
    /// the QUIC handshake must not block the UI thread.
    fn pair_agent(&mut self) {
        let addr = format!(
            "{}:{}",
            self.draft.config.host, self.draft.config.agent.port
        );
        let code = self.draft.config.agent.pair_code.trim().to_string();
        if code.is_empty() {
            self.agent_status = "请输入 Mac 菜单栏显示的配对码".to_string();
            return;
        }
        let expected = self.draft.config.agent.fingerprint.trim().to_string();
        let name = local_client_name();
        let (tx, rx) = std::sync::mpsc::channel();
        self.agent_pair_rx = Some(rx);
        self.agent_status = "配对中…".to_string();
        std::thread::spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime
                    .block_on(pair(PairConfig {
                        addr,
                        code,
                        client_name: name,
                        state_dir: rustvnc_agent_client::default_state_dir(),
                        expect_fingerprint: (!expected.is_empty()).then_some(expected),
                    }))
                    .map(|outcome| outcome.server_fingerprint),
                Err(err) => Err(anyhow::anyhow!("create tokio runtime: {err}")),
            };
            let _ = tx.send(result.map_err(|err| format!("配对失败：{err:#}")));
        });
    }

    fn disconnect(&mut self) {
        // Never leave a modifier held on the remote side.
        self.sync_modifiers(ModifierState::default());
        if let Some(mut session) = self.session.take() {
            session.close();
        }
        if let Some(mut agent) = self.agent.take() {
            agent.stop();
        }
        self.agent_status.clear();
        self.status = "Disconnected".to_string();
        self.texture = None;
        self.frame_dirty = false;
        self.last_pointer = None;
    }

    fn poll_session(&mut self, ctx: &egui::Context) {
        self.poll_agent(ctx);

        let mut events = Vec::new();
        if let Some(session) = &self.session {
            while let Some(event) = session.try_recv() {
                events.push(event);
            }
        }

        let mut closed = false;
        for event in events {
            match event {
                SessionEvent::Connected { name, bpp, .. } => {
                    self.status = format!("Connected: {name}");
                    self.error = None;
                    self.frame.bpp = bpp;
                    ctx.request_repaint();
                }
                SessionEvent::Resized { width, height } => {
                    self.frame.resize(width, height);
                    self.texture = None;
                    self.pending_rects.clear();
                    self.frame_dirty = true;
                    self.status = format!("{width}×{height}");
                    ctx.request_repaint();
                }
                SessionEvent::RawImage { rect, data } => {
                    if self.frame.width == 0 || self.frame.height == 0 {
                        self.frame.resize(
                            rect.x as usize + rect.width as usize,
                            rect.y as usize + rect.height as usize,
                        );
                    }
                    if let Err(err) = self.frame.blit(rect, &data) {
                        self.error = Some(format!("frame: {err:#}"));
                    }
                    self.pending_rects.push(rect);
                    self.frame_dirty = true;
                }
                SessionEvent::Copy { dst, src } => {
                    let _ = self.frame.copy_rect(dst, src);
                    self.pending_rects.push(dst);
                    self.frame_dirty = true;
                }
                SessionEvent::Clipboard(text) => {
                    if self.clipboard_sync {
                        self.last_remote_clipboard = Some(text.clone());
                        self.last_local_clipboard = Some(text.clone());
                        if let Ok(mut clipboard) = arboard::Clipboard::new() {
                            let _ = clipboard.set_text(text.clone());
                        }
                        ctx.copy_text(text);
                    }
                }
                SessionEvent::Stats { fps, rects, bytes } => {
                    self.stats = format!("{fps:.1} fps · {rects} rects · {} KiB", bytes / 1024);
                }
                SessionEvent::Error {
                    code,
                    message,
                    retryable,
                } => {
                    self.error = Some(format!("[{code}] {message}"));
                    if !retryable || !self.draft.config.auto_reconnect {
                        self.status = "Disconnected".to_string();
                        closed = true;
                    } else {
                        self.status = "Waiting to reconnect…".to_string();
                    }
                }
                SessionEvent::Reconnecting { attempt, delay_ms } => {
                    self.status = format!(
                        "Connection lost; retry #{attempt} in {:.1}s",
                        delay_ms as f64 / 1000.0
                    );
                }
                SessionEvent::Closed => {
                    closed = true;
                }
            }
        }
        if closed {
            self.session = None;
            self.status = "Disconnected".to_string();
        }
    }

    fn update_texture(&mut self, ctx: &egui::Context) {
        if !self.frame_dirty || self.frame.width == 0 || self.frame.height == 0 {
            return;
        }
        let size = (self.frame.width, self.frame.height);
        let recreate = self.texture.is_none() || self.last_texture_size != size;
        if recreate {
            let image = self.frame.to_color_image();
            self.texture =
                Some(ctx.load_texture("vnc-framebuffer", image, egui::TextureOptions::NEAREST));
            self.last_texture_size = size;
            self.pending_rects.clear();
        } else if let Some(texture) = self.texture.as_mut() {
            for rect in self.pending_rects.drain(..) {
                if let Some(image) = self.frame.region_color_image(rect) {
                    texture.set_partial(
                        [rect.x as usize, rect.y as usize],
                        image,
                        egui::TextureOptions::NEAREST,
                    );
                }
            }
        }
        self.frame_dirty = false;
    }

    fn session_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let lang = self.lang;
        ui.horizontal(|ui| {
            ui.heading(lang.t("远程桌面", "Remote desktop"));
            ui.separator();
            ui.label(&self.status);
            if !self.stats.is_empty() {
                ui.label(&self.stats);
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(lang.t("断开", "Disconnect")).clicked() {
                    self.disconnect();
                }
                let current = self.draft.config.quality;
                egui::ComboBox::from_id_salt("session-quality")
                    .selected_text(current.label())
                    .show_ui(ui, |ui| {
                        for quality in QualityPreset::ALL {
                            if ui
                                .selectable_value(
                                    &mut self.draft.config.quality,
                                    quality,
                                    quality.label(),
                                )
                                .clicked()
                            {
                                self.draft.config.apply_preset(quality);
                            }
                        }
                    });
                ui.label(lang.t("画质（重连生效）", "Quality (reconnect)"));
                ui.checkbox(&mut self.clipboard_sync, lang.t("剪贴板", "Clipboard"));
                self.special_keys_menu(ui);
                if ui.button(lang.t("发送剪贴板", "Send clipboard")).clicked()
                    && let Ok(mut clipboard) = arboard::Clipboard::new()
                    && let Ok(text) = clipboard.get_text()
                    && let Some(session) = &self.session
                {
                    session.send_input(X11Event::CopyText(text));
                }
            });
        });
        ui.separator();

        self.update_texture(ctx);
        let available = ui.available_size();
        let Some(texture) = self.texture.as_ref() else {
            ui.centered_and_justified(|ui| {
                ui.label(lang.t("正在等待第一帧…", "Waiting for the first frame…"));
            });
            return;
        };

        let size = egui::vec2(self.frame.width as f32, self.frame.height as f32);
        let scale = (available.x / size.x).min(available.y / size.y).min(1.0);
        let display = size * scale;
        let response = ui.add(
            egui::Image::new(egui::load::SizedTexture::new(texture.id(), display))
                .sense(egui::Sense::click_and_drag()),
        );
        if response.clicked() {
            response.request_focus();
        }
        self.image_rect = Some(response.rect);
    }

    fn form_ui(&mut self, ui: &mut egui::Ui) {
        let lang = self.lang;
        ui.heading(lang.t("连接", "Connection"));
        if self.profiles.profiles().is_empty() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.strong(lang.t("首次使用", "First run"));
                ui.label(lang.t(
                    "填写 Mac 的局域网或 Tailscale 地址、端口 5900，以及 macOS 账号和密码。建议在“安全”中选择 SSH 隧道。",
                    "Enter the Mac LAN/Tailscale address, port 5900, and your macOS account password. Prefer the SSH tunnel security mode.",
                ));
            });
            ui.add_space(8.0);
        }
        ui.add_space(4.0);

        egui::Grid::new("connection-form")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                ui.label(lang.t("名称", "Name"));
                ui.text_edit_singleline(&mut self.draft.name);
                ui.end_row();

                ui.label(lang.t("主机", "Host"));
                ui.text_edit_singleline(&mut self.draft.config.host);
                ui.end_row();

                ui.label(lang.t("端口", "Port"));
                ui.add(egui::DragValue::new(&mut self.draft.config.port).range(1..=65535));
                ui.end_row();

                ui.label(lang.t("认证", "Auth"));
                egui::ComboBox::from_id_salt("auth")
                    .selected_text(self.draft.config.auth.label())
                    .show_ui(ui, |ui| {
                        for auth in AuthMode::ALL {
                            ui.selectable_value(&mut self.draft.config.auth, auth, auth.label());
                        }
                    });
                ui.end_row();

                if self.draft.config.auth != AuthMode::Vnc {
                    ui.label(lang.t("macOS 用户", "macOS user"));
                    ui.text_edit_singleline(&mut self.draft.config.user);
                    ui.end_row();
                }

                ui.label(lang.t("自动重连", "Auto reconnect"));
                ui.checkbox(
                    &mut self.draft.config.auto_reconnect,
                    lang.t("断线后自动重连", "Reconnect after disconnect"),
                );
                ui.end_row();

                ui.label(lang.t("密码", "Password"));
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .desired_width(240.0),
                    );
                    ui.checkbox(&mut self.remember, lang.t("记住", "Remember"));
                });
                ui.end_row();

                ui.label(lang.t("画质（重连生效）", "Quality (reconnect)"));
                egui::ComboBox::from_id_salt("quality")
                    .selected_text(self.draft.config.quality.label())
                    .show_ui(ui, |ui| {
                        for quality in QualityPreset::ALL {
                            if ui
                                .selectable_value(
                                    &mut self.draft.config.quality,
                                    quality,
                                    quality.label(),
                                )
                                .clicked()
                            {
                                self.draft.config.apply_preset(quality);
                            }
                        }
                    });
                ui.end_row();

                if self.draft.config.quality == QualityPreset::Custom {
                    ui.label(lang.t("位深", "BPP"));
                    egui::ComboBox::from_id_salt("bpp")
                        .selected_text(format!("{} bpp", self.draft.config.bpp))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.draft.config.bpp, 32, "32 bpp");
                            ui.selectable_value(&mut self.draft.config.bpp, 16, "16 bpp");
                        });
                    ui.end_row();

                    ui.label(lang.t("编码", "Encoding"));
                    egui::ComboBox::from_id_salt("encoding")
                        .selected_text(self.draft.config.encoding.label())
                        .show_ui(ui, |ui| {
                            for encoding in EncodingPreference::ALL {
                                ui.selectable_value(
                                    &mut self.draft.config.encoding,
                                    encoding,
                                    encoding.label(),
                                );
                            }
                        });
                    ui.end_row();
                }

                ui.label(lang.t("Apple 扩展", "Apple encodings"));
                ui.checkbox(
                    &mut self.draft.config.apple_encodings,
                    lang.t("请求 1104/1105/1109/1110（实验）", "Request 1104/1105/1109/1110 (experimental)"),
                )
                .on_hover_text(lang.t(
                    "只为真机验证准备：这些私有编码的内容尚未在目标 macOS 版本上确认。",
                    "For real-device validation only: the payload layout is not confirmed against the target macOS build.",
                ));
                ui.end_row();

                ui.label(lang.t("增强通道", "Agent channel"));
                ui.horizontal(|ui| {
                    ui.checkbox(
                        &mut self.draft.config.agent.enabled,
                        lang.t("剪贴板/音频", "Clipboard/audio"),
                    )
                    .on_hover_text(lang.t(
                        "需要在 Mac 上安装 RustVNC Agent；视频仍走 RFB。",
                        "Requires the RustVNC Agent on the Mac; video stays on RFB.",
                    ));
                    ui.add_enabled(
                        self.draft.config.agent.enabled,
                        egui::Checkbox::new(
                            &mut self.draft.config.agent.audio,
                            lang.t("音频", "Audio"),
                        ),
                    );
                    ui.label(": ");
                    ui.add_enabled(
                        self.draft.config.agent.enabled,
                        egui::DragValue::new(&mut self.draft.config.agent.port).range(1..=65535),
                    );
                });
                ui.end_row();

                if self.draft.config.agent.enabled {
                    ui.label(lang.t("配对码", "Pairing code"));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.draft.config.agent.pair_code)
                                .desired_width(96.0)
                                .hint_text("Mac 菜单栏"),
                        );
                        if ui.button(lang.t("配对", "Pair")).clicked() {
                            self.pair_agent();
                        }
                    });
                    ui.end_row();

                    ui.label(lang.t("Agent 状态", "Agent status"));
                    let status = if self.agent_status.is_empty() {
                        lang.t("未连接", "Not connected").to_string()
                    } else {
                        self.agent_status.clone()
                    };
                    ui.label(status);
                    ui.end_row();
                }

                ui.label(lang.t("安全", "Security"));
                egui::ComboBox::from_id_salt("security")
                    .selected_text(self.draft.config.security.label())
                    .show_ui(ui, |ui| {
                        for security in SecurityMode::ALL {
                            ui.selectable_value(
                                &mut self.draft.config.security,
                                security,
                                security.label(),
                            );
                        }
                    });
                ui.end_row();

                if self.draft.config.security == SecurityMode::Ssh {
                    ui.label(lang.t("SSH 用户", "SSH user"));
                    ui.text_edit_singleline(&mut self.draft.config.ssh.user);
                    ui.end_row();

                    ui.label(lang.t("SSH 端口", "SSH port"));
                    ui.add(egui::DragValue::new(&mut self.draft.config.ssh.port).range(1..=65535));
                    ui.end_row();

                    ui.label(lang.t("SSH 私钥", "SSH key"));
                    ui.text_edit_singleline(&mut self.draft.config.ssh.key_path);
                    ui.end_row();

                    ui.label(lang.t("known_hosts", "known_hosts"));
                    ui.text_edit_singleline(&mut self.draft.config.ssh.known_hosts);
                    ui.end_row();

                    ui.label(lang.t("主机密钥", "Host key"));
                    ui.horizontal(|ui| {
                        if ui.button(lang.t("探测指纹", "Fetch fingerprint")).clicked() {
                            self.probe_host_key();
                        }
                        if ui
                            .add_enabled(
                                self.host_key_probe.is_some(),
                                egui::Button::new(lang.t("我已核对，信任", "Verified, trust it")),
                            )
                            .clicked()
                        {
                            self.trust_host_key();
                        }
                    });
                    ui.end_row();

                    if let Some(probe) = &self.host_key_probe {
                        ui.label("");
                        ui.label(egui::RichText::new(&probe.fingerprint).monospace().small());
                        ui.end_row();
                    }

                    ui.label(lang.t("SSH 密码", "SSH password"));
                    ui.add(
                        egui::TextEdit::singleline(&mut self.draft.config.ssh.password)
                            .password(true)
                            .hint_text(lang.t(
                                "仅密钥认证时留空",
                                "Leave empty when using a key",
                            )),
                    );
                    ui.end_row();

                    ui.label(lang.t("密钥", "Key"));
                    ui.horizontal(|ui| {
                        if ui.button(lang.t("生成密钥", "Generate key")).clicked() {
                            self.generate_ssh_key();
                        }
                    });
                    ui.end_row();
                }
            });

        ui.add_space(12.0);
        ui.horizontal(|ui| {
            if ui.button(lang.t("保存配置", "Save profile")).clicked() {
                self.save_profile();
            }
            if ui.button(lang.t("连接", "Connect")).clicked() {
                self.connect();
            }
            if ui.button(lang.t("清空错误", "Clear error")).clicked() {
                self.error = None;
            }
        });
        ui.add_space(8.0);
        if let Some(info) = self.info.clone() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                // Read-only multiline so the public key can be selected and
                // copied; nothing here is a secret.
                let mut text = info;
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .desired_rows(2)
                        .font(egui::TextStyle::Monospace),
                );
                if ui.button(lang.t("知道了", "Dismiss")).clicked() {
                    self.info = None;
                }
            });
        }
        ui.label(lang.t(
            "提示：SSH 隧道使用系统 OpenSSH，仅支持密钥认证，主机密钥变更会被拒绝。",
            "SSH uses system OpenSSH with key auth only; host key changes are rejected.",
        ));
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        let Some(image_rect) = self.image_rect else {
            return;
        };
        if self.session.is_none() || self.frame.width == 0 || self.frame.height == 0 {
            return;
        }

        // Mirror the local modifier state to the remote every frame. Doing it
        // here (instead of synthesising modifiers around each keystroke) is
        // what makes Ctrl+click, Shift+drag and modifier-only presses behave;
        // releasing on focus loss also prevents keys getting stuck remotely.
        let focused = ctx.input(|input| input.focused);
        let local_modifiers = if focused {
            ModifierState::from_egui(ctx.input(|input| input.modifiers))
        } else {
            ModifierState::default()
        };
        self.sync_modifiers(local_modifiers);

        let (hover, primary, secondary, middle, scroll) = ctx.input(|input| {
            (
                input.pointer.hover_pos(),
                input.pointer.button_down(egui::PointerButton::Primary),
                input.pointer.button_down(egui::PointerButton::Secondary),
                input.pointer.button_down(egui::PointerButton::Middle),
                input.raw_scroll_delta.y,
            )
        });

        let mut mask = 0u8;
        if primary {
            mask |= 1;
        }
        if middle {
            mask |= 2;
        }
        if secondary {
            mask |= 4;
        }

        if let Some(position) = hover
            && image_rect.contains(position)
        {
            let relative = (position - image_rect.min) / image_rect.size();
            let x = (relative.x * self.frame.width as f32).clamp(0.0, (self.frame.width - 1) as f32)
                as u16;
            let y = (relative.y * self.frame.height as f32)
                .clamp(0.0, (self.frame.height - 1) as f32) as u16;
            if self.last_pointer != Some((x, y, mask)) {
                self.send_pointer(x, y, mask);
                self.last_pointer = Some((x, y, mask));
            }
        }

        if scroll.abs() > 1.0
            && image_rect.contains(ctx.input(|i| i.pointer.hover_pos()).unwrap_or_default())
        {
            let button = if scroll > 0.0 { 4 } else { 5 };
            let wheel_mask = 1u8 << (button - 1);
            self.send_pointer(
                self.last_pointer.map(|(x, _, _)| x).unwrap_or(0),
                self.last_pointer.map(|(_, y, _)| y).unwrap_or(0),
                wheel_mask,
            );
            self.send_pointer(
                self.last_pointer.map(|(x, _, _)| x).unwrap_or(0),
                self.last_pointer.map(|(_, y, _)| y).unwrap_or(0),
                0,
            );
        }

        if !ctx.wants_keyboard_input() {
            let events = ctx.input(|input| input.events.clone());
            for event in events {
                match event {
                    egui::Event::Key {
                        key,
                        physical_key,
                        pressed: true,
                        repeat,
                        modifiers,
                        ..
                    } => {
                        let physical = physical_key.unwrap_or(key);
                        if is_special_key(key) {
                            // Arrows/Enter/Tab repeat is useful; keep it.
                            if let Some(keysym) = key_to_keysym(physical, modifiers.shift) {
                                self.send_key(keysym, true);
                                self.send_key(keysym, false);
                            }
                        } else if !repeat
                            && (local_modifiers.ctrl
                                || local_modifiers.alt
                                || local_modifiers.command)
                        {
                            // Shortcut: the modifiers are already held remotely,
                            // so only the key itself is sent (Cmd+W, Ctrl+C, …).
                            if let Some(keysym) = key_to_keysym(physical, modifiers.shift) {
                                self.send_key(keysym, true);
                                self.send_key(keysym, false);
                            }
                        }
                        // Plain characters arrive through Event::Text below.
                    }
                    egui::Event::Text(text) => {
                        if text.is_empty() {
                            continue;
                        }
                        // A modified keystroke is a shortcut, not text.
                        if local_modifiers.ctrl || local_modifiers.alt || local_modifiers.command {
                            continue;
                        }
                        // The engine already applied Shift/CapsLock/IME when it
                        // produced this text, so the remote must not also see a
                        // modifier held on top of the character we send.
                        let restore = self.sent_modifiers;
                        if restore != ModifierState::default() {
                            self.sync_modifiers(ModifierState::default());
                        }
                        for keysym in text_to_keysyms(&text) {
                            self.send_key(keysym, true);
                            self.send_key(keysym, false);
                        }
                        if restore != ModifierState::default() {
                            self.sync_modifiers(restore);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    /// Send the transitions that bring the remote modifier state to `target`.
    /// Presses run in a fixed order and releases in reverse so the remote never
    /// sees a shortcut key before the modifiers it belongs to.
    fn sync_modifiers(&mut self, target: ModifierState) {
        if self.session.is_none() {
            self.sent_modifiers = target;
            return;
        }
        let current = self.sent_modifiers;
        if current == target {
            return;
        }
        for (keysym, down) in ModifierState::transitions(current, target) {
            self.send_key(keysym, down);
        }
        self.sent_modifiers = target;
    }

    /// Send a fixed key combination regardless of what the local keyboard is
    /// doing — the "special keys" menu (Cmd+Tab, Cmd+Space, …). Without this
    /// there is no way to reach shortcuts egui never reports as key events.
    fn send_combo(&mut self, keysyms: &[u32]) {
        self.sync_modifiers(ModifierState::default());
        for keysym in keysyms {
            self.send_key(*keysym, true);
        }
        for keysym in keysyms.iter().rev() {
            self.send_key(*keysym, false);
        }
    }

    fn special_keys_menu(&mut self, ui: &mut egui::Ui) {
        ui.menu_button("⌘ 特殊键", |ui| {
            const CMD: u32 = 0xffeb; // Super_L → Command
            const CTRL: u32 = 0xffe3;
            const OPT: u32 = 0xffe9;
            const SHIFT: u32 = 0xffe1;
            const TAB: u32 = 0xff09;
            const ESC: u32 = 0xff1b;
            const SPACE: u32 = 0x20;
            let items: [(&str, &str, &[u32]); 7] = [
                ("⌘Tab", "切换应用 / App switcher", &[CMD, TAB]),
                ("⌘Space", "聚焦搜索 / Spotlight", &[CMD, SPACE]),
                ("⌘Q", "退出应用 / Quit app", &[CMD, 0x71]),
                ("⌘W", "关闭窗口 / Close window", &[CMD, 0x77]),
                ("⌃⌘Q", "锁屏 / Lock screen", &[CTRL, CMD, 0x71]),
                ("⌘⌥Esc", "强制退出 / Force quit", &[CMD, OPT, ESC]),
                ("⌘⇧3", "截屏 / Screenshot", &[CMD, SHIFT, 0x33]),
            ];
            for (combo, name, keys) in items {
                if ui.button(format!("{combo}  {name}")).clicked() {
                    self.send_combo(keys);
                }
            }
        });
    }

    fn send_key(&self, keysym: u32, down: bool) {
        if let Some(session) = &self.session {
            session.send_input(X11Event::KeyEvent(ClientKeyEvent {
                keycode: keysym,
                down,
            }));
        }
    }

    fn send_pointer(&self, x: u16, y: u16, mask: u8) {
        if let Some(session) = &self.session {
            session.send_input(X11Event::PointerEvent(ClientMouseEvent {
                position_x: x,
                position_y: y,
                bottons: mask,
            }));
        }
    }

    fn sync_clipboard(&mut self) {
        if !self.clipboard_sync || (self.session.is_none() && self.agent.is_none()) {
            return;
        }
        if self.last_clipboard_poll.elapsed() < Duration::from_millis(700) {
            return;
        }
        self.last_clipboard_poll = Instant::now();
        let Some(text) = read_local_clipboard() else {
            return;
        };
        if text.is_empty() || self.last_local_clipboard.as_deref() == Some(text.as_str()) {
            return;
        }
        if self.last_remote_clipboard.as_deref() == Some(text.as_str()) {
            self.last_local_clipboard = Some(text);
            return;
        }
        // RFB carries the clipboard for third-party servers; macOS itself only
        // moves it over the agent channel, so both paths get the text.
        if let Some(session) = &self.session {
            session.send_input(X11Event::CopyText(text.clone()));
        }
        if let Some(agent) = &self.agent {
            agent.send_clipboard(text.clone());
        }
        self.last_local_clipboard = Some(text);
    }

    fn left_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let lang = self.lang;
        ui.heading("RustVNC");
        ui.label(lang.t("macOS 远程桌面", "macOS remote desktop"));
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.text_edit_singleline(&mut self.search);
        });
        ui.add_space(4.0);

        let mut select = None;
        egui::ScrollArea::vertical()
            .max_height(420.0)
            .show(ui, |ui| {
                let query = self.search.clone();
                let profiles = self
                    .profiles
                    .search(&query)
                    .into_iter()
                    .map(|profile| {
                        (
                            profile.id.clone(),
                            profile.name.clone(),
                            profile.config.host.clone(),
                        )
                    })
                    .collect::<Vec<_>>();
                for (id, name, host) in profiles {
                    let selected = self.selected_id.as_deref() == Some(id.as_str());
                    let label = format!("{name}\n{host}");
                    if ui.selectable_label(selected, label).clicked() {
                        select = Some(id);
                    }
                }
            });
        if let Some(id) = select {
            self.select_profile(&id);
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            if ui.button(lang.t("新建", "New")).clicked() {
                self.new_profile();
            }
            if ui.button(lang.t("删除", "Delete")).clicked() {
                self.delete_profile();
            }
        });
        ui.horizontal(|ui| {
            if ui.button(lang.t("导出到剪贴板", "Export")).clicked() {
                match self.profiles.export() {
                    Ok(json) => {
                        ctx.copy_text(json);
                        self.status = "Profiles copied to clipboard".to_string();
                    }
                    Err(err) => self.error = Some(format!("export: {err:#}")),
                }
            }
            if ui.button(lang.t("打开日志", "Open logs")).clicked()
                && let Err(err) = rustvnc_client::diag::open_log_dir()
            {
                self.error = Some(format!("open logs: {err:#}"));
            }
            if ui.button(lang.t("导出诊断", "Diagnostics")).clicked() {
                let mut summary = String::new();
                summary.push_str(&format!("status: {}\nstats: {}\n", self.status, self.stats));
                summary.push_str("profiles (host/name only):\n");
                for profile in self.profiles.profiles() {
                    summary.push_str(&format!(
                        "- {}  {}:{}  auth={:?}\n",
                        profile.name, profile.config.host, profile.config.port, profile.config.auth
                    ));
                }
                match rustvnc_client::diag::write_bundle(&summary) {
                    Ok(path) => {
                        self.status = format!("Diagnostics: {}", path.display());
                        let _ = rustvnc_client::diag::open_log_dir();
                    }
                    Err(err) => self.error = Some(format!("diagnostics: {err:#}")),
                }
            }
            if ui.button(lang.t("从剪贴板导入", "Import")).clicked() {
                match arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_text()) {
                    Ok(json) => match self.profiles.import(&json) {
                        Ok(count) => self.status = format!("Imported {count} profiles"),
                        Err(err) => self.error = Some(format!("import: {err:#}")),
                    },
                    Err(err) => self.error = Some(format!("clipboard: {err:#}")),
                }
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::LEFT), |ui| {
            ui.label(lang.t("M1 · 会话/配置/输入", "M1 · session / profiles / input"));
        });
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_session(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("RustVNC");
                ui.separator();
                ui.label(&self.status);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .button(match self.lang {
                            Lang::Zh => "EN",
                            Lang::En => "中文",
                        })
                        .clicked()
                    {
                        self.lang = match self.lang {
                            Lang::Zh => Lang::En,
                            Lang::En => Lang::Zh,
                        };
                    }
                });
            });
        });

        egui::SidePanel::left("profiles")
            .resizable(true)
            .default_width(280.0)
            .show(ctx, |ui| {
                self.left_panel(ui, ctx);
            });

        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some(error) = &self.error {
                    ui.colored_label(egui::Color32::LIGHT_RED, error);
                } else {
                    ui.label(&self.status);
                }
                if self.session.is_some() {
                    ui.separator();
                    ui.label(&self.stats);
                }
            });
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.session.is_some() {
                self.session_ui(ui, ctx);
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.form_ui(ui);
                });
            }
        });

        self.handle_input(ctx);
        self.sync_clipboard();
        ctx.request_repaint_after(Duration::from_millis(16));
    }
}

fn is_special_key(key: egui::Key) -> bool {
    matches!(
        key,
        egui::Key::Enter
            | egui::Key::Tab
            | egui::Key::Backspace
            | egui::Key::Escape
            | egui::Key::Delete
            | egui::Key::Insert
            | egui::Key::Home
            | egui::Key::End
            | egui::Key::PageUp
            | egui::Key::PageDown
            | egui::Key::ArrowLeft
            | egui::Key::ArrowUp
            | egui::Key::ArrowRight
            | egui::Key::ArrowDown
            | egui::Key::F1
            | egui::Key::F2
            | egui::Key::F3
            | egui::Key::F4
            | egui::Key::F5
            | egui::Key::F6
            | egui::Key::F7
            | egui::Key::F8
            | egui::Key::F9
            | egui::Key::F10
            | egui::Key::F11
            | egui::Key::F12
    )
}

fn read_local_clipboard() -> Option<String> {
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut clipboard| clipboard.get_text().ok())
}

#[derive(Default)]
struct Frame {
    width: usize,
    height: usize,
    bpp: u8,
    data: Vec<u8>,
}

impl Frame {
    fn stride(&self) -> usize {
        self.width * self.bpp as usize / 8
    }

    fn resize(&mut self, width: usize, height: usize) {
        if self.width != width || self.height != height {
            self.width = width;
            self.height = height;
            self.data = vec![0u8; width * height * self.bpp.max(8) as usize / 8];
        }
    }

    fn blit(&mut self, rect: Rect, source: &[u8]) -> anyhow::Result<()> {
        if rect.width == 0 || rect.height == 0 {
            return Ok(());
        }
        if self.bpp == 0 {
            self.bpp = 32;
        }
        let bpp = self.bpp as usize / 8;
        let needed_width = rect.x as usize + rect.width as usize;
        let needed_height = rect.y as usize + rect.height as usize;
        if self.width < needed_width || self.height < needed_height {
            self.resize(needed_width.max(self.width), needed_height.max(self.height));
        }
        let row_bytes = rect.width as usize * bpp;
        anyhow::ensure!(
            source.len() >= row_bytes * rect.height as usize,
            "short raw rect"
        );
        let stride = self.stride();
        for row in 0..rect.height as usize {
            let dst = (rect.y as usize + row) * stride + rect.x as usize * bpp;
            let src = row * row_bytes;
            self.data[dst..dst + row_bytes].copy_from_slice(&source[src..src + row_bytes]);
        }
        Ok(())
    }

    fn copy_rect(&mut self, dst: Rect, src: Rect) -> anyhow::Result<()> {
        let bpp = self.bpp as usize / 8;
        let width = src.width.min(dst.width) as usize;
        let height = src.height.min(dst.height) as usize;
        if width == 0 || height == 0 {
            return Ok(());
        }
        let stride = self.stride();
        anyhow::ensure!(
            src.x as usize + width <= self.width
                && src.y as usize + height <= self.height
                && dst.x as usize + width <= self.width
                && dst.y as usize + height <= self.height,
            "CopyRect out of bounds"
        );
        let mut temp = vec![0u8; width * height * bpp];
        for row in 0..height {
            let src_offset = (src.y as usize + row) * stride + src.x as usize * bpp;
            temp[row * width * bpp..(row + 1) * width * bpp]
                .copy_from_slice(&self.data[src_offset..src_offset + width * bpp]);
        }
        for row in 0..height {
            let dst_offset = (dst.y as usize + row) * stride + dst.x as usize * bpp;
            self.data[dst_offset..dst_offset + width * bpp]
                .copy_from_slice(&temp[row * width * bpp..(row + 1) * width * bpp]);
        }
        Ok(())
    }

    fn to_color_image(&self) -> egui::ColorImage {
        let mut rgba = Vec::with_capacity(self.width * self.height * 4);
        match self.bpp {
            16 => {
                for pixel in self.data.as_chunks::<2>().0 {
                    let value = u16::from_le_bytes(*pixel);
                    let r = ((value >> 11) & 0x1f) as u8 * 255 / 31;
                    let g = ((value >> 5) & 0x3f) as u8 * 255 / 63;
                    let b = (value & 0x1f) as u8 * 255 / 31;
                    rgba.extend_from_slice(&[r, g, b, 255]);
                }
            }
            _ => {
                for pixel in self.data.as_chunks::<4>().0 {
                    rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
                }
            }
        }
        egui::ColorImage::from_rgba_unmultiplied([self.width, self.height], &rgba)
    }

    fn region_color_image(&self, rect: Rect) -> Option<egui::ColorImage> {
        let x0 = rect.x as usize;
        let y0 = rect.y as usize;
        let width = rect.width as usize;
        let height = rect.height as usize;
        if width == 0 || height == 0 || x0 + width > self.width || y0 + height > self.height {
            return None;
        }
        let bpp = self.bpp as usize / 8;
        let stride = self.stride();
        let mut rgba = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            let offset = (y0 + row) * stride + x0 * bpp;
            let line = &self.data[offset..offset + width * bpp];
            match self.bpp {
                16 => {
                    for pixel in line.as_chunks::<2>().0 {
                        let value = u16::from_le_bytes(*pixel);
                        let r = ((value >> 11) & 0x1f) as u8 * 255 / 31;
                        let g = ((value >> 5) & 0x3f) as u8 * 255 / 63;
                        let b = (value & 0x1f) as u8 * 255 / 31;
                        rgba.extend_from_slice(&[r, g, b, 255]);
                    }
                }
                _ => {
                    for pixel in line.as_chunks::<4>().0 {
                        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 255]);
                    }
                }
            }
        }
        Some(egui::ColorImage::from_rgba_unmultiplied(
            [width, height],
            &rgba,
        ))
    }
}

/// Name this client announces to the agent (shown in the Mac menu bar).
fn local_client_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "rustvnc-client".to_string())
}
/// Credential-store key for the SSH password of a profile. Kept separate from
/// the VNC/ARD password so one can be rotated without the other.
fn ssh_credential_key(profile_id: &str) -> String {
    format!("{profile_id}#ssh")
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_is_not_translated_into_command() {
        // On Windows egui sets `command` whenever ctrl is down. Using that
        // field for Super_L would send Ctrl+Command for every Ctrl shortcut,
        // which is exactly the bug this guards.
        let modifiers = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        let state = ModifierState::from_egui(modifiers);
        assert!(state.ctrl);
        assert!(!state.command, "Ctrl must not become Command");
        assert_eq!(
            ModifierState::transitions(ModifierState::default(), state),
            vec![(0xffe3, true)]
        );
    }

    #[test]
    fn windows_key_maps_to_super() {
        let modifiers = egui::Modifiers {
            mac_cmd: true,
            ..Default::default()
        };
        let state = ModifierState::from_egui(modifiers);
        assert_eq!(
            ModifierState::transitions(ModifierState::default(), state),
            vec![(0xffeb, true)]
        );
    }

    #[test]
    fn releases_run_in_reverse_press_order() {
        let both = ModifierState {
            shift: true,
            ctrl: true,
            ..Default::default()
        };
        assert_eq!(
            ModifierState::transitions(both, ModifierState::default()),
            vec![(0xffe3, false), (0xffe1, false)]
        );
    }

    #[test]
    fn unchanged_modifiers_send_nothing() {
        let state = ModifierState {
            alt: true,
            ..Default::default()
        };
        assert!(ModifierState::transitions(state, state).is_empty());
    }
}
