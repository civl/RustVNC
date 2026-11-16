#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

#[cfg(not(target_os = "macos"))]
compile_error!("RustVNC Agent is macOS-only");

mod icon;
mod single_instance;
mod smappservice;

use std::fs;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
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

fn run_gui() -> anyhow::Result<()> {
    let mut builder = EventLoopBuilder::<()>::new();
    let mut event_loop = builder.build();
    event_loop.set_activation_policy(ActivationPolicy::Accessory);
    event_loop.set_dock_visibility(false);

    // The tray icon must be kept alive for the lifetime of the process.
    let mut _tray: Option<TrayIcon> = None;
    let mut status_item: Option<MenuItem> = None;
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
                Ok((tray, status)) => {
                    _tray = Some(tray);
                    status_item = Some(status);
                    info!("menu bar icon created");
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
            *control_flow = ControlFlow::Exit;
            return;
        }

        if last_status_check.elapsed() >= Duration::from_secs(1) {
            last_status_check = Instant::now();
            if let Some(item) = status_item.as_ref() {
                refresh_status(item);
            }
        }

        while let Ok(menu_event) = MenuEvent::receiver().try_recv() {
            match menu_event.id().as_ref() {
                "register" => {
                    info!("registering login item");
                    match smappservice::register() {
                        Ok(()) => info!("login item registered"),
                        Err(err) => error!("register failed: {err}"),
                    }
                    if let Some(item) = status_item.as_ref() {
                        refresh_status(item);
                    }
                }
                "unregister" => {
                    warn!("unregistering login item; launchd may terminate this process");
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
                    *control_flow = ControlFlow::Exit;
                }
                other => {
                    warn!("unknown menu event: {other}");
                }
            }
        }
    });
}

fn build_tray() -> anyhow::Result<(TrayIcon, MenuItem)> {
    let menu = Menu::new();

    let status = MenuItem::with_id("status", "登录项：检查中…", false, None);
    let mode = MenuItem::with_id("mode", "模式：B1（A0 仅骨架）", false, None);
    let register = MenuItem::with_id("register", "启用登录时启动", true, None);
    let unregister = MenuItem::with_id("unregister", "停用登录时启动（会退出）", true, None);
    let login_settings = MenuItem::with_id("login_settings", "打开登录项设置…", true, None);
    let logs = MenuItem::with_id("logs", "打开日志目录", true, None);
    let quit = MenuItem::with_id("quit", "退出 RustVNC Agent", true, None);

    menu.append(&status)?;
    menu.append(&mode)?;
    menu.append(&PredefinedMenuItem::separator())?;
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

    Ok((tray, status))
}

fn refresh_status(item: &MenuItem) {
    let status = smappservice::status();
    let text = format!("登录项：{}", smappservice::status_label(status));
    if item.text() != text {
        item.set_text(&text);
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
           rustvnc-agent --serve         Run the A1 QUIC control server (headless)\n\
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
            other => bail!("unknown --serve option: {other}"),
        }
        index += 1;
    }

    Ok(rustvnc_agent::server::ServeOptions {
        listen,
        state_dir,
        pair_code,
        sans,
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
