use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tauri::Manager as _;

use crate::detector::DetectionMode;
use crate::ports::{repair_rules, PortKind, PortRule};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub sensitivity: f32,
    pub cooldown_ms: u64,
    pub volume: f32,
    pub enabled: bool,
    pub detection_mode: DetectionMode,
    pub bundle: String,
    pub port_rules: Vec<PortRule>,
    pub hide_support_prompt: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            sensitivity: 0.15,
            cooldown_ms: 2000,
            volume: 0.8,
            enabled: true,
            detection_mode: DetectionMode::Microphone,
            bundle: "default".to_string(),
            port_rules: PortKind::all()
                .into_iter()
                .map(PortRule::default_for)
                .collect(),
            hide_support_prompt: false,
        }
    }
}

impl Settings {
    pub fn config_path(app_handle: &tauri::AppHandle) -> PathBuf {
        let dir = app_handle
            .path()
            .app_config_dir()
            .expect("failed to get config dir");
        fs::create_dir_all(&dir).ok();
        dir.join("settings.json")
    }

    pub fn load(app_handle: &tauri::AppHandle) -> Self {
        let path = Self::config_path(app_handle);
        match fs::read_to_string(&path) {
            Ok(content) => serde_json::from_str(&content).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, app_handle: &tauri::AppHandle) {
        let path = Self::config_path(app_handle);
        if let Ok(json) = serde_json::to_string_pretty(self) {
            fs::write(path, json).ok();
        }
    }

    pub fn validate(&mut self) {
        self.sensitivity = self.sensitivity.clamp(0.01, 1.0);
        self.cooldown_ms = self.cooldown_ms.clamp(200, 10000);
        self.volume = self.volume.clamp(0.0, 1.0);

        if !matches!(
            self.detection_mode,
            DetectionMode::Microphone | DetectionMode::Accelerometer
        ) {
            self.detection_mode = DetectionMode::Microphone;
        }

        repair_rules(&mut self.port_rules);
    }
}
