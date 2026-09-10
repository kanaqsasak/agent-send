use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::Mutex,
};

use tauri::{
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, PhysicalPosition, PhysicalSize, Position, RunEvent, Size,
};

const HIDDEN_FLAG: &str = "--hidden";
const DAEMON_BIND: &str = "127.0.0.1:8765";

/// The packaged shell owns the bundled daemon. Keeping the child in managed
/// state makes closing the tray app stop the daemon instead of leaving an
/// orphan behind; login startup starts this same sequence before the UI.
struct DaemonProcess(Mutex<Option<Child>>);

/// Return the loopback API endpoint used by the shell.
#[tauri::command]
fn daemon_endpoint() -> String {
    std::env::var("AGENT_SEND_DAEMON_URL").unwrap_or_else(|_| "http://127.0.0.1:8765".to_owned())
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn daemon_path(resource_dir: &Path) -> Option<PathBuf> {
    let directories = [resource_dir.to_path_buf(), resource_dir.join("binaries")];
    directories
        .iter()
        .filter_map(|dir| fs::read_dir(dir).ok())
        .flat_map(|entries| entries.filter_map(Result::ok).map(|entry| entry.path()))
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| {
                    name.starts_with("agent-send-daemon-")
                        && (cfg!(windows) && name.ends_with(".exe")
                            || !cfg!(windows) && !name.ends_with(".exe"))
                })
        })
}

fn start_daemon(app: &tauri::AppHandle) {
    let resource_dir = match app.path().resource_dir() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("agent-send: cannot locate bundled daemon resources: {error}");
            return;
        }
    };
    let path = daemon_path(&resource_dir).or_else(|| {
        // In development, staged sidecars live beside this crate rather than
        // in Tauri's packaged resource directory.
        daemon_path(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("binaries")
                .as_path(),
        )
    });
    let Some(path) = path else {
        eprintln!("agent-send: bundled daemon not found; use the documented development daemon");
        return;
    };
    match Command::new(&path).args(["--bind", DAEMON_BIND]).spawn() {
        Ok(child) => {
            app.manage(DaemonProcess(Mutex::new(Some(child))));
        }
        Err(error) => {
            eprintln!("agent-send: failed to launch {}: {error}", path.display());
        }
    }
}

fn stop_daemon(app: &tauri::AppHandle) {
    if let Some(state) = app.try_state::<DaemonProcess>() {
        if let Ok(mut child) = state.0.lock() {
            if let Some(mut child) = child.take() {
                let _ = child.kill();
                let _ = child.wait();
            }
        }
    }
}

pub fn run() {
    tauri::Builder::default()
        .plugin(
            tauri_plugin_autostart::Builder::new()
                .args([HIDDEN_FLAG])
                .build(),
        )
        .invoke_handler(tauri::generate_handler![daemon_endpoint, app_version])
        .setup(|app| {
            // Start the sidecar before showing the UI. Autostart passes
            // --hidden, so login startup remains invisible but the daemon is
            // already being launched by this process.
            start_daemon(&app.handle());
            // This is a tray-only application on macOS. The window is opened
            // only by a left click on the tray icon.
            let _ = app.handle().set_dock_visibility(false);

            let about = MenuItem::with_id(app, "about", "About agent-send", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&about, &quit])?;

            TrayIconBuilder::new()
                .icon(tauri::image::Image::from_bytes(include_bytes!(
                    "../icons/32x32.png"
                ))?)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("agent-send")
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        rect,
                        ..
                    } = event
                    {
                        show_window_at(tray.app_handle(), rect);
                    }
                })
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "about" => {
                        show_window(app);
                        let _ = app.emit("agent-send://show-about", ());
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| match event {
            tauri::WindowEvent::Focused(false) if !window.is_focused().unwrap_or(false) => {
                // A popover is transient: losing focus returns it to the tray.
                // Re-check focus so a delayed blur from a tray click cannot hide
                // a popover that has already been reopened and focused.
                let _ = window.hide();
            }
            tauri::WindowEvent::CloseRequested { api, .. } => {
                // Closing the small shell must not stop the daemon or transfers.
                api.prevent_close();
                let _ = window.hide();
            }
            _ => {}
        })
        .build(tauri::generate_context!())
        .expect("error while building agent-send desktop shell")
        .run(|app, event| {
            if matches!(event, RunEvent::Exit) {
                stop_daemon(app);
            }
        });
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn show_window_at(app: &tauri::AppHandle, rect: tauri::Rect) {
    if let Some(window) = app.get_webview_window("main") {
        // Position the popover below the tray icon. This keeps the interaction
        // anchored to the tray on menu-bar systems; the UI remains usable on
        // platforms where tray geometry is unavailable.
        let (position, tray_size) = match (rect.position, rect.size) {
            (Position::Physical(position), Size::Physical(size)) => (position, size),
            _ => {
                show_window(app);
                return;
            }
        };
        let popover_size = window
            .outer_size()
            .unwrap_or_else(|_| PhysicalSize::new(396, 540));
        let width = i32::try_from(popover_size.width).unwrap_or(396);
        let height = i32::try_from(popover_size.height).unwrap_or(540);
        let tray_width = i32::try_from(tray_size.width).unwrap_or_default();
        let tray_height = i32::try_from(tray_size.height).unwrap_or_default();
        let mut x = position.x + tray_width / 2 - width / 2;
        let mut y = position.y + tray_height + 8;

        // A taskbar can be at any edge. Keep the compact popover within the
        // monitor work area, preferring the side opposite the tray if needed.
        if let Ok(Some(monitor)) = app.monitor_from_point(position.x as f64, position.y as f64) {
            let work_area = monitor.work_area();
            let right =
                work_area.position.x + i32::try_from(work_area.size.width).unwrap_or(i32::MAX);
            let bottom =
                work_area.position.y + i32::try_from(work_area.size.height).unwrap_or(i32::MAX);
            x = x.clamp(work_area.position.x, right.saturating_sub(width));
            if y.saturating_add(height) > bottom {
                y = position.y.saturating_sub(height + 8);
            }
            y = y.clamp(work_area.position.y, bottom.saturating_sub(height));
        }
        let _ = window.set_position(PhysicalPosition::new(x, y));
        let _ = window.show();
        let _ = window.set_focus();
    }
}
