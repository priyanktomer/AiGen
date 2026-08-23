//! System tray.
//!
//! A download manager spends most of its life not being looked at. The tray is where it lives
//! while that is true: it shows what is moving without a window, and offers the two actions
//! worth reaching for without one — pause everything, or bring the window back.
//!
//! **Closing the window quits.** Hiding to the tray on close is the convention, and it is also
//! how an app ends up running for weeks without its owner realising. The rest of this product
//! refuses to do things the user did not ask for — nothing is auto-opened, nothing is
//! auto-resumed — and silently continuing to run is the same class of decision. Closing the
//! window pauses every download cleanly and exits; the partials are intact and resume next
//! launch. Close-to-tray belongs behind an explicit setting, which is roadmap.

use std::sync::Arc;
use swiftload_core::manager::Manager;
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{TrayIconBuilder, TrayIconEvent},
    AppHandle, Manager as _, Runtime,
};

pub fn build<R: Runtime>(app: &AppHandle<R>) -> tauri::Result<()> {
    let show = MenuItem::with_id(app, "show", "Show SwiftLoad", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause_all", "Pause all downloads", true, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume_all", "Resume all", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show, &pause, &resume, &sep, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip("SwiftLoad")
        .on_menu_event(on_menu)
        .on_tray_icon_event(|tray, event| {
            // Left click is "show me the window" everywhere else on the desktop.
            if let TrayIconEvent::DoubleClick { .. } = event {
                reveal(tray.app_handle());
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

fn on_menu<R: Runtime>(app: &AppHandle<R>, event: tauri::menu::MenuEvent) {
    match event.id.as_ref() {
        "show" => reveal(app),
        "pause_all" => with_manager(app, |m| {
            for d in m.list(None).unwrap_or_default() {
                if d.status == swiftload_core::store::models::DownloadStatus::Active {
                    let _ = m.pause(&d.id);
                }
            }
        }),
        "resume_all" => with_manager(app, |m| {
            for d in m.list(None).unwrap_or_default() {
                if d.status == swiftload_core::store::models::DownloadStatus::Paused {
                    let _ = m.clone().resume(&d.id);
                }
            }
        }),
        "quit" => {
            // Stop cleanly rather than being killed mid-write: every download is paused and
            // checkpointed on the way out.
            with_manager(app, |m| m.shutdown());
            app.exit(0);
        }
        _ => {}
    }
}

fn with_manager<R: Runtime>(app: &AppHandle<R>, f: impl FnOnce(&Arc<Manager>)) {
    if let Some(m) = app.try_state::<Arc<Manager>>() {
        f(&m);
    }
}

fn reveal<R: Runtime>(app: &AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}
