#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
compile_error!("RustVNC Agent is macOS-only");

mod icon;
mod single_instance;
mod smappservice;

use std::fs;
use std::io::Write as _;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use rustvnc_agent::paths;
use rustvnc_agent::server::{self, ServeInfo, ServeOptions, ServerEvent};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tracing::{error, info, warn};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder};

const APP_NAME: &str = "RustVNC Agent";

fn main() {
    if let Err(err) = real_main() {
        eprintln!("fatal: {err:#}");
        std::process::exit(1);
    }
}

fn real_main() -> anyhow::Result<()> {
    let _guard = init_logging();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--serve") => {
            let options = parse_serve_options(&args[1..])?;
            return run_serve(options);
        }
        Some("--status") => {
            print_status();
            return Ok(());
        }
        Some("--register") => {
            let result = smappservice::register();
            print_status();
            if let Err(err) = result {
                error!("register failed: {err}");
                anyhow::bail!("register failed: {err}");
            }
            return Ok(());
        }
        Some("--unregister") => {
            let result = smappservice::unregister();
            print_status();
            if let Err(err) = result {
                error!("unregister failed: {err}");
                anyhow::bail!("unregister failed: {err}");
            }
            return Ok(());
        }
        Some("--version") | Some("-V") => {
            println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help") | Some("-h") => {
            print_help();
            return Ok(());
        }
        Some(other) => {
            eprintln!("unknown argument: {other}");
            print_help();
            std::process::exit(2);
        }
        None => {}
    }

    let _lock = match single_instance::acquire() {
        Ok(lock) => lock,
        Err(err) => {
            // Exit 0 on purpose: launchd's KeepAlive only restarts non-zero exits,
            // so a second instance must not cause a restart loop.
            warn!("another instance is already running ({err}); exiting");
            return Ok(());
        }
    };

    info!("starting {APP_NAME} {}", env!("CARGO_PKG_VERSION"));
    run_gui()
}

#[derive(Default)]
struct ServerUi {
    running: bool,
    info: Option<ServeInfo>,
    client: Option<String>,
    code_used: bool,
    last_error: Option<String>,
    events_rx: Option<std::sync::mpsc::Receiver<ServerEvent>>,
    shutdown: Option<tokio::sync::watch::Sender<bool>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

struct MenuItems {
    server: MenuItem,
    pair: MenuItem,
    client: MenuItem,
    start_stop: MenuItem,
    login_status: MenuItem,
}

fn run_gui() -> anyhow::Result<()> {
    let mut builder = EventLoopBuilder::<()>::new();
    let mut event_loop = builder.build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);

    // The tray icon must be kept alive for the lifetime of the process.
    let mut _tray: Option<TrayIcon> = None;
    let mut menu_items: Option<MenuItems> = None;
    let mut server_ui = ServerUi::default();
    let mut last_status_check = Instant::now() - Duration::from_secs(5);

    // launchd sends SIGTERM on logout/shutdown. Exit 0 so KeepAlive does not
    // immediately respawn us.
    let terminate = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        let _ = signal_hook::flag::register(signal, Arc::clone(&terminate));
    }

    event_loop.run(move |event, _, control_flow| {
        // Wake up periodically so menu events (which arrive on the main thread)
        // are drained even when no AppKit event wakes the loop.
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250));

        if let Event::NewEvents(StartCause::Init) = event {
            match build_tray() {
                Ok((tray, items)) => {
                    _tray = Some(tray);
                    menu_items = Some(items);
                    info!("menu bar icon created");
                    start_server(&mut server_ui);
                    if let Some(items) = menu_items.as_ref() {
                        refresh_server_items(&server_ui, items);
                    }
                }
                Err(err) => {
                    error!("failed to create menu bar icon: {err:#}");
                    *control_flow = ControlFlow::Exit;
                    return;
                }
            }
        }

        if terminate.load(Ordering::Relaxed) {
            info!("termination signal received; exiting cleanly");
            stop_server(&mut server_ui);
            *control_flow = ControlFlow::Exit;
            return;
        }

        if let Some(items) = menu_items.as_ref() {
            poll_server_events(&mut server_ui, items);
        }

        if last_status_check.elapsed() >= Duration::from_secs(1) {
            last_status_check = Instant::now();
            if let Some(items) = menu_items.as_ref() {
                refresh_login_status(&items.login_status);
            }
        }

        while let Ok(menu_event) = MenuEvent::receiver().try_recv() {
            match menu_event.id().as_ref() {
                "start_stop" => {
                    if server_ui.running {
                        info!("stopping control server");
                        stop_server(&mut server_ui);
                    } else {
                        info!("starting control server");
                        start_server(&mut server_ui);
                    }
                    if let Some(items) = menu_items.as_ref() {
                        refresh_server_items(&server_ui, items);
                    }
                }
                "copy_code" => {
                    if let Some(info) = server_ui.info.as_ref().filter(|_| !server_ui.code_used) {
                        copy_text(&info.pair_code);
                        info!("pairing code copied to clipboard");
                    }
                }
                "register" => {
                    info!("registering login item");
                    match smappservice::register() {
                        Ok(()) => info!("login item registered"),
                        Err(err) => error!("register failed: {err}"),
                    }
                    if let Some(items) = menu_items.as_ref() {
                        refresh_login_status(&items.login_status);
                    }
                }
                "unregister" => {
                    warn!("unregistering login item; launchd may terminate this process");
                    stop_server(&mut server_ui);
                    match smappservice::unregister() {
                        Ok(()) => info!("login item unregistered"),
                        Err(err) => error!("unregister failed: {err}"),
                    }
                    *control_flow = ControlFlow::Exit;
                }
                "login_settings" => {
                    smappservice::open_login_items_settings();
                }
                "logs" => {
                    let _ = Command::new("open").arg(log_dir()).spawn();
                }
                "quit" => {
                    info!("quit requested");
                    stop_server(&mut server_ui);
                    *control_flow = ControlFlow::Exit;
                }
                other => {
                    warn!("unknown menu event: {other}");
                }
            }
        }
    });
}

fn build_tray() -> anyhow::Result<(TrayIcon, MenuItems)> {
    let menu = Menu::new();

    let server = MenuItem::with_id("server_status", "服务器：启动中…", false, None);
    let pair = MenuItem::with_id("copy_code", "配对码：—", false, None);
    let client = MenuItem::with_id("client_status", "客户端：未连接", false, None);
    let mode = MenuItem::with_id("mode", "模式：A2（控制面 + 剪贴板）", false, None);
    let start_stop = MenuItem::with_id("start_stop", "停止服务器", true, None);
    let login_status = MenuItem::with_id("login_status", "登录项：检查中…", false, None);
    let register = MenuItem::with_id("register", "启用登录时启动", true, None);
    let unregister = MenuItem::with_id("unregister", "停用登录时启动（会退出）", true, None);
    let login_settings = MenuItem::with_id("login_settings", "打开登录项设置…", true, None);
    let logs = MenuItem::with_id("logs", "打开日志目录", true, None);
    let quit = MenuItem::with_id("quit", "退出 RustVNC Agent", true, None);

    menu.append(&server)?;
    menu.append(&pair)?;
    menu.append(&client)?;
    menu.append(&mode)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&start_stop)?;
    menu.append(&login_status)?;
    menu.append(&register)?;
    menu.append(&unregister)?;
    menu.append(&login_settings)?;
    menu.append(&PredefinedMenuItem::separator())?;
    menu.append(&logs)?;
    menu.append(&quit)?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon_templated(icon::make_icon())
        .with_tooltip("RustVNC Agent")
        .build()
        .context("create tray icon")?;

    Ok((
        tray,
        MenuItems {
            server,
            pair,
            client,
            start_stop,
            login_status,
        },
    ))
}

fn start_server(ui: &mut ServerUi) {
    if ui.running {
        return;
    }

    let (events_tx, events_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let options = ServeOptions {
        listen: listen_addr(),
        state_dir: paths::agent_dir(),
        pair_code: std::env::var("RUSTVNC_PAIR_CODE").ok(),
        sans: Vec::new(),
        keychain: std::env::var("RUSTVNC_NO_KEYCHAIN").is_err(),
        events: Some(events_tx.clone()),
    };

    let thread = match std::thread::Builder::new()
        .name("rustvnc-server".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    let _ = events_tx.send(ServerEvent::Error(format!("runtime: {err}")));
                    return;
                }
            };
            if let Err(err) =
                runtime.block_on(server::run_with_shutdown(options, None, shutdown_rx))
            {
                let _ = events_tx.send(ServerEvent::Error(format!("{err:#}")));
            }
        }) {
        Ok(thread) => thread,
        Err(err) => {
            ui.last_error = Some(format!("spawn server thread: {err}"));
            return;
        }
    };

    ui.running = true;
    ui.info = None;
    ui.client = None;
    ui.code_used = false;
    ui.last_error = None;
    ui.events_rx = Some(events_rx);
    ui.shutdown = Some(shutdown_tx);
    ui.thread = Some(thread);
}

fn stop_server(ui: &mut ServerUi) {
    if let Some(shutdown) = ui.shutdown.take() {
        let _ = shutdown.send(true);
    }
    if let Some(thread) = ui.thread.take() {
        let _ = thread.join();
    }
    ui.running = false;
    ui.info = None;
    ui.client = None;
    ui.code_used = false;
    ui.events_rx = None;
}

fn poll_server_events(ui: &mut ServerUi, items: &MenuItems) {
    let mut events = Vec::new();
    if let Some(receiver) = ui.events_rx.as_ref() {
        while let Ok(event) = receiver.try_recv() {
            events.push(event);
        }
    }

    for event in events {
        match event {
            ServerEvent::Started(info) => {
                ui.running = true;
                ui.info = Some(info);
                ui.client = None;
                ui.code_used = false;
                ui.last_error = None;
            }
            ServerEvent::Paired { client, .. } => {
                ui.client = Some(format!("{client}（已配对，等待连接）"));
                ui.code_used = true;
            }
            ServerEvent::Connected { client, .. } => {
                ui.client = Some(format!("{client}（已连接）"));
            }
            ServerEvent::Disconnected => {
                ui.client = None;
            }
            ServerEvent::Stopped => {
                ui.running = false;
                ui.info = None;
                ui.client = None;
            }
            ServerEvent::Error(err) => {
                ui.last_error = Some(err);
                ui.running = false;
            }
        }
    }

    refresh_server_items(ui, items);
}

fn refresh_server_items(ui: &ServerUi, items: &MenuItems) {
    let server_text = if let Some(err) = ui.last_error.as_ref() {
        format!("服务器：错误（{}）", truncate(err, 48))
    } else if let Some(info) = ui.info.as_ref() {
        format!("服务器：运行中 {}", info.local_addr)
    } else if ui.running {
        "服务器：启动中…".to_string()
    } else {
        "服务器：已停止".to_string()
    };
    set_text(&items.server, server_text);

    let pair_text = if ui.code_used {
        "配对码：已使用（重启服务器生成新码）".to_string()
    } else if let Some(info) = ui.info.as_ref() {
        format!("配对码：{}（点击复制）", info.pair_code)
    } else {
        "配对码：—".to_string()
    };
    set_text(&items.pair, pair_text);
    items.pair.set_enabled(ui.info.is_some() && !ui.code_used);

    let client_text = match ui.client.as_ref() {
        Some(client) => format!("客户端：{client}"),
        None => "客户端：未连接".to_string(),
    };
    set_text(&items.client, client_text);

    let start_stop_text = if ui.running {
        "停止服务器"
    } else {
        "启动服务器"
    };
    set_text(&items.start_stop, start_stop_text.to_string());
}

fn refresh_login_status(item: &MenuItem) {
    let status = smappservice::status();
    let text = format!("登录项：{}", smappservice::status_label(status));
    set_text(item, text);
}

fn set_text(item: &MenuItem, text: String) {
    if item.text() != text {
        item.set_text(&text);
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut truncated: String = text.chars().take(max).collect();
        truncated.push('…');
        truncated
    }
}

fn listen_addr() -> SocketAddr {
    std::env::var("RUSTVNC_LISTEN")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| {
            "0.0.0.0:8443"
                .parse()
                .expect("valid default listen address")
        })
}

fn copy_text(text: &str) {
    if let Ok(mut child) = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
    {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        let _ = child.wait();
    }
}

fn print_status() {
    let status = smappservice::status();
    println!("{APP_NAME} {}", env!("CARGO_PKG_VERSION"));
    println!(
        "bundle: {}",
        std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "<unknown>".to_string())
    );
    println!("plist:  {}", smappservice::PLIST_NAME);
    println!(
        "login item: {} (raw {})",
        smappservice::status_label(status),
        status.0
    );
}

fn print_help() {
    println!(
        "{APP_NAME} {}\n\
         \n\
         Usage:\n\
           rustvnc-agent                 Run the menu bar agent\n\
           rustvnc-agent --serve [--no-keychain]  Run the A1 QUIC control server (headless)\n\
           rustvnc-agent --status        Print SMAppService status\n\
           rustvnc-agent --register      Register the bundled LaunchAgent\n\
           rustvnc-agent --unregister    Unregister the bundled LaunchAgent\n\
           rustvnc-agent --version       Print version\n",
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_serve_options(args: &[String]) -> anyhow::Result<rustvnc_agent::server::ServeOptions> {
    let mut listen: SocketAddr = "127.0.0.1:8443"
        .parse()
        .expect("valid default listen address");
    let mut state_dir = rustvnc_agent::paths::agent_dir();
    let mut pair_code = None;
    let mut sans = Vec::new();
    let mut keychain = true;

    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--listen" => {
                index += 1;
                listen = args
                    .get(index)
                    .context("--listen requires a value")?
                    .parse()
                    .context("invalid --listen address")?;
            }
            "--state-dir" => {
                index += 1;
                state_dir = PathBuf::from(args.get(index).context("--state-dir requires a value")?);
            }
            "--pair-code" => {
                index += 1;
                pair_code = Some(
                    args.get(index)
                        .context("--pair-code requires a value")?
                        .clone(),
                );
            }
            "--san" => {
                index += 1;
                sans.push(args.get(index).context("--san requires a value")?.clone());
            }
            "--no-keychain" => {
                keychain = false;
            }
            other => bail!("unknown --serve option: {other}"),
        }
        index += 1;
    }

    Ok(rustvnc_agent::server::ServeOptions {
        listen,
        state_dir,
        pair_code,
        sans,
        keychain,
        events: None,
    })
}

fn run_serve(options: rustvnc_agent::server::ServeOptions) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("create tokio runtime")?;
    runtime.block_on(rustvnc_agent::server::run(options))
}

fn log_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Library/Logs/RustVNC")
}

fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    let dir = log_dir();
    if fs::create_dir_all(&dir).is_err() {
        let _ = tracing_subscriber::fmt().try_init();
        return None;
    }

    let appender = tracing_appender::rolling::daily(&dir, "agent.log");
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking)
        .with_ansi(false)
        .try_init();

    Some(guard)
}
