use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Manager,
};

const HIDDEN_FLAG: &str = "--hidden";

/// The shell deliberately does not own the daemon process yet. Keeping this
/// seam here means a future sidecar/service implementation can be added
/// without changing tray, autostart, or window behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonLifecycle {
    External,
}

fn daemon_lifecycle_mode() -> DaemonLifecycle {
    // The daemon is currently started by the platform/user service manager.
    DaemonLifecycle::External
}

fn launch_hidden() -> bool {
    std::env::args().any(|arg| arg == HIDDEN_FLAG)
        || std::env::var_os("AGENT_SEND_START_HIDDEN").is_some()
}

/// Return the loopback API endpoint used by the shell.
///
/// The daemon is intentionally a separate process. A packaged deployment can
/// set AGENT_SEND_DAEMON_URL when it starts the daemon; local development uses
/// the daemon's documented default port.
#[tauri::command]
fn daemon_endpoint() -> String {
    std::env::var("AGENT_SEND_DAEMON_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".to_owned())
}

pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args([HIDDEN_FLAG])
                .build(),
        )
        .invoke_handler(tauri::generate_handler![daemon_endpoint])
        .setup(|app| {
            // Autostart passes --hidden; a regular launch should still open the
            // window even though the config starts it invisible for both paths.
            if !launch_hidden() {
                show_window(&app.handle());
            }
            let _daemon_lifecycle = daemon_lifecycle_mode();

            let show = MenuItem::with_id(app, "show", "Show", true, None::<&str>)?;
            let hide = MenuItem::with_id(app, "hide", "Hide", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &hide, &quit])?;

            TrayIconBuilder::new()
                .icon(tauri::image::Image::from_bytes(include_bytes!("../icons/32x32.png"))?)
                .menu(&menu)
                .tooltip("agent-send")
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        show_window(tray.app_handle());
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "show" => show_window(app),
                    "hide" => hide_window(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // Closing the small shell must not stop the daemon or transfers.
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running agent-send desktop shell");
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.hide();
    }
}
