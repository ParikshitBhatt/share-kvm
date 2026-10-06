#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;
mod settings;

use engine::{Engine, Snapshot};
use serde::Serialize;
use settings::Settings;
use sharekvm_core::discovery::{self, Found};
use sharekvm_core::trust::{to_hex, Role, TrustStore};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Listener, Manager, RunEvent, State, WindowEvent, Wry};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};

/// Passed by the login item, so ShareKVM starts quietly in the menu bar / tray.
const HIDDEN_ARG: &str = "--hidden";

struct AppState {
    engine: Mutex<Engine>,
    settings: Mutex<Settings>,
    settings_path: PathBuf,
    /// Engine identity and remembered pairings.
    data_dir: PathBuf,
    /// ShareKVM servers currently visible on the LAN, by mDNS name.
    discovered: Mutex<HashMap<String, Found>>,
    /// Tray menu items that reflect live status.
    tray: Mutex<Option<TrayItems>>,
    /// Engine exits in a row without reaching a working state (for retry backoff).
    failures: Mutex<u32>,
}

struct TrayItems {
    status: MenuItem<Wry>,
    toggle: CheckMenuItem<Wry>,
}

/// A remembered computer as the UI sees it (no secret).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairedView {
    id: String,
    name: String,
    /// "client" = a computer this one controls; "server" = one that controls this one.
    role: Role,
    address: Option<String>,
    paired_at: u64,
    last_seen: u64,
}

#[tauri::command]
fn get_settings(state: State<AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

/// Saves settings and applies them: the engine restarts if anything it uses changed.
#[tauri::command]
fn save_settings(app: AppHandle, state: State<AppState>, settings: Settings) -> Result<(), String> {
    let old = std::mem::replace(&mut *state.settings.lock().unwrap(), settings.clone());
    settings.save(&state.settings_path)?;
    if old.launch_at_login != settings.launch_at_login {
        apply_login_item(&app, settings.launch_at_login);
    }
    if old.enabled != settings.enabled || (settings.enabled && old.engine_differs(&settings)) {
        *state.failures.lock().unwrap() = 0;
        // Restarting waits for the old engine to exit; keep that off the UI thread.
        let app = app.clone();
        std::thread::spawn(move || sync_engine(&app));
    }
    Ok(())
}

/// Starts (or restarts) the engine if sharing is on, stops it if off.
fn sync_engine(app: &AppHandle) {
    let state = app.state::<AppState>();
    let settings = state.settings.lock().unwrap().clone();
    let mut engine = state.engine.lock().unwrap();
    if settings.enabled {
        if let Err(e) = engine.start(app, &settings, &state.data_dir) {
            engine.fail(app, &settings.mode, e);
        }
    } else {
        engine.stop(app);
    }
}

fn set_enabled(app: &AppHandle, enabled: bool) {
    let state = app.state::<AppState>();
    let mut settings = state.settings.lock().unwrap().clone();
    if settings.enabled != enabled {
        settings.enabled = enabled;
        let _ = save_settings(app.clone(), state, settings);
        let _ = app.emit("settings-changed", ());
    }
}

fn apply_login_item(app: &AppHandle, on: bool) {
    let launcher = app.autolaunch();
    if launcher.is_enabled().unwrap_or(false) == on {
        return;
    }
    let result = if on { launcher.enable() } else { launcher.disable() };
    if let Err(e) = result {
        eprintln!("could not {} launch at login: {e}", if on { "enable" } else { "disable" });
    }
}

#[tauri::command]
fn send_files(state: State<AppState>, paths: Vec<String>) -> Result<(), String> {
    if paths.is_empty() {
        return Ok(());
    }
    state.engine.lock().unwrap().command(&serde_json::json!({ "cmd": "send_files", "paths": paths }))
}

#[tauri::command]
fn cancel_transfer(state: State<AppState>, id: u64) -> Result<(), String> {
    state.engine.lock().unwrap().command(&serde_json::json!({ "cmd": "cancel_transfer", "id": id }))
}

/// Shows a received file/folder in Finder or Explorer (or opens the downloads folder).
#[tauri::command]
fn reveal(path: Option<String>) -> Result<(), String> {
    let dir = sharekvm_core::files::default_download_dir();
    let target = path.map(PathBuf::from).unwrap_or_else(|| {
        let _ = std::fs::create_dir_all(&dir);
        dir.clone()
    });
    // Only reveal things inside the downloads folder.
    if !target.starts_with(&dir) {
        return Err("Can only show received files.".into());
    }
    let select = target != dir;
    let mut cmd = if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        if select {
            c.arg("-R");
        }
        c.arg(&target);
        c
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("explorer");
        if select {
            c.arg(format!("/select,{}", target.display()));
        } else {
            c.arg(&target);
        }
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(if select { target.parent().unwrap_or(&dir) } else { &target });
        c
    };
    cmd.spawn().map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
fn list_discovered(state: State<AppState>) -> Vec<Found> {
    sorted(&state.discovered.lock().unwrap())
}

fn sorted(map: &HashMap<String, Found>) -> Vec<Found> {
    let mut v: Vec<Found> = map.values().cloned().collect();
    v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    v
}

/// Watches the LAN for ShareKVM servers for as long as the app runs.
fn start_discovery(app: &AppHandle, own_id: String) {
    let handle = app.clone();
    let result = discovery::browse(move |ev| {
        let state = handle.state::<AppState>();
        let mut map = state.discovered.lock().unwrap();
        let changed = match ev {
            discovery::Event::Found(f) if f.id != own_id => map.insert(f.fullname.clone(), f.clone()).as_ref() != Some(&f),
            discovery::Event::Found(_) => false,
            discovery::Event::Lost { fullname } => map.remove(&fullname).is_some(),
        };
        if changed {
            let _ = handle.emit("discovered", sorted(&map));
        }
    });
    match result {
        // The browser lives as long as the app; leaking the daemon handle keeps it running.
        Ok(daemon) => std::mem::forget(daemon),
        Err(e) => eprintln!("LAN discovery unavailable: {e:#}"),
    }
}

#[tauri::command]
fn list_paired(state: State<AppState>) -> Result<Vec<PairedView>, String> {
    let store = TrustStore::open(&state.data_dir).map_err(|e| e.to_string())?;
    let mut peers: Vec<PairedView> = store
        .peers()
        .into_iter()
        .map(|p| PairedView { id: p.id, name: p.name, role: p.role, address: p.address, paired_at: p.paired_at, last_seen: p.last_seen })
        .collect();
    peers.sort_by(|a, b| b.last_seen.cmp(&a.last_seen));
    Ok(peers)
}

/// Forgets a computer. Forgetting a computer we control also rotates the pairing
/// code, so it can't simply re-pair with the old one. A running engine restarts
/// so any live session with that computer ends now.
#[tauri::command]
fn forget_paired(app: AppHandle, state: State<AppState>, id: String, role: Role) -> Result<(), String> {
    let store = TrustStore::open(&state.data_dir).map_err(|e| e.to_string())?;
    store.forget(role, &id).map_err(|e| e.to_string())?;
    let settings = {
        let mut s = state.settings.lock().unwrap();
        if role == Role::Client {
            s.code = settings::new_code();
            s.save(&state.settings_path)?;
        }
        s.clone()
    };
    let _ = app.emit("settings-changed", ());
    let _ = app.emit("paired-changed", ());
    let mut engine = state.engine.lock().unwrap();
    if engine.snapshot().running {
        engine.start(&app, &settings, &state.data_dir)?;
    }
    Ok(())
}

#[tauri::command]
fn engine_status(state: State<AppState>) -> Snapshot {
    state.engine.lock().unwrap().snapshot()
}

#[tauri::command]
fn new_code() -> String {
    settings::new_code()
}

/// This machine's LAN address, so the user knows what to type on the other computer.
#[tauri::command]
fn local_address() -> Option<String> {
    // Connecting a UDP socket sends nothing; it only picks the outgoing interface.
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:9").ok()?;
    Some(sock.local_addr().ok()?.ip().to_string())
}

/// This computer's monitor arrangement, for the layout picture.
#[tauri::command]
fn screens() -> sharekvm_core::screens::Layout {
    sharekvm_core::screens::Layout::detect()
}

#[tauri::command]
fn protocol_version() -> u32 {
    sharekvm_core::protocol::PROTOCOL_VERSION
}

#[tauri::command]
fn platform() -> &'static str {
    std::env::consts::OS
}

/// Opens the settings window. On macOS the Dock icon appears only while it's open.
fn show_main(app: &AppHandle) {
    #[cfg(target_os = "macos")]
    let _ = app.set_activation_policy(tauri::ActivationPolicy::Regular);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

fn hide_main(window: &tauri::Window) {
    let _ = window.hide();
    #[cfg(target_os = "macos")]
    let _ = window.app_handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
}

fn window_visible(app: &AppHandle) -> bool {
    app.get_webview_window("main").and_then(|w| w.is_visible().ok()).unwrap_or(false)
}

/// One-line status for the tray menu and tooltip.
fn status_line(v: &serde_json::Value) -> String {
    let peer = v["peer"].as_str().unwrap_or("the other computer");
    let controlling = v["controlling"].as_bool().unwrap_or(false);
    match v["state"].as_str().unwrap_or("stopped") {
        "waiting" => "Waiting for the other computer".into(),
        "connecting" => "Connecting…".into(),
        "connected" => format!("Connected to {peer}"),
        "active" if controlling => format!("Controlling {peer}"),
        "active" => format!("Being controlled by {peer}"),
        "error" => "Needs attention: open Settings".into(),
        _ => "Sharing is off".into(),
    }
}

fn build_tray(app: &tauri::App) -> tauri::Result<()> {
    let enabled = app.state::<AppState>().settings.lock().unwrap().enabled;
    let status = MenuItem::with_id(app, "status", "Starting…", false, None::<&str>)?;
    let toggle = CheckMenuItem::with_id(app, "toggle", "Sharing", true, enabled, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "Settings…", true, None::<&str>)?;
    let quit = MenuItem::with_id(app, "quit", "Quit ShareKVM", true, None::<&str>)?;
    let sep = || PredefinedMenuItem::separator(app);
    let menu = Menu::with_items(app, &[&status, &sep()?, &toggle, &show, &sep()?, &quit])?;

    let mut tray = TrayIconBuilder::with_id("main")
        .tooltip("ShareKVM")
        .menu(&menu)
        // macOS: click opens the menu. Windows: left-click opens settings, right-click the menu.
        .show_menu_on_left_click(cfg!(target_os = "macos"))
        .on_menu_event(|app, ev| match ev.id.as_ref() {
            "toggle" => {
                let on = !app.state::<AppState>().settings.lock().unwrap().enabled;
                set_enabled(app, on);
            }
            "show" => show_main(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, ev| {
            if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = ev {
                if !cfg!(target_os = "macos") {
                    show_main(tray.app_handle());
                }
            }
        });
    // A monochrome template icon suits the macOS menu bar; Windows gets the colour icon.
    if cfg!(target_os = "macos") {
        tray = tray.icon(tauri::image::Image::from_bytes(include_bytes!("../icons/tray.png"))?).icon_as_template(true);
    } else if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    tray.build(app)?;
    *app.state::<AppState>().tray.lock().unwrap() = Some(TrayItems { status, toggle });
    Ok(())
}

/// Keeps the tray in step with the engine and the settings.
fn update_tray(app: &AppHandle, snapshot: &serde_json::Value) {
    let line = status_line(snapshot);
    let state = app.state::<AppState>();
    if let Some(t) = &*state.tray.lock().unwrap() {
        let _ = t.status.set_text(&line);
        let _ = t.toggle.set_checked(state.settings.lock().unwrap().enabled);
    }
    if let Some(tray) = app.tray_by_id("main") {
        let _ = tray.set_tooltip(Some(format!("ShareKVM: {line}")));
    }
}

/// The engine stopped on its own. Retry with backoff, unless the user needs to act
/// (wrong code, missing setting), in which case open the window and wait.
fn on_engine_exit(app: &AppHandle, generation: u64, needs_user: bool) {
    let state = app.state::<AppState>();
    if !state.settings.lock().unwrap().enabled {
        return;
    }
    if needs_user {
        show_main(app);
        return;
    }
    let failures = {
        let mut f = state.failures.lock().unwrap();
        *f += 1;
        *f
    };
    if failures == 1 && !window_visible(app) {
        show_main(app); // say once what's wrong (e.g. macOS permissions), then keep retrying quietly
    }
    let delay = Duration::from_secs((5u64 << (failures - 1).min(4)).min(60));
    let app = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let state = app.state::<AppState>();
        let stale = {
            let engine = state.engine.lock().unwrap();
            engine.generation() != generation || engine.snapshot().running
        };
        if !stale && state.settings.lock().unwrap().enabled {
            sync_engine(&app);
        }
    });
}

fn main() {
    let app = tauri::Builder::default()
        // Must be first: opening ShareKVM again just shows the running copy's settings.
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| show_main(app)))
        .plugin(tauri_plugin_autostart::init(MacosLauncher::LaunchAgent, Some(vec![HIDDEN_ARG])))
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let settings_path = config_dir.join("settings.json");
            let first_run = !settings_path.exists();
            let settings = Settings::load(&settings_path);
            if first_run {
                let _ = settings.save(&settings_path);
            }
            let launch_at_login = settings.launch_at_login;
            app.manage(AppState {
                engine: Mutex::new(Engine::default()),
                settings: Mutex::new(settings),
                settings_path,
                data_dir: config_dir.clone(),
                discovered: Mutex::new(HashMap::new()),
                tray: Mutex::new(None),
                failures: Mutex::new(0),
            });
            apply_login_item(app.handle(), launch_at_login);

            // Started at login: stay in the menu bar. Opened by the user (or first run): show settings.
            let hidden = !first_run && std::env::args().any(|a| a == HIDDEN_ARG);
            if hidden {
                #[cfg(target_os = "macos")]
                app.set_activation_policy(tauri::ActivationPolicy::Accessory);
            } else {
                show_main(app.handle());
            }

            build_tray(app)?;
            let handle = app.handle().clone();
            app.listen("engine-status", move |event| {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(event.payload()) else { return };
                if matches!(v["state"].as_str(), Some("waiting" | "connected" | "active")) {
                    *handle.state::<AppState>().failures.lock().unwrap() = 0;
                }
                update_tray(&handle, &v);
            });
            let handle = app.handle().clone();
            app.listen("engine-exited", move |event| {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(event.payload()) else { return };
                on_engine_exit(&handle, v["generation"].as_u64().unwrap_or(0), v["needsUser"].as_bool().unwrap_or(false));
            });

            // Our own engine also advertises; don't list this computer.
            let own_id = TrustStore::open(&config_dir).map(|t| to_hex(&t.device_id())).unwrap_or_default();
            start_discovery(app.handle(), own_id);

            // Remember where the server is and who it is, so it can be found again later.
            let handle = app.handle().clone();
            app.listen("client-connected", move |event| {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(event.payload()) else { return };
                let addr = v["addr"].as_str().unwrap_or_default();
                let addr = addr.strip_suffix(&format!(":{}", settings::DEFAULT_PORT)).unwrap_or(addr).to_string();
                let id = v["server_id"].as_str().unwrap_or_default().to_string();
                let state = handle.state::<AppState>();
                let mut s = state.settings.lock().unwrap();
                if s.server_address != addr || s.server_id != id {
                    s.server_address = addr;
                    s.server_id = id;
                    let _ = s.save(&state.settings_path);
                    let _ = handle.emit("settings-changed", ());
                }
            });

            // Once a client has paired, its saved key replaces the typed code.
            let handle = app.handle().clone();
            app.listen("client-paired", move |_| {
                let state = handle.state::<AppState>();
                let mut s = state.settings.lock().unwrap();
                if !s.client_code.is_empty() {
                    s.client_code.clear();
                    let _ = s.save(&state.settings_path);
                    let _ = handle.emit("settings-changed", ());
                }
            });

            // Sharing runs whenever ShareKVM runs (unless switched off).
            let handle = app.handle().clone();
            std::thread::spawn(move || sync_engine(&handle));
            update_tray(app.handle(), &serde_json::json!({ "state": "stopped" }));
            Ok(())
        })
        // Closing the window keeps sharing running in the menu bar / tray.
        .on_window_event(|window, event| {
            if let WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                hide_main(window);
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            save_settings,
            engine_status,
            list_paired,
            forget_paired,
            list_discovered,
            protocol_version,
            screens,
            send_files,
            cancel_transfer,
            reveal,
            new_code,
            local_address,
            platform,
        ])
        .build(tauri::generate_context!())
        .expect("error while building ShareKVM");

    app.run(|app, event| match event {
        RunEvent::Exit => {
            let state = app.state::<AppState>();
            let mut engine = state.engine.lock().unwrap();
            engine.stop(app);
        }
        #[cfg(target_os = "macos")]
        RunEvent::Reopen { .. } => show_main(app),
        _ => {}
    });
}
