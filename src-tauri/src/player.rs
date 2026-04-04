use rand::seq::SliceRandom;
use rodio::{Decoder, OutputStream, Sink};
use std::fs;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::mpsc;

struct PlayCmd {
    bundle: String,
    volume: f32,
    intensity: f32,
}

/// Thread-safe handle to play sounds (Send + Sync).
pub struct PlayerHandle {
    cmd_tx: mpsc::Sender<PlayCmd>,
    sounds_dir: PathBuf,
}

impl PlayerHandle {
    pub fn new(sounds_dir: PathBuf) -> Self {
        fs::create_dir_all(&sounds_dir).ok();

        let (cmd_tx, cmd_rx) = mpsc::channel::<PlayCmd>();
        let dir = sounds_dir.clone();

        std::thread::spawn(move || {
            let (_stream, stream_handle) = match OutputStream::try_default() {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Failed to open audio output: {}", e);
                    return;
                }
            };

            // Shuffle bag: plays all sounds before repeating any
            let mut bag: Vec<PathBuf> = Vec::new();
            let mut bag_index: usize = 0;
            let mut bag_bundle = String::new();

            // Single sink — stops previous sound before playing next
            let mut current_sink: Option<Sink> = None;

            loop {
                match cmd_rx.recv() {
                    Ok(cmd) => {
                        let bundle_dir = dir.join(&cmd.bundle);
                        let files = list_sounds(&bundle_dir);

                        if files.is_empty() {
                            eprintln!("No sound files in bundle {:?}", cmd.bundle);
                            continue;
                        }

                        // Reset bag if bundle changed or bag exhausted or files changed
                        if cmd.bundle != bag_bundle || bag_index >= bag.len() || bag.len() != files.len() {
                            bag = files;
                            bag.shuffle(&mut rand::thread_rng());
                            bag_index = 0;
                            bag_bundle = cmd.bundle.clone();
                        }

                        let file = &bag[bag_index];
                        bag_index += 1;

                        match fs::File::open(file) {
                            Ok(f) => {
                                let reader = BufReader::new(f);
                                match Decoder::new(reader) {
                                    Ok(source) => {
                                        // Stop previous sound
                                        if let Some(old) = current_sink.take() {
                                            old.stop();
                                        }
                                        if let Ok(sink) = Sink::try_new(&stream_handle) {
                                            sink.set_volume(cmd.volume * cmd.intensity);
                                            sink.append(source);
                                            current_sink = Some(sink);
                                        }
                                    }
                                    Err(e) => eprintln!("Failed to decode {:?}: {}", file, e),
                                }
                            }
                            Err(e) => eprintln!("Failed to open {:?}: {}", file, e),
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        PlayerHandle { cmd_tx, sounds_dir }
    }

    pub fn play(&self, bundle: &str, volume: f32, intensity: f32) {
        self.cmd_tx
            .send(PlayCmd {
                bundle: bundle.to_string(),
                volume,
                intensity,
            })
            .ok();
    }

    pub fn bundle_has_sounds(&self, bundle: &str) -> bool {
        if bundle.trim().is_empty() {
            return false;
        }

        !list_sounds(&self.sounds_dir.join(bundle)).is_empty()
    }

    pub fn first_playable_bundle(&self) -> Option<String> {
        self.list_bundles()
            .into_iter()
            .find(|bundle| bundle.count > 0)
            .map(|bundle| bundle.name)
    }

    // ── Bundle Management ───────────────────────────

    pub fn list_bundles(&self) -> Vec<BundleInfo> {
        let mut bundles = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.sounds_dir) {
            for e in entries.filter_map(|e| e.ok()) {
                if e.path().is_dir() {
                    if let Some(name) = e.file_name().into_string().ok() {
                        let count = list_sounds(&e.path()).len();
                        bundles.push(BundleInfo { name, count });
                    }
                }
            }
        }
        bundles.sort_by(|a, b| a.name.cmp(&b.name));
        bundles
    }

    pub fn create_bundle(&self, name: &str) -> Result<(), String> {
        let dir = self.sounds_dir.join(name);
        if dir.exists() {
            return Err(format!("Bundle '{}' already exists", name));
        }
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create bundle: {}", e))
    }

    pub fn delete_bundle(&self, name: &str) -> Result<(), String> {
        let dir = self.sounds_dir.join(name);
        if !dir.exists() {
            return Err(format!("Bundle '{}' not found", name));
        }
        fs::remove_dir_all(&dir).map_err(|e| format!("Failed to delete bundle: {}", e))
    }

    pub fn rename_bundle(&self, old: &str, new: &str) -> Result<(), String> {
        let old_dir = self.sounds_dir.join(old);
        let new_dir = self.sounds_dir.join(new);
        if !old_dir.exists() {
            return Err(format!("Bundle '{}' not found", old));
        }
        if new_dir.exists() {
            return Err(format!("Bundle '{}' already exists", new));
        }
        fs::rename(&old_dir, &new_dir).map_err(|e| format!("Failed to rename: {}", e))
    }

    // ── Sound File Management ───────────────────────

    pub fn list_bundle_sounds(&self, bundle: &str) -> Vec<SoundInfo> {
        let dir = self.sounds_dir.join(bundle);
        list_sounds(&dir)
            .into_iter()
            .filter_map(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .map(|name| SoundInfo {
                        name: name.to_string(),
                    })
            })
            .collect()
    }

    pub fn import_sounds(&self, bundle: &str, paths: &[String]) -> Result<Vec<String>, String> {
        let dir = self.sounds_dir.join(bundle);
        fs::create_dir_all(&dir).map_err(|e| format!("Failed to create dir: {}", e))?;

        let mut imported = Vec::new();
        for path in paths {
            let src = std::path::Path::new(path);
            if let Some(filename) = src.file_name() {
                let dest = dir.join(filename);
                match fs::copy(src, &dest) {
                    Ok(_) => imported.push(filename.to_string_lossy().to_string()),
                    Err(e) => eprintln!("Failed to copy {}: {}", path, e),
                }
            }
        }
        Ok(imported)
    }

    pub fn remove_sound(&self, bundle: &str, filename: &str) -> Result<(), String> {
        let path = self.sounds_dir.join(bundle).join(filename);
        fs::remove_file(&path).map_err(|e| format!("Failed to remove: {}", e))
    }
}

#[derive(serde::Serialize)]
pub struct BundleInfo {
    pub name: String,
    pub count: usize,
}

#[derive(serde::Serialize)]
pub struct SoundInfo {
    pub name: String,
}

fn list_sounds(dir: &PathBuf) -> Vec<PathBuf> {
    match fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("wav" | "mp3" | "ogg" | "flac")
                )
            })
            .collect(),
        Err(_) => Vec::new(),
    }
}
