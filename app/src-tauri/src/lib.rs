//! SwiftLoad desktop shell.
//!
//! Window, menus, OS integration and the IPC boundary. Every decision about *downloading*
//! lives in `swiftload-core`; this crate exists to put a window in front of it.

mod commands;
mod event_pump;
mod tray;

use std::sync::Arc;
use swiftload_core::manager::Manager;
use tauri::Manager as _;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("SWIFTLOAD_LOG")
                .unwrap_or_else(|_| "swiftload_core=info,swiftload_desktop_lib=info".into()),
        )
        .init();

    let mut builder = tauri::Builder::default();

    // A download manager with two copies of itself running has two schedulers dividing one
    // link between them and two writers on one part file. The second launch hands its work to
    // the first and exits.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder
            .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
                if let Some(w) = app.get_webview_window("main") {
                    let _ = w.unminimize();
                    let _ = w.show();
                    let _ = w.set_focus();
                }
            }))
            // Start-with-Windows. Registered with no arguments and no auto-enable: whether it
            // is on is the user's decision, made in Settings, and the plugin reads the real
            // registry state rather than a preference we keep in parallel with it.
            .plugin(tauri_plugin_autostart::init(
                tauri_plugin_autostart::MacosLauncher::LaunchAgent,
                None,
            ));
    }

    builder
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            let dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&dir)?;
            let db = dir.join("swiftload.db");

            // `Manager::open` starts its event pump, so it must be constructed inside the
            // async runtime rather than alongside it.
            let manager = tauri::async_runtime::block_on(async { Manager::open(&db) })?;

            event_pump::spawn(app.handle().clone(), manager.clone());
            app.manage(manager);
            tray::build(app.handle())?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::Destroyed = event {
                // Pause everything rather than letting the process die mid-write: a download
                // stopped this way stays exactly as checkpointed and resumes cleanly.
                if let Some(m) = window.try_state::<Arc<Manager>>() {
                    m.shutdown();
                }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::probe_url,
            commands::add_download,
            commands::pause,
            commands::resume,
            commands::retry,
            commands::cancel,
            commands::remove,
            commands::set_priority,
            commands::start_now,
            commands::list_downloads,
            commands::get_details,
            commands::find_existing_for,
            commands::get_url_history,
            commands::reveal_full_url,
            commands::subscribe_connections,
            commands::unsubscribe_connections,
            commands::get_settings,
            commands::set_settings,
            commands::validate_replacement_url,
            commands::commit_replacement_url,
            commands::choose_folder,
            commands::autostart_enabled,
            commands::set_autostart,
            commands::reveal_in_explorer,
            commands::open_file,
        ])
        .run(tauri::generate_context!())
        .expect("failed to start SwiftLoad");
}
