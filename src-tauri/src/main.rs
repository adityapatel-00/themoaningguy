// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod detector;
mod player;
mod settings;

use detector::DetectorHandle;
use player::{BundleInfo, PlayerHandle, SoundInfo};
use settings::Settings;

use std::sync::{Arc, Mutex};
use tauri::{
    menu::{MenuBuilder, MenuItemBuilder},
    tray::TrayIconBuilder,
    Emitter, Manager,
};

struct AppState {
    settings: Arc<Mutex<Settings>>,
    detector: DetectorHandle,
    player: Arc<PlayerHandle>,
}

// ── Settings Commands ───────────────────────────────────────────────

#[tauri::command]
fn get_settings(state: tauri::State<'_, AppState>) -> Settings {
    state.settings.lock().unwrap().clone()
}

#[tauri::command]
fn save_settings(
    state: tauri::State<'_, AppState>,
    app_handle: tauri::AppHandle,
    new_settings: Settings,
) -> Result<(), String> {
    let mut s = state.settings.lock().unwrap();
    *s = new_settings;
    s.validate();
    s.save(&app_handle);
    let sc = s.clone();
    drop(s);

    if sc.enabled {
        state.detector.start(sc.sensitivity, sc.cooldown_ms);
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
    let mut s = state.settings.lock().unwrap();
    s.enabled = !s.enabled;
    s.save(&app_handle);
    let sc = s.clone();
    drop(s);

    if sc.enabled {
        state.detector.start(sc.sensitivity, sc.cooldown_ms);
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
fn delete_bundle(state: tauri::State<'_, AppState>, name: String) -> Result<(), String> {
    state.player.delete_bundle(&name)
}

#[tauri::command]
fn rename_bundle(
    state: tauri::State<'_, AppState>,
    old_name: String,
    new_name: String,
) -> Result<(), String> {
    state.player.rename_bundle(&old_name, &new_name)
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
        .setup(|app| {
            let settings = Settings::load(&app.handle());

            let sounds_dir = app
                .path()
                .app_data_dir()
                .expect("failed to get app data dir")
                .join("sounds");

            let player = Arc::new(PlayerHandle::new(sounds_dir));
            let shared_settings = Arc::new(Mutex::new(settings.clone()));

            let player_ref = player.clone();
            let settings_ref = shared_settings.clone();
            let detector = DetectorHandle::spawn(move |intensity| {
                let s = settings_ref.lock().unwrap();
                player_ref.play(&s.bundle, s.volume, intensity);
            });

            if settings.enabled {
                detector.start(settings.sensitivity, settings.cooldown_ms);
            }

            app.manage(AppState {
                settings: shared_settings,
                detector,
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
                .tooltip("The Moaning Guy")
                .icon(app.default_window_icon().unwrap().clone())
                .on_menu_event(move |app, event| match event.id().as_ref() {
                    "toggle" => {
                        let state: tauri::State<'_, AppState> = app.state();
                        let mut s = state.settings.lock().unwrap();
                        s.enabled = !s.enabled;
                        s.save(app);
                        let enabled = s.enabled;

                        toggle
                            .set_text(if enabled { "Pause" } else { "Resume" })
                            .ok();

                        if enabled {
                            state.detector.start(s.sensitivity, s.cooldown_ms);
                        } else {
                            state.detector.stop();
                        }

                        let sc = s.clone();
                        drop(s);
                        app.emit("settings-changed", &sc).ok();
                    }
                    "settings" => {
                        if let Some(win) = app.get_webview_window("settings") {
                            win.show().ok();
                            win.set_focus().ok();
                        }
                    }
                    "test" => {
                        let state: tauri::State<'_, AppState> = app.state();
                        let s = state.settings.lock().unwrap();
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
