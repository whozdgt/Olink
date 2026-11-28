// Prevent an extra console window on Windows in release. (Harmless on Linux.)
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

/// Every `println!` in this crate goes through `util::print_line`, which shows
/// control characters escaped: much of what we log comes from the network
/// (phone and app names, file names, notification text), and raw escape
/// sequences in it would reach the user's terminal.
macro_rules! println {
    () => { $crate::util::print_line(String::new()) };
    ($($arg:tt)*) => { $crate::util::print_line(format!($($arg)*)) };
}

mod approval;
mod autostart;
mod ble;
mod commands;
mod desktop;
mod discovery;
mod events;
mod i18n;
mod instance;
mod ipc;
mod protocol;
mod scp;
mod settings;
mod status;
mod util;

fn main() {
    // Pick the provider explicitly so rustls never has to guess.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "olink=debug,info".into()),
        )
        .init();

    // Diagnostic CLI: `olink ble-probe scan|advertise`.
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("ble-probe") {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        if let Err(e) = rt.block_on(ble::probe::run(&args[1..])) {
            eprintln!("ble-probe error: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if args.first().map(String::as_str) == Some("net-probe") {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        if let Err(e) = rt.block_on(discovery::qr_probe::run(&args[1..])) {
            eprintln!("net-probe error: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // For the bar widget: `olink status` prints the phone's state line ("0"
    // when Olink isn't running); `olink cast` shows the phone screen.
    match args.first().map(String::as_str) {
        Some("status") => {
            match ipc::cli_request("status", serde_json::Value::Null) {
                Some(Ok(v)) => println!("{}", v.as_str().unwrap_or("0")),
                _ => println!("0"),
            }
            return;
        }
        Some("cast") => {
            match ipc::cli_request("start_cast", serde_json::json!({ "app": null })) {
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
                None => {
                    eprintln!("{}", i18n::t("error.not_running", &[]));
                    std::process::exit(1);
                }
            }
            return;
        }
        _ => {}
    }

    // The window process (started by the background process; see `ipc`).
    if args.iter().any(|a| a == "--window") {
        run_window();
        return;
    }

    // `--hidden` (used by the login autostart): tray only, no window.
    let hidden = args.iter().any(|a| a == "--hidden");
    let listener = match instance::claim(!hidden) {
        instance::Instance::Other => {
            println!("{}", i18n::t("status.already_running", &[]));
            return;
        }
        instance::Instance::First(l) => l,
    };

    tracing::info!("Olink starting{}", if hidden { " in the tray" } else { "" });
    util::migrate_old_name();
    util::tune_allocator();

    // The app offers Screencast to the phone and waits for it to be tapped
    // (there or with the app's button). Still single-threaded here, so
    // setting the variable is safe.
    if std::env::var_os("OPLUS_CAST").is_none() {
        std::env::set_var("OPLUS_CAST", "manual");
    }
    // The cast window lives on Tauri's GTK main loop.
    scp::castrx::use_external_gtk();

    let rt = small_runtime();
    tauri::async_runtime::set(rt.handle().clone());

    let app = tauri::Builder::default()
        .setup(move |app| {
            events::set_sink(move |ev| {
                match &ev {
                    // A pairing question needs the user now: open the window
                    // (it fetches the pending question when it loads).
                    events::Event::PairRequest { .. } => desktop::open_window(),
                    events::Event::Connected { .. } => desktop::set_phone_connected(true),
                    events::Event::Disconnected => desktop::set_phone_connected(false),
                    _ => {}
                }
                status::on_event(&ev);
                ipc::broadcast_event(&ev);
            });
            // Load settings before the tray so its labels match the saved language.
            let _ = settings::get();
            desktop::create_tray(app.handle())?;
            if let Some(l) = listener {
                ipc::serve(l, desktop::open_window);
            }
            if !hidden {
                desktop::open_window();
            }
            if let Err(e) = autostart::sync(settings::get().start_on_login) {
                tracing::warn!("autostart: {e:#}");
            }
            // The phone connection server (same as `net-probe pair`).
            tauri::async_runtime::spawn(async {
                if let Err(e) = discovery::qr_probe::run(&["pair".to_string()]).await {
                    tracing::error!("phone server stopped: {e:#}");
                    let busy = e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::AddrInUse);
                    let msg = if busy {
                        i18n::t("error.ports_busy", &[])
                    } else {
                        i18n::t("error.connection_stopped", &[("error", &e.to_string())])
                    };
                    events::emit(events::Event::Fatal { msg });
                }
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building the app");

    app.run(|_, event| match event {
        // No windows in this process: keep running (tray, phone link) until
        // "Quit" (an explicit exit code).
        tauri::RunEvent::ExitRequested { code: None, api, .. } => api.prevent_exit(),
        tauri::RunEvent::Exit => {
            desktop::close_window();
            status::on_quit();
        }
        _ => {}
    });
    drop(rt);
}

/// One small runtime for everything: two workers; idle blocking threads
/// exit after a few seconds.
fn small_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .max_blocking_threads(16)
        .thread_keep_alive(std::time::Duration::from_secs(5))
        .thread_name("oplus")
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// The window process: the page, talking to the background process. It
/// exits when the window is closed, taking all of the web view's memory
/// with it.
fn run_window() {
    util::tune_allocator();
    // This process shows the file dialogs, so it needs the saved language
    // before the first one opens.
    let _ = settings::get();
    // File pickers through the desktop portal; GTK falls back to its own
    // dialog when there's no portal. Single-threaded here, so this is safe.
    if std::env::var_os("GTK_USE_PORTAL").is_none() {
        std::env::set_var("GTK_USE_PORTAL", "1");
    }
    let rt = small_runtime();
    tauri::async_runtime::set(rt.handle().clone());

    tauri::Builder::default()
        .setup(|app| {
            use tauri::{Emitter, Manager};
            let (on_event, on_focus, on_closed) = (app.handle().clone(), app.handle().clone(), app.handle().clone());
            let connected = tauri::async_runtime::block_on(ipc::connect(
                move |ev| {
                    let _ = on_event.emit("phone", ev);
                },
                move || {
                    if let Some(w) = on_focus.get_webview_window("main") {
                        let _ = w.unminimize();
                        let _ = w.show();
                        let _ = w.set_focus();
                        let _ = w.request_user_attention(Some(tauri::UserAttentionType::Critical));
                    }
                },
                // The background process quit (from the tray): so do we.
                move || on_closed.exit(0),
            ));
            if let Err(e) = connected {
                // Not started by the background process (or it's gone): start it.
                tracing::warn!("no background process ({e}); starting it");
                if let Ok(exe) = std::env::current_exe() {
                    let _ = std::process::Command::new(exe).spawn();
                }
                std::process::exit(0);
            }
            let cfg = app.config().app.windows.iter().find(|w| w.label == "main").cloned().ok_or("no window config")?;
            let w = tauri::WebviewWindowBuilder::from_config(app.handle(), &cfg)?.build()?;
            let _ = w.set_focus();
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::DragDrop(tauri::DragDropEvent::Drop { paths, .. }) = event {
                if !paths.is_empty() {
                    commands::send_dropped(tauri::Manager::app_handle(window).clone(), paths.clone());
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_settings,
            commands::set_settings,
            commands::answer_pairing,
            commands::pick_files_to_send,
            commands::cancel_transfer,
            commands::open_received_folder,
            commands::ui_log,
            commands::phone_state,
            commands::start_cast,
        ])
        .run(tauri::generate_context!())
        .expect("error while running the window");
    drop(rt);
}
