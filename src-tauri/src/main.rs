// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod detector;
mod player;
mod ports;
mod settings;

use detector::{accelerometer_available, DetectorHandle};
use ports::{
    platform_label, port_monitor_mode_label, probe_port_capabilities, update_rule_bundle,
    PortMonitorHandle,
};
use player::{BundleInfo, PlayerHandle, SoundInfo};
use settings::Settings;

use std::sync::{Arc, Mutex};
use std::time::Instant;
use serde::Serialize;
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    Emitter, Manager,
};

struct AppState {
    settings: Arc<Mutex<Settings>>,
    detector: DetectorHandle,
    _port_monitor: PortMonitorHandle,
    player: Arc<PlayerHandle>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DetectorCapabilities {
    accelerometer_available: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PortCapabilitiesResponse {
    ports: Vec<ports::PortCapability>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeDiagnostics {
    platform: String,
    port_monitor_mode: String,
    accelerometer_available: bool,
    ports: Vec<ports::PortCapability>,
}

fn show_settings_window(app: &tauri::AppHandle) {
    if let Some(win) = app.get_webview_window("settings") {
        let _ = win.unminimize();
        let _ = win.show();
        let _ = win.set_focus();
    }
}

fn sanitize_settings_for_available_bundles(settings: &mut Settings, player: &PlayerHandle) {
    let fallback = player.first_playable_bundle().unwrap_or_default();

    if !player.bundle_has_sounds(&settings.bundle) {
        settings.bundle = fallback.clone();
    }

    for rule in settings.port_rules.iter_mut() {
        if !player.bundle_has_sounds(&rule.bundle) {
            rule.bundle = fallback.clone();
        }
    }

    if settings.enabled && settings.bundle.is_empty() {
        settings.enabled = false;
    }
}

// ── Settings Commands ───────────────────────────────────────────────

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

#[tauri::command]
fn save_settings(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    new_settings: Settings,
) -> Result<(), String> {
    let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    *s = new_settings;
    s.validate();
    sanitize_settings_for_available_bundles(&mut s, &state.player);
    s.save(&app_handle);
    let sc = s.clone();
    drop(s);

    if sc.enabled {
        state
            .detector
            .start(sc.detection_mode, sc.sensitivity, sc.cooldown_ms);
    } else {
        state.detector.stop();
    }

    app_handle.emit("settings-changed", &sc).ok();
    Ok(())
}

#[tauri::command]
fn toggle_enabled(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
) -> bool {
    let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    s.enabled = !s.enabled;
    sanitize_settings_for_available_bundles(&mut s, &state.player);
    s.save(&app_handle);
    let sc = s.clone();
    drop(s);

    if sc.enabled {
        state
            .detector
            .start(sc.detection_mode, sc.sensitivity, sc.cooldown_ms);
    } else {
        state.detector.stop();
    }

    app_handle.emit("settings-changed", &sc).ok();
    sc.enabled
}

#[tauri::command]
fn test_sound(state: tauri::State<'_, AppState>, volume: f32, bundle: String) {
    state.player.play(&bundle, volume, 1.0);
}

#[tauri::command]
fn get_detector_capabilities() -> DetectorCapabilities {
    DetectorCapabilities {
        accelerometer_available: accelerometer_available(),
    }
}

#[tauri::command]
fn get_port_capabilities() -> PortCapabilitiesResponse {
    PortCapabilitiesResponse {
        ports: probe_port_capabilities(),
    }
}

#[tauri::command]
fn get_runtime_diagnostics() -> RuntimeDiagnostics {
    RuntimeDiagnostics {
        platform: platform_label().to_string(),
        port_monitor_mode: port_monitor_mode_label().to_string(),
        accelerometer_available: accelerometer_available(),
        ports: probe_port_capabilities(),
    }
}

// ── Bundle Commands ─────────────────────────────────────────────────

#[tauri::command]
fn list_bundles(state: tauri::State<'_, AppState>) -> Vec<BundleInfo> {
    state.player.list_bundles()
}

#[tauri::command]
fn create_bundle(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    state.player.create_bundle(&name)
}

#[tauri::command]
fn delete_bundle(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    name: String,
) -> Result<(), String> {
    state.player.delete_bundle(&name)?;

    let available = state.player.list_bundles();
    let fallback = available
        .iter()
        .find(|bundle| bundle.count > 0)
        .map(|bundle| bundle.name.clone())
        .unwrap_or_default();

    let mut settings = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    if settings.bundle == name {
        settings.bundle = fallback.clone();
    }
    for rule in settings.port_rules.iter_mut() {
        if rule.bundle == name {
            rule.bundle = fallback.clone();
        }
    }
    settings.validate();
    settings.save(&app_handle);
    let snapshot = settings.clone();
    drop(settings);
    app_handle.emit("settings-changed", &snapshot).ok();

    Ok(())
}

#[tauri::command]
fn rename_bundle(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    old_name: String,
    new_name: String,
) -> Result<(), String> {
    state.player.rename_bundle(&old_name, &new_name)?;

    let mut settings = state.settings.lock().unwrap_or_else(|e| e.into_inner());
    if settings.bundle == old_name {
      settings.bundle = new_name.clone();
    }
    update_rule_bundle(&mut settings.port_rules, &old_name, &new_name);
    settings.validate();
    settings.save(&app_handle);
    let snapshot = settings.clone();
    drop(settings);
    app_handle.emit("settings-changed", &snapshot).ok();

    Ok(())
}

// ── Sound File Commands ─────────────────────────────────────────────

#[tauri::command]
fn list_bundle_sounds(state: tauri::State<'_, AppState>, bundle: String) -> Vec<SoundInfo> {
    state.player.list_bundle_sounds(&bundle)
}

#[tauri::command]
fn import_sounds(
    state: tauri::State<'_, AppState>,
    bundle: String,
    paths: Vec<String>,
) -> Result<Vec<String>, String> {
    state.player.import_sounds(&bundle, &paths)
}

#[tauri::command]
fn remove_sound(
    state: tauri::State<'_, AppState>,
    bundle: String,
    filename: String,
) -> Result<(), String> {
    state.player.remove_sound(&bundle, &filename)
}

// ── App Setup ───────────────────────────────────────────────────────

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // Another instance was launched — show settings window
            show_settings_window(app);
        }))
        .setup(|app| {
            #[cfg(target_os = "macos")]
            {
                let _ = app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
                let _ = app.handle().set_dock_visibility(false);
            }

            let settings = Settings::load(&app.handle());

            let sounds_dir = app
                .path()
                .app_data_dir()
                .expect("failed to get app data dir")
                .join("sounds");

            let player = Arc::new(PlayerHandle::new(sounds_dir));
            let mut settings = settings;
            settings.validate();
            sanitize_settings_for_available_bundles(&mut settings, &player);
            settings.save(&app.handle());
            let shared_settings = Arc::new(Mutex::new(settings.clone()));

            let player_ref = player.clone();
            let settings_ref = shared_settings.clone();
            let detector = DetectorHandle::spawn(move |intensity| {
                let s = settings_ref.lock().unwrap_or_else(|e| e.into_inner());
                player_ref.play(&s.bundle, s.volume, intensity);
            });

            let player_ref = player.clone();
            let settings_ref = shared_settings.clone();
            let last_port_trigger = Arc::new(Mutex::new(Instant::now() - std::time::Duration::from_secs(10)));
            let port_monitor = PortMonitorHandle::spawn(move |kind, connected| {
                let s = settings_ref.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(rule) = s.port_rules.iter().find(|rule| rule.kind == kind) {
                    let should_fire = if connected {
                        rule.on_connect
                    } else {
                        rule.on_disconnect
                    };

                    if should_fire && !rule.bundle.is_empty() {
                        // Port events have a cooldown to prevent rapid-fire triggers
                        // from device negotiation or charger state toggling.
                        let mut lt = last_port_trigger.lock().unwrap_or_else(|e| e.into_inner());
                        let elapsed = Instant::now().duration_since(*lt).as_millis() as u64;
                        let port_cooldown = s.cooldown_ms.max(1500);
                        if elapsed >= port_cooldown {
                            *lt = Instant::now();
                            drop(lt);
                            player_ref.play(&rule.bundle, s.volume, 0.9);
                        }
                    }
                }
            });

            if settings.enabled {
                detector.start(
                    settings.detection_mode,
                    settings.sensitivity,
                    settings.cooldown_ms,
                );
            }

            app.manage(AppState {
                settings: shared_settings,
                detector,
                _port_monitor: port_monitor,
                player,
            });

            // ── System Tray ─────────────────────────────────────
            let toggle_label = if settings.enabled { "Pause" } else { "Resume" };
            let toggle = MenuItemBuilder::with_id("toggle", toggle_label).build(app)?;
            let settings_item = MenuItemBuilder::with_id("settings", "Settings").build(app)?;
            let test = MenuItemBuilder::with_id("test", "Test Sound").build(app)?;
            let quit = MenuItemBuilder::with_id("quit", "Quit").build(app)?;

            let menu = MenuBuilder::new(app)
                .item(&toggle)
                .separator()
                .item(&test)
                .item(&settings_item)
                .separator()
                .item(&quit)
                .build()?;

            let _tray = TrayIconBuilder::new()
                .menu(&menu)
                .show_menu_on_left_click(false)
                .tooltip("The Moaning Guy")
                .icon(app.default_window_icon().unwrap().clone())
                .on_tray_icon_event(|tray, event| {
                    if let tauri::tray::TrayIconEvent::Click { button: tauri::tray::MouseButton::Left, .. } = event {
                        show_settings_window(&tray.app_handle());
                    }
                })
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "toggle" => {
                        let state: tauri::State<'_, AppState> = app.state();
                        let mut s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
                        s.enabled = !s.enabled;
                        s.validate();
                        sanitize_settings_for_available_bundles(&mut s, &state.player);
                        s.save(app);
                        let enabled = s.enabled;

                        toggle
                            .set_text(if enabled { "Pause" } else { "Resume" })
                            .ok();

                        if enabled {
                            state
                                .detector
                                .start(s.detection_mode, s.sensitivity, s.cooldown_ms);
                        } else {
                            state.detector.stop();
                        }

                        let sc = s.clone();
                        drop(s);
                        app.emit("settings-changed", &sc).ok();
                    }
                    "settings" => {
                        show_settings_window(app);
                    }
                    "test" => {
                        let state: tauri::State<'_, AppState> = app.state();
                        let s = state.settings.lock().unwrap_or_else(|e| e.into_inner());
                        state.player.play(&s.bundle, s.volume, 0.8);
                    }
                    "quit" => {
                        app.exit(0);
                    }
                    _ => {}
                })
                .build(app)?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_settings,
            get_detector_capabilities,
            get_port_capabilities,
            get_runtime_diagnostics,
            save_settings,
            toggle_enabled,
            test_sound,
            list_bundles,
            create_bundle,
            delete_bundle,
            rename_bundle,
            list_bundle_sounds,
            import_sounds,
            remove_sound,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if window.label() == "settings" {
                    api.prevent_close();
                    window.hide().ok();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
