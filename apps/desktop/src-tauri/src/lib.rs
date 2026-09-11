use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
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
    let entries = fs::read_dir(resource_dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                name.starts_with("agent-send-daemon-")
                    && (cfg!(windows) && name.ends_with(".exe")
                        || !cfg!(windows) && !name.ends_with(".exe"))
            })
        {
            return Some(path);
        }
        if path.is_dir() {
            if let Some(path) = daemon_path(&path) {
                return Some(path);
            }
        }
    }
    None
}

fn startup_log(message: impl AsRef<str>) {
    let path = std::env::temp_dir().join("agent-send-desktop-startup.log");
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write;
        let _ = writeln!(file, "{}", message.as_ref());
    }
}

fn start_daemon(app: &tauri::AppHandle) {
    startup_log("starting daemon");
    let resource_dir = match app.path().resource_dir() {
        Ok(path) => {
            startup_log(format!("resource_dir={}", path.display()));
            path
        }
        Err(error) => {
            startup_log(format!("resource_dir_error={error}"));
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
        startup_log("daemon_sidecar=not_found");
        eprintln!("agent-send: bundled daemon not found; use the documented development daemon");
        return;
    };
    startup_log(format!("daemon_sidecar={}", path.display()));
    let data_dir = app.path().app_data_dir().ok();
    let identity_path = data_dir.as_ref().map(|dir| dir.join("identity.json"));
    if let Some(dir) = &data_dir {
        if let Err(error) = fs::create_dir_all(dir) {
            startup_log(format!("data_dir_error={} error={error}", dir.display()));
        } else {
            startup_log(format!("data_dir={}", dir.display()));
        }
    } else {
        startup_log("data_dir=unavailable");
    }
    let log = data_dir.as_ref().and_then(|dir| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("daemon.log"))
            .ok()
    });
    let mut command = Command::new(&path);
    command.args(["--bind", DAEMON_BIND]);
    if let Some(identity_path) = &identity_path {
        command.args(["--identity-path", &identity_path.to_string_lossy()]);
    }
    if let Some(log) = log {
        if let Ok(stderr) = log.try_clone() {
            command.stdout(Stdio::from(log)).stderr(Stdio::from(stderr));
        }
    }
    match command.spawn() {
        Ok(child) => {
            startup_log("daemon_spawn=ok");
            app.manage(DaemonProcess(Mutex::new(Some(child))));
        }
        Err(error) => {
            startup_log(format!("daemon_spawn_error={error}"));
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

#[cfg(target_os = "macos")]
fn hide_dock(app: &tauri::AppHandle) {
    let _ = app.set_dock_visibility(false);
}

#[cfg(not(target_os = "macos"))]
fn hide_dock(_app: &tauri::AppHandle) {}

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
            hide_dock(&app.handle());

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
