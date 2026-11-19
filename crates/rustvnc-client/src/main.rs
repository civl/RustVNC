#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use auto_launch::{AutoLaunch, AutoLaunchBuilder};
use eframe::egui;
use rustvnc_agent_client::{
    AgentEvent, AgentSession, PairConfig, SessionConfig as AgentSessionConfig, pair,
};
use rustvnc_client::config::{
    AuthMode, ClientConfig, EncodingPreference, KeyMapping, ModifierTarget, QualityPreset,
    ResolutionPreset, SecurityMode, ViewMode,
};
use rustvnc_client::credentials::CredentialStore;
use rustvnc_client::input::{key_to_keysym, text_to_keysyms};
use rustvnc_client::profiles::{Profile, ProfileStore};
use rustvnc_client::session::{Session, SessionEvent};
use rustvnc_client::single_instance::{self, InstanceHandle, SingleInstance};
use rustvnc_client::ssh::{self, HostKeyProbe};
use rustvnc_client::ui_state::UiState;
use rustvnc_client::update::{self, UpdateInfo};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use vnc::{ClientKeyEvent, ClientMouseEvent, Rect, X11Event};

/// Base name for the single-instance guard.
///
/// The guard resolves it to `Global\<name>.<user sid>` and falls back to
/// `Local\<name>` when the machine refuses global objects, so one client runs per
/// user across logon sessions while two users never block each other.
///
/// Keep the base stable across releases: a new name lets an old and a new build
/// run side by side, which is exactly the corruption the guard prevents.
const APP_INSTANCE: &str = "RustVNC.Client";

/// Command-line driven start-up behaviour, used by the soak and benchmark
/// scripts (`scripts/bench-soak.ps1`) so they can run unattended.
#[derive(Default, Clone, Debug)]
struct StartupOptions {
    /// Profile name (or host) to connect to once the UI exists.
    connect: Option<String>,
    /// Measure local frame cadence for this many seconds, then exit.
    bench_seconds: Option<f64>,
    /// Where to write the frame-cadence summary.
    bench_out: Option<std::path::PathBuf>,
}

fn startup_options() -> StartupOptions {
    let mut options = StartupOptions::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--connect" => options.connect = args.next(),
            "--bench-frames" => options.bench_seconds = args.next().and_then(|v| v.parse().ok()),
            "--bench-out" => options.bench_out = args.next().map(std::path::PathBuf::from),
            _ => {}
        }
    }
    options
}

/// Local presentation statistics: how often the UI actually manages to present.
struct FrameBench {
    started: Instant,
    until: Instant,
    intervals_ms: Vec<f64>,
    out: Option<std::path::PathBuf>,
}
/// How the UI should be rendered, in preference order.
///
/// Windows gets a native D3D12 attempt first: it is the API the DWM compositor
/// and remote-capture tools (RustDesk, RDP) actually expect, and the OpenGL path
/// does not present at all on indirect/headless displays. Vulkan and wgpu's GL
/// backend follow, and `glow` (the old pure-OpenGL path) is the last resort.
#[derive(Debug, Clone, Copy)]
enum RenderAttempt {
    Wgpu { backends: wgpu::Backends },
    Glow,
}

fn render_attempts() -> Vec<RenderAttempt> {
    let dx12 = RenderAttempt::Wgpu {
        backends: wgpu::Backends::DX12,
    };
    let vulkan = RenderAttempt::Wgpu {
        backends: wgpu::Backends::VULKAN,
    };
    let gl = RenderAttempt::Wgpu {
        backends: wgpu::Backends::GL,
    };
    let glow = RenderAttempt::Glow;

    match std::env::var("RUSTVNC_RENDERER")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        // Explicit pinning, for diagnostics.
        "glow" | "opengl-old" => vec![glow],
        "dx12" | "d3d12" => vec![dx12],
        "vulkan" => vec![vulkan],
        "gl" | "opengl" => vec![gl],
        // "wgpu" and anything unrecognised: the full chain.
        _ => vec![dx12, vulkan, gl, glow],
    }
}

fn wgpu_configuration(backends: wgpu::Backends) -> eframe::egui_wgpu::WgpuConfiguration {
    let mut configuration = eframe::egui_wgpu::WgpuConfiguration::default();
    if let eframe::egui_wgpu::WgpuSetup::CreateNew(setup) = &mut configuration.wgpu_setup {
        // wgpu's own default is `PRIMARY | GL`, and on Windows that enumerates
        // Vulkan before D3D12 — which is how a Vulkan adapter ended up driving a
        // desktop that D3D12 handles better.
        setup.instance_descriptor.backends = backends;
        // egui-wgpu hardcodes `max_texture_dimension_2d: 8192`. A Mac driving a
        // 8960-pixel-wide display (BetterDisplay, ultrawide, or a scaled virtual
        // screen) then fails at `create_texture` with a wgpu validation error and
        // the process aborts on the first frame. Ask the adapter what it can
        // actually do and keep everything else egui-wgpu chose (notably the
        // downlevel limits it uses for the GL backend).
        let egui_descriptor = setup.device_descriptor.clone();
        setup.device_descriptor = std::sync::Arc::new(move |adapter| {
            let mut descriptor = egui_descriptor(adapter);
            let supported = adapter.limits().max_texture_dimension_2d;
            descriptor.required_limits.max_texture_dimension_2d = highest_texture_dimension(
                descriptor.required_limits.max_texture_dimension_2d,
                supported,
            );
            descriptor
        });
    }
    // A remote desktop wants the shortest possible present queue.
    configuration.desired_maximum_frame_latency = Some(1);
    configuration
}

/// The larger of what the renderer asked for and what the GPU supports: never
/// ask for less than needed, never ask for more than exists.
fn highest_texture_dimension(requested: u32, supported: u32) -> u32 {
    requested.max(supported)
}

fn main() -> eframe::Result {
    // OpenSSH invokes SSH_ASKPASS without a terminal; use this same binary as
    // the helper and keep the secret in the child's environment.
    if std::env::var_os("RUSTVNC_SSH_ASKPASS").is_some()
        || std::env::args().nth(1).as_deref() == Some("--ssh-askpass")
    {
        if let Ok(password) = std::env::var("RUSTVNC_SSH_PASSWORD") {
            println!("{password}");
        }
        return Ok(());
    }

    let _log_guard = rustvnc_client::diag::init_tracing();
    let startup = startup_options();

    // One client per session. Two copies hold their own in-memory `profiles.json`
    // and `ui.json` and save them wholesale, so the later writer silently drops
    // the other's tabs and settings. The loser wakes the winner and exits — it
    // has no window of its own to explain itself with.
    let instance = if std::env::var_os("RUSTVNC_ALLOW_MULTIPLE").is_some() {
        SingleInstance::standalone()
    } else {
        SingleInstance::acquire(APP_INSTANCE)
    };
    if !instance.is_primary() {
        let raised = single_instance::signal_existing(APP_INSTANCE);
        tracing::info!("another RustVNC instance is running (raised={raised}); exiting");
        return Ok(());
    }
    let instance_handle = instance.handle();

    let mut last_error = Ok(());
    for attempt in render_attempts() {
        match run_native(attempt, startup.clone(), instance_handle.clone()) {
            Ok(()) => return Ok(()),
            Err(err) => {
                tracing::warn!("renderer {attempt:?} unavailable: {err}");
                last_error = Err(err);
            }
        }
    }
    last_error
}

fn run_native(
    attempt: RenderAttempt,
    startup: StartupOptions,
    instance: InstanceHandle,
) -> eframe::Result {
    let renderer = match attempt {
        RenderAttempt::Wgpu { .. } => eframe::Renderer::Wgpu,
        RenderAttempt::Glow => eframe::Renderer::Glow,
    };
    let wgpu_options = match attempt {
        RenderAttempt::Wgpu { backends } => wgpu_configuration(backends),
        RenderAttempt::Glow => Default::default(),
    };
    // The window exists before `App` does, so the saved geometry has to be read
    // here rather than in `App::new`. A corrupt file must not block start-up:
    // it is logged and treated as an empty workspace.
    let ui_path = UiState::default_path();
    let ui_state = match UiState::load(&ui_path) {
        Ok(state) => state,
        Err(err) => {
            tracing::warn!("ignoring {}: {err:#}", ui_path.display());
            UiState::default()
        }
    };
    let mut viewport = egui::ViewportBuilder::default().with_inner_size([1120.0, 720.0]);
    if let Some(size) = ui_state.window.inner_size() {
        viewport = viewport.with_inner_size(size);
    }
    if let Some(position) = ui_state.window.position() {
        viewport = viewport.with_position(position);
    }
    if ui_state.window.maximized {
        viewport = viewport.with_maximized(true);
    }
    tracing::info!("starting UI with {attempt:?}");
    let options = eframe::NativeOptions {
        viewport,
        renderer,
        wgpu_options,
        ..Default::default()
    };
    eframe::run_native(
        "RustVNC",
        options,
        Box::new(move |cc| Ok(Box::new(App::new(cc, startup, ui_state, instance)))),
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

    /// Stable tag for `ui.json`; the enum itself is not serialised so a rename
    /// cannot invalidate a saved workspace.
    fn tag(self) -> &'static str {
        match self {
            Self::Zh => "zh",
            Self::En => "en",
        }
    }

    fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "zh" => Some(Self::Zh),
            "en" => Some(Self::En),
            _ => None,
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
    /// `--connect`: profile to select and connect to on the first frame.
    pending_connect: Option<String>,
    /// Physical size of the monitor this window is on, used by the Auto
    /// resolution preset.
    client_screen: Option<(u32, u32)>,
    /// The rung of the resolution ladder currently in use for Auto.
    auto_preset: Option<ResolutionPreset>,
    /// The Auto target itself: the ladder must never go below what the client
    /// screen can display, or a transient slow second would leave the user on a
    /// blurrier screen than they asked for.
    auto_floor: Option<ResolutionPreset>,
    /// When the current session was started (ladder warm-up).
    session_started: Instant,
    /// Consecutive stats samples below the frame-rate floor.
    low_fps_streak: u8,
    /// Latest measured frame rate, for the resolution ladder.
    session_fps: f64,
    /// Set once the first frame of a session has been decoded (logged for the
    /// soak script, which needs proof that pixels actually flow).
    first_frame_logged: bool,
    /// True while the session is backed off because the remote screen is static;
    /// drives the UI tick so a static session does not repaint at 60 Hz.
    session_idle: bool,
    /// `--bench-frames`: local presentation measurement.
    bench: Option<FrameBench>,
    /// The remote pointer shape, when the server sends one.
    remote_cursor: Option<RemoteCursor>,
    /// A `FramebufferUpdate` has been fully decoded and is ready to present.
    frame_complete: bool,
    /// When the first rectangle of the pending frame arrived.
    first_pending: Option<Instant>,
    /// Fullscreen hides the chrome and lets the remote image fill the screen.
    fullscreen: bool,
    /// When to stop showing the "press F11 to exit" hint.
    fullscreen_hint: Option<Instant>,
    /// Host key fetched for the current form, waiting for the user to compare
    /// the fingerprint and pin it.
    host_key_probe: Option<HostKeyProbe>,
    /// Receive side of a host-key probe that is running on its own thread;
    /// `Some` means one is in flight.
    ///
    /// `ssh-keyscan` can stall for seconds on a filtered port, so it must never
    /// be called from `update()` — that is the freeze this channel removes.
    host_key_probe_rx: Option<std::sync::mpsc::Receiver<Result<ssh::HostKeyProbe, String>>>,
    /// Multi-line, user-facing note that is not an error (e.g. the public key
    /// to install on the Mac).
    info: Option<String>,
    /// Enhanced channel to the macOS agent: clipboard + audio. Optional, and a
    /// failure here never tears down the RFB session.
    agent: Option<AgentSession>,
    agent_status: String,
    pending_resolution: Option<(u16, u16)>,
    agent_pair_rx: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// Floating key-mapping editor.
    show_keymap: bool,
    /// Tabs with inactive sessions. The active tab's session lives in `session`
    /// and its frame in `frame` so all existing rendering code stays single-tab.
    tabs: Vec<Tab>,
    active_tab: Option<usize>,
    /// Shared with the single-instance guard: another launch bumps its counter.
    instance: InstanceHandle,
    /// Last raise count the UI has handled.
    raise_seen: usize,
    /// The workspace as it was restored, kept up to date with the window
    /// geometry so [`Self::persist_ui`] can write the whole thing back.
    ui_state: UiState,
    ui_state_path: std::path::PathBuf,
    autostart: Option<AutoLaunch>,
    autostart_enabled: bool,
    _tray: Option<TrayIcon>,
    tray_rx: Option<Receiver<TrayAction>>,
    quitting: bool,
    hidden_to_tray: bool,
    start_minimized: bool,
    update_rx: Option<std::sync::mpsc::Receiver<Result<Option<UpdateInfo>, String>>>,
    update_install_rx: Option<std::sync::mpsc::Receiver<Result<(), String>>>,
    update_status: String,
    available_update: Option<UpdateInfo>,
    show_update: bool,
}

#[derive(Debug, Clone, Copy)]
enum TrayAction {
    Show,
    Quit,
}

struct Tab {
    profile: Profile,
    /// Present only while this tab is inactive; the active session is in `App`.
    session: Option<Session>,
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
    fn from_egui(modifiers: egui::Modifiers, mapping: &KeyMapping) -> Self {
        let mut state = Self {
            shift: modifiers.shift,
            ..Self::default()
        };
        let mut apply = |target: ModifierTarget| match target {
            ModifierTarget::Control => state.ctrl = true,
            ModifierTarget::Command => state.command = true,
            ModifierTarget::Option => state.alt = true,
            ModifierTarget::None => {}
        };
        if modifiers.ctrl {
            apply(mapping.ctrl);
        }
        if modifiers.alt {
            apply(mapping.alt);
        }
        // egui exposes macOS Command as `mac_cmd`; on Windows the Win key is
        // polled directly because egui-winit does not put it in `Modifiers`.
        if modifiers.mac_cmd || win_key_down() {
            apply(mapping.win);
        }
        state
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
    /// Auto resolution ladder: if the stream cannot hold `FPS_FLOOR`, ask the Mac
    /// for a smaller display mode instead of dropping frames. Pixels the client
    /// cannot show are pure cost on both ends.
    fn update_resolution_ladder(&mut self, fps: f64) {
        let Some(current) = self.auto_preset else {
            return;
        };
        // The first seconds of a session are dominated by the full-frame decode,
        // and an idle screen is supposed to run slowly: neither is evidence that
        // the link is too slow for the chosen size.
        if self.session_idle || self.session_started.elapsed() < LADDER_WARMUP {
            return;
        }
        // Never drop below the size the client screen itself can show.
        if self.auto_floor.is_some_and(|floor| current == floor) {
            return;
        }
        if fps <= 0.0 {
            // No frames yet (still connecting); not a signal.
            return;
        }
        if fps >= FPS_FLOOR {
            self.low_fps_streak = 0;
            return;
        }
        self.low_fps_streak = self.low_fps_streak.saturating_add(1);
        if self.low_fps_streak < LOW_FPS_SAMPLES {
            return;
        }
        self.low_fps_streak = 0;
        let Some(next) = current.step_down() else {
            return;
        };
        let Some((width, height)) = next.dimensions() else {
            return;
        };
        let Some(agent) = &self.agent else {
            tracing::warn!(
                "frame rate {fps:.1} fps is below {FPS_FLOOR:.0}; a smaller display mode would help but the Agent channel is not connected"
            );
            return;
        };
        agent.set_display_mode(width, height);
        self.auto_preset = Some(next);
        self.pending_resolution = Some((width, height));
        tracing::warn!(
            "frame rate {fps:.1} fps is below {FPS_FLOOR:.0}: stepping the Mac display down to {width}x{height}"
        );
        self.agent_status
            .push_str(&format!(" · 帧率不足，已降到 {width}×{height}"));
    }
    fn new(
        cc: &eframe::CreationContext<'_>,
        startup: StartupOptions,
        mut ui_state: UiState,
        instance: InstanceHandle,
    ) -> Self {
        rustvnc_client::fonts::install_cjk_font(&cc.egui_ctx);
        let path = ProfileStore::default_path();
        let (profiles, error) = match ProfileStore::load(&path) {
            Ok(profiles) => (profiles, None),
            Err(err) => (
                ProfileStore::empty(path),
                Some(format!("load profiles: {err:#}")),
            ),
        };
        let credentials = CredentialStore::system();
        // Rebuild the workspace: the saved tabs whose profile still exists, in
        // their saved order. Tabs come back without a session — restoring must
        // never dial out on its own, or an autostart at login would open N SSH
        // tunnels behind the user's back.
        ui_state.sanitize(|id| profiles.get(id).is_some());
        let tabs: Vec<Tab> = ui_state
            .tabs
            .iter()
            .filter_map(|id| profiles.get(id))
            .map(|profile| Tab {
                profile: profile.clone(),
                session: None,
            })
            .collect();
        let active_tab = ui_state.active.filter(|index| *index < tabs.len());
        let mut draft = active_tab
            .and_then(|index| tabs.get(index).map(|tab| tab.profile.clone()))
            .or_else(|| profiles.profiles().first().cloned())
            .unwrap_or_else(|| Profile::new("New Mac", ClientConfig::default()));
        // Same credential hydration as `activate_tab`: a restored tab must be
        // connectable without being switched away from and back first.
        draft.config.ssh.password = credentials
            .get(&ssh_credential_key(&draft.id))
            .ok()
            .flatten()
            .unwrap_or_default();
        let password = credentials
            .get(&draft.id)
            .ok()
            .flatten()
            .unwrap_or_default();
        let selected_id = profiles.get(&draft.id).map(|_| draft.id.clone());
        let lang = ui_state
            .lang
            .as_deref()
            .and_then(Lang::from_tag)
            .unwrap_or(Lang::Zh);
        let (tray, tray_rx) = build_tray(cc.egui_ctx.clone());
        let (autostart, autostart_enabled) = init_autostart();
        // A window hidden to the tray may not be repainting, so the guard's
        // listener needs a way to wake the UI when another launch asks for it.
        instance.set_waker({
            let ctx = cc.egui_ctx.clone();
            move || ctx.request_repaint()
        });

        Self {
            lang,
            profiles,
            selected_id,
            draft,
            password,
            remember: true,
            credentials,
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
            client_screen: None,
            auto_preset: None,
            auto_floor: None,
            session_started: Instant::now(),
            low_fps_streak: 0,
            session_fps: 0.0,
            first_frame_logged: false,
            session_idle: false,
            pending_connect: startup.connect.clone(),
            bench: startup.bench_seconds.map(|seconds| {
                let now = Instant::now();
                FrameBench {
                    started: now,
                    until: now + Duration::from_secs_f64(seconds),
                    intervals_ms: Vec::new(),
                    out: startup.bench_out.clone(),
                }
            }),
            remote_cursor: None,
            frame_complete: false,
            first_pending: None,
            fullscreen: false,
            fullscreen_hint: None,
            host_key_probe: None,
            host_key_probe_rx: None,
            info: None,
            agent: None,
            agent_status: String::new(),
            pending_resolution: None,
            agent_pair_rx: None,
            show_keymap: false,
            tabs,
            active_tab,
            ui_state,
            ui_state_path: UiState::default_path(),
            instance,
            raise_seen: 0,
            autostart,
            autostart_enabled,
            _tray: tray,
            tray_rx,
            quitting: false,
            hidden_to_tray: false,
            start_minimized: std::env::args().any(|arg| arg == "--minimized"),
            update_rx: None,
            update_install_rx: None,
            update_status: String::new(),
            available_update: None,
            show_update: false,
        }
    }

    /// Bring the window forward when another launch asked for it.
    ///
    /// The losing process has already exited; poking this one is all it can do.
    fn poll_raise_request(&mut self, ctx: &egui::Context) {
        let seen = self.instance.raise_count();
        if seen == self.raise_seen {
            return;
        }
        self.raise_seen = seen;
        // It may have been minimised to the tray, which also means it was not
        // repainting — hence the waker the guard holds.
        self.hidden_to_tray = false;
        self.start_minimized = false;
        ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        self.status = self
            .lang
            .t("已在运行的窗口中被唤起", "Raised the running window")
            .to_string();
    }

    /// Track the window geometry so a later [`Self::persist_ui`] can save it.
    ///
    /// Called every frame because a resize or a drag produces no dedicated
    /// event here, and the values are only read when something is written.
    fn capture_window_state(&mut self, ctx: &egui::Context) {
        let viewport = ctx.input(|input| input.viewport().clone());
        if let Some(rect) = viewport.inner_rect {
            let size = rect.size();
            if size.x > 0.0 && size.y > 0.0 {
                self.ui_state.window.width = Some(size.x);
                self.ui_state.window.height = Some(size.y);
            }
        }
        if let Some(rect) = viewport.outer_rect {
            self.ui_state.window.x = Some(rect.min.x);
            self.ui_state.window.y = Some(rect.min.y);
        }
        if let Some(maximized) = viewport.maximized {
            self.ui_state.window.maximized = maximized;
        }
    }

    /// Write the workspace (tabs, active tab, geometry, language).
    ///
    /// Called on every tab mutation and on exit rather than on a dirty-flag
    /// timer: tab changes are rare, the file is a couple of hundred bytes, and
    /// the write is atomic, so there is nothing to lose by being eager.
    fn persist_ui(&mut self) {
        self.ui_state.tabs = self.tabs.iter().map(|tab| tab.profile.id.clone()).collect();
        self.ui_state.active = self.active_tab;
        self.ui_state.lang = Some(self.lang.tag().to_string());
        if let Err(err) = self.ui_state.save(&self.ui_state_path) {
            tracing::warn!("save {}: {err:#}", self.ui_state_path.display());
        }
    }

    fn select_profile(&mut self, id: &str) {
        if let Some(profile) = self.profiles.get(id).cloned() {
            self.stash_active_tab();
            self.active_tab = None;
            self.session = None;
            self.agent = None;
            self.reset_visible_session();
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
            self.host_key_probe_rx = None;
            self.error = None;
            self.status = "Ready".to_string();
        }
        self.persist_ui();
    }

    /// Move the active session into its tab and pause it. The active session
    /// fields stay for the one visible tab, keeping the render path unchanged.
    fn stash_active_tab(&mut self) {
        let Some(index) = self.active_tab else {
            return;
        };
        self.sync_modifiers(ModifierState::default());
        if let Some(session) = self.session.as_ref() {
            session.set_paused(true);
        }
        if let Some(session) = self.session.take() {
            self.tabs[index].session = Some(session);
        }
        if let Some(mut agent) = self.agent.take() {
            agent.stop();
        }
        self.agent_status.clear();
        self.pending_resolution = None;
        self.tabs[index].profile = self.draft.clone();
        self.reset_visible_session();
    }

    fn reset_visible_session(&mut self) {
        self.frame = Frame::default();
        self.texture = None;
        self.frame_dirty = false;
        self.pending_rects.clear();
        self.image_rect = None;
        self.last_pointer = None;
        self.last_texture_size = (0, 0);
        self.sent_modifiers = ModifierState::default();
        self.remote_cursor = None;
        self.frame_complete = false;
        self.first_pending = None;
        self.stats.clear();
    }

    fn activate_tab(&mut self, index: usize) {
        if index >= self.tabs.len() || Some(index) == self.active_tab {
            return;
        }
        self.stash_active_tab();
        self.active_tab = Some(index);
        self.draft = self.tabs[index].profile.clone();
        self.password = self
            .credentials
            .get(&self.draft.id)
            .ok()
            .flatten()
            .unwrap_or_default();
        self.draft.config.ssh.password = self
            .credentials
            .get(&ssh_credential_key(&self.draft.id))
            .ok()
            .flatten()
            .unwrap_or_default();
        self.selected_id = Some(self.draft.id.clone());
        self.error = None;
        self.reset_visible_session();
        self.clipboard_sync = true;
        self.last_local_clipboard = read_local_clipboard();
        self.last_remote_clipboard = None;
        if let Some(session) = self.tabs[index].session.take() {
            session.set_paused(false);
            self.status = format!("Resuming {}", session.local_addr());
            self.session = Some(session);
            self.start_agent();
        } else {
            self.session = None;
            self.status = "Ready".to_string();
        }
        self.persist_ui();
    }

    fn close_tab(&mut self, index: usize) {
        if index >= self.tabs.len() {
            return;
        }
        if Some(index) == self.active_tab {
            self.disconnect();
            self.active_tab = None;
            self.tabs.remove(index);
            self.session = None;
            self.reset_visible_session();
            if !self.tabs.is_empty() {
                let next = index.min(self.tabs.len() - 1);
                self.activate_tab(next);
            } else {
                self.status = "Ready".to_string();
            }
        } else {
            let mut tab = self.tabs.remove(index);
            if let Some(mut session) = tab.session.take() {
                session.close();
            }
            if let Some(active) = self.active_tab
                && active > index
            {
                self.active_tab = Some(active - 1);
            }
        }
        self.persist_ui();
    }

    fn update_url() -> String {
        std::env::var("RUSTVNC_UPDATE_URL").unwrap_or_else(|_| {
            "https://github.com/civl/RustVNC/releases/latest/download/update.json".to_string()
        })
    }

    fn check_updates(&mut self) {
        if self.update_rx.is_some() {
            return;
        }
        let url = Self::update_url();
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_rx = Some(rx);
        self.update_status = "检查更新…".to_string();
        std::thread::spawn(move || {
            let result = update::check(&url).map_err(|err| format!("{err:#}"));
            let _ = tx.send(result);
        });
    }

    fn install_update(&mut self, ctx: &egui::Context) {
        let Some(info) = self.available_update.clone() else {
            return;
        };
        if self.update_install_rx.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.update_install_rx = Some(rx);
        self.update_status = format!("准备安装 {}…", info.version);
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let _backup = update::backup_current_exe()?;
                let extension = std::path::Path::new(&info.url)
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("exe");
                let dest = update::updates_dir()
                    .join("downloads")
                    .join(format!("RustVNC-{}.{}", info.version, extension));
                let installer = update::download_and_verify(&info, &dest)?;
                update::launch_installer(&installer)?;
                Ok(())
            })();
            let _ = tx.send(result.map_err(|err| format!("{err:#}")));
            ctx.request_repaint();
        });
    }

    fn poll_update(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &self.update_rx {
            match rx.try_recv() {
                Ok(Ok(Some(info))) => {
                    self.update_status = format!("发现新版本 {}", info.version);
                    self.show_update = true;
                    self.available_update = Some(info);
                    self.update_rx = None;
                }
                Ok(Ok(None)) => {
                    self.update_status = format!("已是最新版本 {}", update::current_version());
                    self.update_rx = None;
                }
                Ok(Err(err)) => {
                    self.update_status = format!("检查更新失败: {err}");
                    self.update_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.update_rx = None,
            }
        }
        if let Some(rx) = &self.update_install_rx {
            match rx.try_recv() {
                Ok(Ok(())) => {
                    self.update_status = "安装程序已启动，应用将退出".to_string();
                    self.quitting = true;
                    self.update_install_rx = None;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Ok(Err(err)) => {
                    self.update_status = format!("更新失败: {err}");
                    self.update_install_rx = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.update_install_rx = None;
                }
            }
        }
    }

    fn update_window(&mut self, ctx: &egui::Context) {
        let Some(info) = self.available_update.clone() else {
            self.show_update = false;
            return;
        };
        let mut open = self.show_update;
        egui::Window::new("软件更新")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.heading(format!("新版本 {}", info.version));
                ui.label(format!("当前版本 {}", update::current_version()));
                if !info.notes.is_empty() {
                    ui.separator();
                    ui.label(&info.notes);
                }
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("下载并安装").clicked() {
                        self.install_update(ctx);
                        self.show_update = false;
                    }
                    if ui.button("稍后").clicked() {
                        self.show_update = false;
                    }
                });
                ui.label("安装包必须通过 SHA-256 校验；Windows 上还要求 Authenticode 有效。");
            });
        self.show_update = open;
    }

    fn set_autostart(&mut self, enabled: bool) {
        let Some(auto) = self.autostart.as_ref() else {
            self.error = Some("当前平台不支持开机自启".to_string());
            return;
        };
        let result = if enabled {
            auto.enable()
        } else {
            auto.disable()
        };
        match result {
            Ok(()) => {
                self.autostart_enabled = enabled;
                self.status = if enabled {
                    "已开启开机自启".to_string()
                } else {
                    "已关闭开机自启".to_string()
                };
                self.error = None;
            }
            Err(err) => {
                self.autostart_enabled = !enabled;
                self.error = Some(format!("设置开机自启失败: {err}"));
            }
        }
    }

    fn handle_tray(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.tray_rx.as_ref() else {
            return;
        };
        let mut actions = Vec::new();
        while let Ok(action) = rx.try_recv() {
            actions.push(action);
        }
        for action in actions {
            match action {
                TrayAction::Show => {
                    self.hidden_to_tray = false;
                    self.status = "Ready".to_string();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    ctx.request_repaint();
                }
                TrayAction::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
    }

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        let mut activate = None;
        let mut close = None;
        ui.horizontal_wrapped(|ui| {
            for (index, tab) in self.tabs.iter().enumerate() {
                let active = Some(index) == self.active_tab;
                let title = if tab.profile.name.trim().is_empty() {
                    tab.profile.config.host.clone()
                } else {
                    tab.profile.name.clone()
                };
                if ui.selectable_label(active, title).clicked() {
                    activate = Some(index);
                }
                if ui
                    .small_button("×")
                    .on_hover_text(self.lang.t("关闭标签", "Close tab"))
                    .clicked()
                {
                    close = Some(index);
                }
                ui.separator();
            }
            if ui
                .button("+")
                .on_hover_text(self.lang.t("新建标签", "New tab"))
                .clicked()
            {
                activate = None;
                close = None;
                self.stash_active_tab();
                self.active_tab = None;
                self.session = None;
                self.agent = None;
                self.reset_visible_session();
                self.draft = Profile::new("New Mac", ClientConfig::default());
                self.password.clear();
                self.selected_id = None;
                self.status = "Ready".to_string();
                self.persist_ui();
            }
        });
        if let Some(index) = activate {
            self.activate_tab(index);
        } else if let Some(index) = close {
            self.close_tab(index);
        }
    }

    /// Fetch the host key and show its SHA256 fingerprint. Nothing is trusted
    /// until the user confirms it and [`Self::trust_host_key`] runs.
    ///
    /// The scan itself runs on a worker thread: `ssh-keyscan` opens network
    /// connections, and calling it from `update()` blocked the whole window for
    /// as long as the scan took (seconds, and unbounded on a filtered port).
    /// [`Self::poll_host_key_probe`] collects the result.
    fn probe_host_key(&mut self) {
        if self.host_key_probe_rx.is_some() {
            return;
        }
        let host = self.draft.config.host.clone();
        let port = self.draft.config.ssh.port;
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("rustvnc-host-key-probe".to_string())
            .spawn(move || {
                let result = ssh::probe_host_key(&host, port).map_err(|err| format!("{err:#}"));
                let _ = tx.send(result);
            });
        match spawned {
            Ok(_) => {
                self.host_key_probe_rx = Some(rx);
                self.host_key_probe = None;
                self.error = None;
                self.status = "正在探测主机密钥…".to_string();
            }
            Err(err) => self.error = Some(format!("无法启动探测线程：{err}")),
        }
    }

    /// Collect a finished host-key probe, if one is in flight.
    fn poll_host_key_probe(&mut self) {
        let Some(rx) = self.host_key_probe_rx.take() else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(probe)) => {
                let host = self.draft.config.host.clone();
                let port = self.draft.config.ssh.port;
                self.status = format!("已获取 {host}:{port} 的主机密钥，请核对指纹");
                self.host_key_probe = Some(probe);
                self.error = None;
            }
            Ok(Err(message)) => {
                self.host_key_probe = None;
                self.error = Some(message);
                self.status = "主机密钥探测失败".to_string();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                self.host_key_probe_rx = Some(rx);
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.host_key_probe = None;
                self.error = Some("主机密钥探测线程意外结束".to_string());
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
        self.stash_active_tab();
        self.active_tab = None;
        self.session = None;
        self.agent = None;
        self.reset_visible_session();
        self.draft = Profile::new("New Mac", ClientConfig::default());
        self.selected_id = None;
        self.password.clear();
        self.error = None;
        self.status = "Ready".to_string();
        self.persist_ui();
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
            if let Some(index) = self.active_tab {
                self.tabs[index].profile = self.draft.clone();
            }
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
        if self.active_tab.is_none() {
            self.tabs.push(Tab {
                profile: self.draft.clone(),
                session: None,
            });
            self.active_tab = Some(self.tabs.len() - 1);
        } else if let Some(index) = self.active_tab {
            self.tabs[index].profile = self.draft.clone();
        }
        self.persist_ui();
        if self.remember && !self.password.is_empty() {
            let _ = self.credentials.set(&self.draft.id, &self.password);
        } else if !self.remember {
            let _ = self.credentials.delete(&self.draft.id);
        }

        // Automation hook: `MACOS_PASSWORD` (or `RUSTVNC_PASSWORD`) supplies the
        // macOS account password for headless runs (soak, benchmarks, CI) so the
        // secret need not sit in the credential store. It is used for this
        // session only and never written back.
        let password = match env_password() {
            Some(from_env) => {
                tracing::info!("using the password from the environment");
                from_env
            }
            None => self.password.clone(),
        };
        if password.is_empty() {
            // ARD/VNC 都需要密码；空密码只会让握手卡住直到超时。
            self.error =
                Some("需要密码：在下方输入并勾选“记住”，或设置环境变量 MACOS_PASSWORD".to_string());
            self.status = "需要密码".to_string();
            return;
        }
        // Auto resolves against the client screen: never ask the Mac for more
        // pixels than this display can show.
        self.auto_preset = self
            .draft
            .config
            .resolution
            .target_for_client(self.client_screen.unwrap_or((1920, 1080)));
        self.auto_floor = self.auto_preset;
        self.session_started = Instant::now();
        self.low_fps_streak = 0;
        self.session_idle = false;
        self.pending_resolution = self.auto_preset.and_then(|preset| preset.dimensions());
        if self.draft.config.resolution == ResolutionPreset::Auto {
            match self.pending_resolution {
                Some((width, height)) => tracing::info!(
                    "auto resolution: client screen {:?} -> {width}x{height}",
                    self.client_screen
                ),
                None => tracing::info!(
                    "auto resolution: client screen {:?} fits no preset, keeping native",
                    self.client_screen
                ),
            }
        }
        match Session::start(self.draft.config.clone(), password) {
            Ok(session) => {
                self.frame = Frame::default();
                self.texture = None;
                self.frame_dirty = false;
                self.first_frame_logged = false;
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
            if self.pending_resolution.is_some() {
                tracing::warn!(
                    "resolution control needs the Agent channel; keeping the Mac display as is"
                );
            }
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
                session.set_display_layout(self.draft.config.single_display);
                if let Some((width, height)) = self.pending_resolution {
                    session.set_display_mode(width, height);
                    self.agent_status = format!("连接 Agent…（请求分辨率 {width}×{height}）");
                }
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
                    if let Some((width, height)) = self.pending_resolution.take() {
                        if capabilities & rustvnc_agent_client::CAP_DISPLAY != 0 {
                            if let Some(session) = &self.agent {
                                session.set_display_mode(width, height);
                            }
                            self.agent_status
                                .push_str(&format!(" · 请求分辨率 {width}×{height}"));
                        } else {
                            self.agent_status.push_str(" · Agent 不支持分辨率控制");
                        }
                    }
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
                AgentEvent::DisplayLayoutResult {
                    single,
                    applied,
                    message,
                } => {
                    self.agent_status = if applied {
                        if single {
                            "已切为单屏（镜像）".to_string()
                        } else {
                            "已恢复多屏".to_string()
                        }
                    } else {
                        format!("显示器布局切换失败：{message}")
                    };
                }
                AgentEvent::DisplayModeResult {
                    width,
                    height,
                    applied,
                    message,
                } => {
                    self.agent_status = if applied {
                        format!("显示分辨率已切换为 {width}×{height}")
                    } else {
                        format!("分辨率切换失败：{message}")
                    };
                }
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

    /// Brief overlay explaining how to leave immersive mode; it would otherwise
    /// be impossible to guess that F11 comes back.
    fn fullscreen_hint_ui(&mut self, ctx: &egui::Context) {
        let Some(until) = self.fullscreen_hint else {
            return;
        };
        if Instant::now() >= until {
            self.fullscreen_hint = None;
            return;
        }
        let text = self
            .lang
            .t("全屏中 · 按 F11 退出", "Fullscreen · press F11 to exit");
        egui::Area::new("fullscreen-hint".into())
            .anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 24.0))
            .show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.label(text);
                });
            });
        ctx.request_repaint_after(Duration::from_millis(250));
    }
    /// Toggle immersive mode: hide the chrome and let the remote screen fill
    /// the display. F11 and the toolbar button both land here.
    fn toggle_fullscreen(&mut self, ctx: &egui::Context) {
        if self.session.is_none() {
            return;
        }
        self.fullscreen = !self.fullscreen;
        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(self.fullscreen));
        self.fullscreen_hint = self
            .fullscreen
            .then(|| Instant::now() + Duration::from_secs(3));
        ctx.request_repaint();
    }

    fn disconnect(&mut self) {
        // Fullscreen is left by `update` once it sees there is no session; it
        // owns the context needed to resize the window back.
        if self.fullscreen {
            self.fullscreen = false;
            self.fullscreen_hint = None;
        }
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
                    self.first_pending.get_or_insert_with(Instant::now);
                    self.frame_dirty = true;
                }
                SessionEvent::Cursor {
                    hotspot,
                    size,
                    rgba,
                } => {
                    self.remote_cursor = RemoteCursor::new(ctx, hotspot, size, &rgba);
                }
                SessionEvent::FrameComplete => {
                    if !self.first_frame_logged && self.frame_dirty {
                        self.first_frame_logged = true;
                        tracing::info!("first frame {}x{}", self.frame.width, self.frame.height);
                    }
                    // Everything that belongs to this update is in the frame
                    // buffer now; present it in one go instead of sweeping.
                    self.frame_complete = true;
                    ctx.request_repaint();
                }
                SessionEvent::Copy { dst, src } => {
                    let _ = self.frame.copy_rect(dst, src);
                    self.pending_rects.push(dst);
                    self.first_pending.get_or_insert_with(Instant::now);
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
                SessionEvent::Stats {
                    fps,
                    rects,
                    bytes,
                    backed_off,
                    latency_ms,
                    adaptive_interval_ms,
                    degraded,
                } => {
                    self.stats = format!("{fps:.1} fps · {rects} rects · {} KiB", bytes / 1024);
                    if latency_ms > 0.0 {
                        self.stats.push_str(&format!(" · {latency_ms:.0}ms"));
                    }
                    if degraded {
                        self.stats.push_str(&format!(
                            " · {} {adaptive_interval_ms}ms",
                            self.lang.t("自适应降频", "adaptive")
                        ));
                    }
                    self.session_idle = backed_off;
                    self.session_fps = fps;
                    self.update_resolution_ladder(fps);
                    if backed_off {
                        // Low fps on a static desktop is the controller saving
                        // work, not a stall.
                        self.stats.push_str(self.lang.t(" · 静止降频", " · idle"));
                    }
                }
                SessionEvent::Error {
                    code,
                    message,
                    retryable,
                } => {
                    tracing::warn!("session error [{code}] {message} (retryable={retryable})");
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
        // Present whole frames only. macOS sends a 5K update as a sequence of
        // rectangles, and uploading each one as it lands paints the screen top
        // to bottom — the "scanning" the user sees while dragging a window.
        // `FrameComplete` arrives when the batch is fully decoded; the timeout
        // keeps partial feedback for servers that never finish a batch.
        let complete = self.frame_complete
            || self
                .first_pending
                .is_some_and(|at| at.elapsed() > PARTIAL_FRAME_TIMEOUT);
        if !complete {
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
        self.frame_complete = false;
        self.first_pending = None;
    }

    fn session_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, chrome: bool) {
        let lang = self.lang;
        if chrome {
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
                    if ui.button(lang.t("全屏 F11", "Fullscreen F11")).clicked() {
                        self.toggle_fullscreen(ctx);
                    }
                    let current_view = self.draft.config.view_mode;
                    let mut selected_view = current_view;
                    egui::ComboBox::from_id_salt("session-view")
                        .selected_text(current_view.label())
                        .show_ui(ui, |ui| {
                            for view in ViewMode::ALL {
                                ui.selectable_value(&mut selected_view, view, view.label());
                            }
                        });
                    if selected_view != current_view {
                        self.draft.config.view_mode = selected_view;
                        self.save_profile();
                    }
                    ui.label(lang.t("视图", "View"));
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
        }

        self.update_texture(ctx);
        let available = ui.available_size();
        let Some(texture) = self.texture.as_ref() else {
            ui.centered_and_justified(|ui| {
                ui.label(lang.t("正在等待第一帧…", "Waiting for the first frame…"));
            });
            return;
        };

        let size = egui::vec2(self.frame.width as f32, self.frame.height as f32);
        let ppp = ctx.pixels_per_point().max(0.1);
        let scale = self.draft.config.view_mode.scale(ppp).unwrap_or_else(|| {
            let fit = (available.x / size.x).min(available.y / size.y);
            // Windowed mode never enlarges beyond 1:1; fullscreen may fill.
            if chrome { fit.min(1.0) } else { fit }
        });
        let display = size * scale;
        let image = egui::Image::new(egui::load::SizedTexture::new(texture.id(), display))
            .sense(egui::Sense::click_and_drag());
        let needs_scroll = display.x > available.x || display.y > available.y;
        let response = if needs_scroll {
            egui::ScrollArea::both()
                .auto_shrink([false, false])
                .show(ui, |ui| ui.add(image))
                .inner
        } else if chrome {
            ui.add(image)
        } else {
            // Letterbox evenly instead of pinning the frame to the top-left.
            ui.centered_and_justified(|ui| ui.add(image)).inner
        };
        if response.clicked() {
            response.request_focus();
        }
        self.image_rect = Some(response.rect);
        self.paint_remote_cursor(ui, ctx);
    }

    /// Replace the local pointer with the remote shape while it is over the
    /// remote screen; otherwise two arrows would be drawn on top of each other.
    fn paint_remote_cursor(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(image_rect) = self.image_rect else {
            return;
        };
        let hovering = ctx
            .input(|input| input.pointer.hover_pos())
            .is_some_and(|position| image_rect.contains(position));
        let shape = if hovering {
            self.remote_cursor.as_ref()
        } else {
            None
        };
        let Some(cursor) = shape else {
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
            return;
        };
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(false));
        let Some((x, y, _)) = self.last_pointer else {
            return;
        };
        let rect = cursor_screen_rect(
            image_rect,
            (self.frame.width, self.frame.height),
            (x, y),
            cursor.hotspot,
            cursor.size,
        );
        let clip = ui.clip_rect();
        ui.set_clip_rect(image_rect);
        egui::Image::new(egui::load::SizedTexture::new(
            cursor.texture.id(),
            rect.size(),
        ))
        .paint_at(ui, rect);
        ui.set_clip_rect(clip);
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
                    let from_env = env_password().is_some();
                    ui.add(
                        egui::TextEdit::singleline(&mut self.password)
                            .password(true)
                            .hint_text(if from_env {
                                lang.t("已由环境变量提供，可留空", "provided by env, may stay empty")
                            } else {
                                lang.t("输入 macOS 账号密码", "macOS account password")
                            })
                            .desired_width(240.0),
                    );
                    ui.checkbox(&mut self.remember, lang.t("记住", "Remember"));
                    if from_env {
                        // Testing/automation path: no typing needed, and saying so
                        // avoids the "the app will not take my password" confusion.
                        ui.label(
                            egui::RichText::new(lang.t("使用 MACOS_PASSWORD", "using MACOS_PASSWORD"))
                                .small()
                                .color(egui::Color32::from_rgb(0x2e, 0x8b, 0x57)),
                        );
                    }
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

                ui.label(lang.t("自适应帧率", "Adaptive rate"));
                ui.checkbox(
                    &mut self.draft.config.adaptive,
                    lang.t(
                        "按延迟自动降/升（关闭=锁定画质档帧率）",
                        "Scale requests with latency (off = fixed quality rate)",
                    ),
                );
                ui.end_row();

                ui.label(lang.t("开机自启", "Launch at login"));
                let mut autostart = self.autostart_enabled;
                if ui
                    .checkbox(&mut autostart, lang.t("登录时自动启动", "Launch at login"))
                    .changed()
                {
                    self.set_autostart(autostart);
                }
                ui.end_row();

                ui.label(lang.t("远端分辨率", "Remote resolution"));
                egui::ComboBox::from_id_salt("resolution")
                    .selected_text(self.draft.config.resolution.label())
                    .show_ui(ui, |ui| {
                        for resolution in ResolutionPreset::ALL {
                            ui.selectable_value(
                                &mut self.draft.config.resolution,
                                resolution,
                                resolution.label(),
                            );
                        }
                    });
                ui.end_row();

                ui.label(lang.t("Mac 显示器", "Mac displays"));
                ui.horizontal(|ui| {
                    ui.checkbox(
                        &mut self.draft.config.single_display,
                        lang.t("只显示单屏（镜像）", "single screen (mirrored)"),
                    );
                    ui.label(
                        egui::RichText::new(lang.t(
                            "勾选后由 Agent 把多屏镜像成单屏，帧缓冲随之变小；取消则恢复多屏",
                            "the Agent mirrors the displays into one, shrinking the framebuffer",
                        ))
                        .small()
                        .weak(),
                    );
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

                    ui.label(lang.t("帧率上限", "Frame rate cap"));
                    ui.add(
                        egui::DragValue::new(&mut self.draft.config.custom_fps)
                            .range(5..=60)
                            .suffix(" fps"),
                    );
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
                        let probing = self.host_key_probe_rx.is_some();
                        if ui
                            .add_enabled(
                                !probing,
                                egui::Button::new(lang.t("探测指纹", "Fetch fingerprint")),
                            )
                            .clicked()
                        {
                            self.probe_host_key();
                        }
                        if probing {
                            ui.spinner();
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
        ui.label(lang.t(
            "远端分辨率需要启用 Agent 通道，并在 Mac 菜单栏开启“允许远程调整分辨率”；断开时自动恢复原分辨率。",
            "Remote resolution needs the agent channel and the Mac menu toggle; the original mode is restored on disconnect.",
        ));
    }

    fn handle_input(&mut self, ctx: &egui::Context) {
        if self.show_keymap {
            if self.session.is_some() {
                self.sync_modifiers(ModifierState::default());
            }
            return;
        }
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
            ModifierState::from_egui(
                ctx.input(|input| input.modifiers),
                &self.draft.config.key_mapping,
            )
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
                        // F11 is ours: it toggles fullscreen in `update`, so it
                        // must not also reach the remote as a key press.
                        if key == egui::Key::F11 {
                            continue;
                        }
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

    fn keymap_window(&mut self, ctx: &egui::Context) {
        let mut open = self.show_keymap;
        let mut mapping = self.draft.config.key_mapping.clone();
        let mut save = false;
        egui::Window::new("按键映射")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(520.0)
            .show(ctx, |ui| {
                ui.label("把 Windows 键盘的修饰键映射到 macOS。修改对当前会话立即生效。");
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .selectable_label(mapping.is_standard(), "标准")
                        .on_hover_text("Ctrl→Control，Alt→Option，Win→Command")
                        .clicked()
                    {
                        mapping = KeyMapping::standard();
                    }
                    if ui
                        .selectable_label(mapping.is_windows_style(), "Windows 习惯")
                        .on_hover_text("Ctrl→Command，Alt→Option，Win→Control")
                        .clicked()
                    {
                        mapping = KeyMapping::windows_style();
                    }
                });
                ui.separator();
                egui::Grid::new("keymap-grid")
                    .num_columns(2)
                    .spacing([24.0, 8.0])
                    .show(ui, |ui| {
                        ui.strong("Windows 按键");
                        ui.strong("发送到 macOS");
                        ui.end_row();
                        keymap_row(ui, "Ctrl", "keymap-ctrl", &mut mapping.ctrl);
                        keymap_row(ui, "Alt", "keymap-alt", &mut mapping.alt);
                        keymap_row(ui, "Win / ⌘", "keymap-win", &mut mapping.win);
                    });
                ui.add_space(8.0);
                ui.label(
                    "提示：Win 键只在 Windows 上通过系统键状态读取；macOS 客户端使用 ⌘。\
                     文本、功能键和方向键按 macOS 键位发送。",
                );
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button("恢复默认").clicked() {
                        mapping = KeyMapping::standard();
                    }
                    if ui.button("保存配置").clicked() {
                        save = true;
                    }
                });
            });
        self.show_keymap = open;
        self.draft.config.key_mapping = mapping;
        if save {
            self.save_profile();
        }
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
            if ui.button(lang.t("检查更新", "Check updates")).clicked() {
                self.check_updates();
            }
            if ui.button(lang.t("回滚", "Rollback")).clicked() {
                if let Some(path) = update::rollback_installer() {
                    match std::process::Command::new(&path).spawn() {
                        Ok(_) => {
                            self.quitting = true;
                            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        }
                        Err(err) => self.error = Some(format!("rollback: {err}")),
                    }
                } else {
                    self.update_status = "没有可回滚的版本".to_string();
                }
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

impl App {
    /// `--connect <name>`: select the profile and start the session once the UI
    /// exists, so a soak run needs no mouse.
    fn run_pending_connect(&mut self) {
        let Some(wanted) = self.pending_connect.take() else {
            return;
        };
        let lowered = wanted.to_lowercase();
        let found = self
            .profiles
            .profiles()
            .iter()
            .find(|profile| profile.name.eq_ignore_ascii_case(&wanted))
            .or_else(|| {
                self.profiles
                    .profiles()
                    .iter()
                    .find(|profile| profile.name.to_lowercase().contains(&lowered))
            })
            .or_else(|| {
                self.profiles
                    .profiles()
                    .iter()
                    .find(|profile| profile.config.host == wanted)
            })
            .map(|profile| profile.id.clone());
        match found {
            Some(id) => {
                self.select_profile(&id);
                self.connect();
                tracing::info!("--connect: started session for profile {id}");
            }
            None => {
                self.error = Some(format!("--connect: 找不到配置 “{wanted}”"));
                tracing::warn!("--connect: no profile matched {wanted}");
            }
        }
    }

    /// `--bench-frames <seconds>`: record the interval between presented frames
    /// and write a summary, then exit. This is the "does the UI keep up" number:
    /// on a 60 Hz display a healthy run clusters at ~16.7 ms.
    fn run_frame_bench(&mut self, ctx: &egui::Context) {
        let Some(bench) = self.bench.as_mut() else {
            return;
        };
        // `unstable_dt` is the wall-clock gap between the previous and this
        // frame, which is what the user perceives as smoothness.
        let interval_ms = f64::from(ctx.input(|input| input.unstable_dt)) * 1000.0;
        // The first frames include window creation and GPU init; they are not
        // what "smoothness" means, so skip the warm-up second.
        if interval_ms > 0.0 && bench.started.elapsed() >= Duration::from_secs(1) {
            bench.intervals_ms.push(interval_ms);
        }
        if Instant::now() < bench.until {
            ctx.request_repaint();
            return;
        }

        let mut sorted = bench.intervals_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let count = sorted.len();
        let percentile = |p: f64| -> f64 {
            if count == 0 {
                return 0.0;
            }
            let index = (((count as f64) * p / 100.0) as usize).min(count - 1);
            sorted[index]
        };
        let mean = if count == 0 {
            0.0
        } else {
            sorted.iter().sum::<f64>() / count as f64
        };
        let fps = if mean > 0.0 { 1000.0 / mean } else { 0.0 };
        let summary = format!(
            "frames={count} mean={mean:.2}ms p50={:.2}ms p95={:.2}ms p99={:.2}ms max={:.2}ms fps={fps:.1}",
            percentile(50.0),
            percentile(95.0),
            percentile(99.0),
            sorted.last().copied().unwrap_or(0.0)
        );
        println!("bench-frames: {summary}");
        tracing::info!("bench-frames: {summary}");
        if let Some(path) = &bench.out {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(path, format!("{summary}\n"));
        }
        self.bench = None;
        // Closing the window normally hides to the tray, which would leave a
        // benchmark run hanging; a benchmark run must terminate on its own.
        std::process::exit(0);
    }
}

impl eframe::App for App {
    /// Last chance to record where the window was: the event loop is going away
    /// and `update` will not run again.
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.persist_ui();
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if let Some(size) = ctx.input(|input| input.viewport().monitor_size) {
            let scale = ctx.pixels_per_point();
            let physical = (
                (size.x * scale).round() as u32,
                (size.y * scale).round() as u32,
            );
            if physical.0 > 0 && physical.1 > 0 {
                self.client_screen = Some(physical);
            }
        }
        self.capture_window_state(ctx);
        self.poll_raise_request(ctx);
        self.run_pending_connect();
        self.poll_host_key_probe();
        if self.host_key_probe_rx.is_some() {
            // An idle window is not repainted by egui, so a finished probe would
            // sit in the channel until the user moved the mouse.
            ctx.request_repaint_after(Duration::from_millis(100));
        }
        self.run_frame_bench(ctx);
        self.handle_tray(ctx);
        self.poll_update(ctx);
        if self.start_minimized && self._tray.is_some() {
            self.start_minimized = false;
            self.hidden_to_tray = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if ctx.input(|input| input.viewport().close_requested())
            && !self.quitting
            && self._tray.is_some()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden_to_tray = true;
            self.status = "已最小化到托盘".to_string();
        }
        self.poll_session(ctx);

        // App-level shortcuts run before input forwarding: otherwise F11 would
        // be sent to the Mac as well, and fullscreen would toggle the remote.
        if ctx.input(|input| input.key_pressed(egui::Key::F11)) {
            self.toggle_fullscreen(ctx);
        }

        // Immersive mode only makes sense while a session is being displayed.
        // Disconnecting (or a session that ended on its own) must also restore
        // the window, or the user is left with an empty fullscreen window.
        // A session that ended on its own must hand the pointer back.
        if self.session.is_none() && self.remote_cursor.is_some() {
            self.remote_cursor = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
        }
        if self.fullscreen && self.session.is_none() {
            self.fullscreen = false;
            self.fullscreen_hint = None;
            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
        }
        let immersive = self.fullscreen && self.session.is_some();

        if !immersive {
            egui::TopBottomPanel::top("top").show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.heading("RustVNC");
                    ui.separator();
                    ui.label(&self.status);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let keyboard = keyboard_icon(ui, self.lang);
                        if keyboard.clicked() {
                            self.show_keymap = !self.show_keymap;
                        }
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
                            self.persist_ui();
                        }
                    });
                });
                self.tab_bar(ui);
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
                    if !self.update_status.is_empty() {
                        ui.separator();
                        ui.label(&self.update_status);
                    }
                    if self.session.is_some() {
                        ui.separator();
                        ui.label(&self.stats);
                        // RFB has no way to ask macOS for a smaller framebuffer
                        // (it does not implement SetDesktopSize), so the only
                        // lever is the Mac's own display mode. Say so instead of
                        // leaving the user to wonder why 5K is heavy.
                        if self.frame.width > 2560 || self.frame.height > 1440 {
                            ui.separator();
                            ui.colored_label(
                                egui::Color32::from_rgb(214, 162, 40),
                                match self.auto_preset {
                                    // The auto target is known but cannot be
                                    // applied without the Agent; say so instead of
                                    // asking the user to do it by hand.
                                    Some(preset) if self.agent.is_none()
                                        && preset.dimensions().is_some() =>
                                    {
                                        let (width, height) = preset.dimensions().unwrap_or_default();
                                        format!(
                                            "{} {width}×{height}",
                                            self.lang.t(
                                                "远端分辨率较高：自动目标尚未应用（需要 Agent 通道）",
                                                "High remote resolution: auto target not applied (needs the Agent channel)"
                                            )
                                        )
                                    }
                                    _ => self
                                        .lang
                                        .t(
                                            "远端分辨率较高：把 Mac 的显示器设为 1920×1080 可显著降低带宽",
                                            "High remote resolution: set the Mac's display to 1920×1080 to cut bandwidth",
                                        )
                                        .to_string(),
                                },
                            );
                        }
                    }
                });
            });
        }

        let panel = if immersive {
            egui::CentralPanel::default().frame(egui::Frame::NONE)
        } else {
            egui::CentralPanel::default()
        };
        panel.show(ctx, |ui| {
            if self.session.is_some() {
                self.session_ui(ui, ctx, !immersive);
            } else {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.form_ui(ui);
                });
            }
        });

        if immersive {
            self.fullscreen_hint_ui(ctx);
        }

        if self.show_keymap {
            self.keymap_window(ctx);
        }
        if self.show_update {
            self.update_window(ctx);
        }
        self.handle_input(ctx);
        self.sync_clipboard();
        // A live session needs a 60 Hz tick to present frames; an idle window
        // does not, and repainting it at 60 Hz cost ~11 % of a core in the soak
        // run for no visible benefit.
        // Frames repaint themselves (`request_repaint` on FrameComplete), so the
        // tick only paces the UI chrome: 60 Hz while the remote screen is
        // changing, a slow tick when it is static or no session is open.
        let tick = if self.session.is_none() {
            Duration::from_millis(250)
        } else if self.session_idle {
            Duration::from_millis(100)
        } else {
            Duration::from_millis(16)
        };
        if self.quitting {
            // `on_exit` is the backstop; writing here means the geometry is the
            // one on screen rather than whatever the loop last captured.
            self.persist_ui();
        }
        ctx.request_repaint_after(tick);
    }
}

/// Password supplied by the environment, for automated runs (`MACOS_PASSWORD`
/// per this project's convention, `RUSTVNC_PASSWORD` accepted as an alias).
fn env_password() -> Option<String> {
    std::env::var("MACOS_PASSWORD")
        .or_else(|_| std::env::var("RUSTVNC_PASSWORD"))
        .ok()
        .filter(|value| !value.is_empty())
}

/// Frame rate below which the Auto resolution ladder steps the Mac display down.
const FPS_FLOOR: f64 = 30.0;
/// Ignore the first seconds of a session: the initial full-frame decode always
/// looks like a slow link.
const LADDER_WARMUP: Duration = Duration::from_secs(8);
/// Consecutive one-second samples under `FPS_FLOOR` before stepping down, so a
/// single slow second (a big scroll, a window animation) is not punished.
const LOW_FPS_SAMPLES: u8 = 3;

/// Longest a half-received frame may hold the display back before the partial
/// result is shown anyway. Servers answer every request, so this is only a
/// safety net, not the normal path.
const PARTIAL_FRAME_TIMEOUT: Duration = Duration::from_millis(250);

/// The remote pointer shape, painted over the framebuffer while the mouse is
/// over the remote screen. Without it the local Windows arrow is the only
/// feedback, so an I-beam or resize cursor never shows up.
struct RemoteCursor {
    texture: egui::TextureHandle,
    hotspot: (u16, u16),
    size: (u16, u16),
}

impl RemoteCursor {
    fn new(
        ctx: &egui::Context,
        hotspot: (u16, u16),
        size: (u16, u16),
        bgra: &[u8],
    ) -> Option<Self> {
        let rgba = bgra_to_rgba(bgra, size)?;
        let image =
            egui::ColorImage::from_rgba_unmultiplied([size.0 as usize, size.1 as usize], &rgba);
        let texture = ctx.load_texture("remote-cursor", image, egui::TextureOptions::NEAREST);
        Some(Self {
            texture,
            hotspot,
            size,
        })
    }
}

/// The engine hands cursor pixels back in the negotiated 32 bpp order (BGRA for
/// us) with the server's mask already written into the alpha byte.
fn bgra_to_rgba(bgra: &[u8], size: (u16, u16)) -> Option<Vec<u8>> {
    let (width, height) = (size.0 as usize, size.1 as usize);
    if width == 0 || height == 0 {
        return None;
    }
    let pixels = width * height;
    if bgra.len() < pixels * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity(pixels * 4);
    for pixel in bgra.as_chunks::<4>().0.iter().take(pixels) {
        rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
    }
    Some(rgba)
}

/// Where the remote cursor belongs on screen, in points.
///
/// `remote_pointer` is the position last sent to the server, so the shape tracks
/// exactly what the remote is doing rather than the local pointer, which may be
/// a frame ahead of the pixels.
fn cursor_screen_rect(
    image_rect: egui::Rect,
    framebuffer: (usize, usize),
    remote_pointer: (u16, u16),
    hotspot: (u16, u16),
    size: (u16, u16),
) -> egui::Rect {
    let scale = image_rect.width() / framebuffer.0.max(1) as f32;
    let origin = egui::pos2(
        image_rect.min.x + (f32::from(remote_pointer.0) - f32::from(hotspot.0)) * scale,
        image_rect.min.y + (f32::from(remote_pointer.1) - f32::from(hotspot.1)) * scale,
    );
    egui::Rect::from_min_size(
        origin,
        egui::vec2(f32::from(size.0) * scale, f32::from(size.1) * scale),
    )
}

fn keyboard_icon(ui: &mut egui::Ui, lang: Lang) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(30.0, 30.0), egui::Sense::click());
    let visuals = ui.style().interact(&response);
    let painter = ui.painter();
    painter.rect_filled(rect, 5.0, visuals.weak_bg_fill);
    painter.rect_stroke(rect, 5.0, visuals.bg_stroke, egui::StrokeKind::Inside);
    let key_color = visuals.fg_stroke.color.gamma_multiply(0.8);
    let inner = rect.shrink2(egui::vec2(6.0, 7.0));
    let gap = 2.0;
    let key_width = (inner.width() - gap * 2.0) / 3.0;
    let key_height = (inner.height() - gap) / 2.0;
    for row in 0..2 {
        for column in 0..3 {
            let min = egui::pos2(
                inner.min.x + column as f32 * (key_width + gap),
                inner.min.y + row as f32 * (key_height + gap),
            );
            painter.rect_filled(
                egui::Rect::from_min_size(min, egui::vec2(key_width, key_height)),
                1.5,
                key_color,
            );
        }
    }
    response.on_hover_text(lang.t("按键映射", "Key mapping"))
}

fn keymap_row(ui: &mut egui::Ui, label: &str, id: &str, target: &mut ModifierTarget) {
    ui.label(label);
    egui::ComboBox::from_id_salt(id)
        .selected_text(target.label())
        .show_ui(ui, |ui| {
            for option in ModifierTarget::ALL {
                ui.selectable_value(target, option, option.label());
            }
        });
    ui.end_row();
}

fn init_autostart() -> (Option<AutoLaunch>, bool) {
    let Ok(exe) = std::env::current_exe() else {
        return (None, false);
    };
    let path = exe.display().to_string();
    let mut builder = AutoLaunchBuilder::new();
    builder
        .set_app_name("RustVNC")
        .set_app_path(&path)
        .set_args(&["--minimized"]);
    let auto = builder.build().ok();
    let enabled = auto
        .as_ref()
        .and_then(|auto| auto.is_enabled().ok())
        .unwrap_or(false);
    (auto, enabled)
}

fn build_tray(ctx: egui::Context) -> (Option<TrayIcon>, Option<Receiver<TrayAction>>) {
    let menu = Menu::new();
    let show = MenuItem::with_id("show", "显示 RustVNC", true, None);
    let quit = MenuItem::with_id("quit", "退出", true, None);
    if menu.append(&show).is_err()
        || menu.append(&PredefinedMenuItem::separator()).is_err()
        || menu.append(&quit).is_err()
    {
        return (None, None);
    }
    let Some(icon) = make_tray_icon() else {
        return (None, None);
    };
    let tray = match TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon)
        .with_tooltip("RustVNC")
        .build()
    {
        Ok(tray) => tray,
        Err(err) => {
            eprintln!("tray icon unavailable: {err}");
            return (None, None);
        }
    };

    let (tx, rx) = std::sync::mpsc::channel();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        while let Ok(event) = MenuEvent::receiver().recv() {
            let action = match event.id().as_ref() {
                "show" => Some(TrayAction::Show),
                "quit" => Some(TrayAction::Quit),
                _ => None,
            };
            if let Some(action) = action {
                if matches!(action, TrayAction::Quit) {
                    // A hidden eframe window may stop repainting; make sure the
                    // quit action is processed before the process exits.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                }
                if tx.send(action).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        }
    });
    (Some(tray), Some(rx))
}

fn make_tray_icon() -> Option<Icon> {
    let mut rgba = vec![0u8; 16 * 16 * 4];
    for y in 0..16 {
        for x in 0..16 {
            let index = (y * 16 + x) * 4;
            let inside = (2..14).contains(&x) && (3..13).contains(&y);
            let key = (4..12).contains(&x) && (6..10).contains(&y);
            let (r, g, b, a) = if key {
                (245, 248, 255, 255)
            } else if inside {
                (32, 96, 190, 255)
            } else {
                (0, 0, 0, 0)
            };
            rgba[index..index + 4].copy_from_slice(&[r, g, b, a]);
        }
    }
    Icon::from_rgba(rgba, 16, 16).ok()
}

#[cfg(target_os = "windows")]
fn win_key_down() -> bool {
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetAsyncKeyState(v_key: i32) -> i16;
    }
    const VK_LWIN: i32 = 0x5B;
    const VK_RWIN: i32 = 0x5C;
    unsafe {
        (GetAsyncKeyState(VK_LWIN) as u16 & 0x8000) != 0
            || (GetAsyncKeyState(VK_RWIN) as u16 & 0x8000) != 0
    }
}

#[cfg(not(target_os = "windows"))]
fn win_key_down() -> bool {
    false
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
                    // Widen before scaling: `channel as u8 * 255` overflows u8
                    // (31 * 255 wraps in release to 225, i.e. ~7 after division),
                    // which collapsed every channel towards black.
                    let r = (((value >> 11) & 0x1f) * 255 / 31) as u8;
                    let g = (((value >> 5) & 0x3f) * 255 / 63) as u8;
                    let b = ((value & 0x1f) * 255 / 31) as u8;
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
                        // Same widening rule as `to_color_image`.
                        let r = (((value >> 11) & 0x1f) * 255 / 31) as u8;
                        let g = (((value >> 5) & 0x3f) * 255 / 63) as u8;
                        let b = ((value & 0x1f) * 255 / 31) as u8;
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
        let state = ModifierState::from_egui(modifiers, &KeyMapping::standard());
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
        let state = ModifierState::from_egui(modifiers, &KeyMapping::standard());
        assert_eq!(
            ModifierState::transitions(ModifierState::default(), state),
            vec![(0xffeb, true)]
        );
    }

    #[test]
    fn windows_style_swaps_ctrl_and_win() {
        let mapping = KeyMapping::windows_style();
        let ctrl = egui::Modifiers {
            ctrl: true,
            command: true,
            ..Default::default()
        };
        let state = ModifierState::from_egui(ctrl, &mapping);
        assert!(state.command, "Ctrl should become Command");
        assert!(!state.ctrl, "Ctrl must not stay Control");

        let win = egui::Modifiers {
            mac_cmd: true,
            ..Default::default()
        };
        let state = ModifierState::from_egui(win, &mapping);
        assert!(state.ctrl, "Win should become Control");
        assert!(!state.command, "Win must not stay Command");
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

#[cfg(test)]
mod pixel_tests {
    use super::*;

    /// RGB565 must survive the conversion. `channel as u8 * 255` used to wrap in
    /// release builds and turned a 16 bpp ("极速") session almost black.
    fn frame_from(pixels: &[u16]) -> Frame {
        Frame {
            width: pixels.len(),
            height: 1,
            bpp: 16,
            data: pixels
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect(),
        }
    }

    #[test]
    fn sixteen_bpp_pixels_keep_their_colour() {
        // 0xF800 pure red, 0xFFFF white, 0x8410 mid grey in RGB565.
        let frame = frame_from(&[0xF800, 0xFFFF, 0x8410]);
        let image = frame.to_color_image();
        assert_eq!(image.pixels[0], egui::Color32::from_rgb(255, 0, 0));
        assert_eq!(image.pixels[1], egui::Color32::from_rgb(255, 255, 255));
        let grey = image.pixels[2];
        assert!(
            grey.r() > 120 && grey.g() > 120 && grey.b() > 120,
            "mid grey collapsed towards black: {grey:?}"
        );
    }

    #[test]
    fn sixteen_bpp_partial_region_matches_the_full_frame() {
        let frame = frame_from(&[0xF800, 0x8410]);
        let region = frame
            .region_color_image(Rect {
                x: 1,
                y: 0,
                width: 1,
                height: 1,
            })
            .expect("region");
        assert_eq!(region.pixels[0], frame.to_color_image().pixels[1]);
        assert!(
            region.pixels[0].r() > 120,
            "grey at x=1 collapsed: {:?}",
            region.pixels[0]
        );
    }
}

#[cfg(test)]
mod renderer_tests {
    use super::*;

    fn chain_of(attempts: &[RenderAttempt]) -> Vec<String> {
        attempts
            .iter()
            .map(|attempt| match attempt {
                RenderAttempt::Wgpu { backends } => format!("{backends:?}"),
                RenderAttempt::Glow => "glow".to_string(),
            })
            .collect()
    }

    /// One test, not two: `RUSTVNC_RENDERER` is process-global, and two tests
    /// racing on it is how this suite briefly became flaky.
    ///
    /// Windows must try native D3D12 first — it is what the compositor and
    /// remote-capture tools expect, while wgpu's own default (PRIMARY | GL)
    /// enumerates Vulkan before it.
    #[test]
    fn the_renderer_chain_prefers_d3d12_and_can_be_pinned() {
        // SAFETY: this is the only test that touches `RUSTVNC_RENDERER`.
        unsafe { std::env::remove_var("RUSTVNC_RENDERER") };
        let chain = chain_of(&render_attempts());
        assert!(
            chain[0].contains("DX12"),
            "first attempt must be D3D12: {chain:?}"
        );
        assert_eq!(chain.len(), 4, "unexpected chain: {chain:?}");
        assert!(
            chain.last().unwrap().contains("glow"),
            "glow must be the last resort: {chain:?}"
        );

        for (value, expected) in [
            ("dx12", "DX12"),
            ("d3d12", "DX12"),
            ("vulkan", "VULKAN"),
            ("gl", "GL"),
            ("glow", "glow"),
        ] {
            // SAFETY: sequential within this test.
            unsafe { std::env::set_var("RUSTVNC_RENDERER", value) };
            let pinned = chain_of(&render_attempts());
            assert_eq!(
                pinned.len(),
                1,
                "{value} should pin one backend: {pinned:?}"
            );
            assert!(
                pinned[0].contains(expected),
                "{value} selected {}",
                pinned[0]
            );
        }
        // SAFETY: restore the shared environment for any later test.
        unsafe { std::env::remove_var("RUSTVNC_RENDERER") };
    }
}

#[cfg(test)]
mod cursor_tests {
    use super::*;

    #[test]
    fn bgra_pixels_become_rgba_with_the_mask_alpha() {
        // Two pixels: blue-ish opaque, red-ish transparent (mask already applied).
        let bgra = vec![
            0xff, 0x00, 0x00, 0xff, // B=255 G=0 R=0 A=255
            0x00, 0x00, 0xff, 0x00, // B=0 G=0 R=255 A=0
        ];
        let rgba = bgra_to_rgba(&bgra, (2, 1)).expect("convert");
        assert_eq!(rgba, vec![0x00, 0x00, 0xff, 0xff, 0xff, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn malformed_cursor_data_is_rejected() {
        assert!(bgra_to_rgba(&[], (0, 0)).is_none());
        assert!(bgra_to_rgba(&[1, 2, 3, 4], (2, 2)).is_none(), "short body");
    }

    /// Placement must follow the *sent* pointer position and the hotspot, so the
    /// shape lands where clicks go.
    #[test]
    fn cursor_is_placed_by_hotspot_at_display_scale() {
        let image = egui::Rect::from_min_size(egui::pos2(100.0, 50.0), egui::vec2(800.0, 450.0));
        // 1600x900 framebuffer shown at 800x450 => scale 0.5.
        let rect = cursor_screen_rect(image, (1600, 900), (400, 300), (2, 2), (24, 24));
        assert!((rect.min.x - 299.0).abs() < 0.01, "x = {}", rect.min.x);
        assert!((rect.min.y - 199.0).abs() < 0.01, "y = {}", rect.min.y);
        assert!((rect.width() - 12.0).abs() < 0.01, "w = {}", rect.width());
        assert!((rect.height() - 12.0).abs() < 0.01, "h = {}", rect.height());
    }

    #[test]
    fn a_zero_sized_framebuffer_does_not_divide_by_zero() {
        let image = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0));
        let rect = cursor_screen_rect(image, (0, 0), (5, 5), (0, 0), (10, 10));
        assert!(rect.min.x.is_finite() && rect.width().is_finite());
    }
}
