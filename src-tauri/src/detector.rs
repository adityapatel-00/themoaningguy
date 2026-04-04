use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use hidapi::{HidApi, HidDevice};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum DetectionMode {
    Microphone,
    Accelerometer,
}

impl Default for DetectionMode {
    fn default() -> Self {
        Self::Microphone
    }
}

pub enum DetectorCmd {
    Start {
        mode: DetectionMode,
        threshold: f32,
        cooldown_ms: u64,
    },
    Stop,
}

/// Thread-safe handle to control the slap detector.
pub struct DetectorHandle {
    cmd_tx: mpsc::Sender<DetectorCmd>,
}

enum ActiveDetector {
    Microphone {
        stream: cpal::Stream,
        running: Arc<AtomicBool>,
    },
    Accelerometer {
        stop_tx: mpsc::Sender<()>,
        join: JoinHandle<()>,
    },
}

impl DetectorHandle {
    /// Spawn the detector on a dedicated thread.
    pub fn spawn(on_slap: impl Fn(f32) + Send + Sync + 'static) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<DetectorCmd>();
        let on_slap = Arc::new(on_slap);

        thread::spawn(move || {
            let mut runtime: Option<ActiveDetector> = None;

            loop {
                match cmd_rx.recv() {
                    Ok(DetectorCmd::Start {
                        mode,
                        threshold,
                        cooldown_ms,
                    }) => {
                        stop_runtime(&mut runtime);

                        runtime = match mode {
                            DetectionMode::Microphone => {
                                build_microphone_detector(
                                    threshold,
                                    cooldown_ms,
                                    on_slap.clone(),
                                )
                                .map(Some)
                                .unwrap_or_else(|e| {
                                    eprintln!("Failed to start microphone detector: {}", e);
                                    None
                                })
                            }
                            DetectionMode::Accelerometer => {
                                match build_accelerometer_detector(
                                    threshold,
                                    cooldown_ms,
                                    on_slap.clone(),
                                ) {
                                    Ok(detector) => Some(detector),
                                    Err(err) => {
                                        eprintln!(
                                            "Accelerometer detector unavailable, falling back to microphone: {}",
                                            err
                                        );
                                        build_microphone_detector(
                                            threshold,
                                            cooldown_ms,
                                            on_slap.clone(),
                                        )
                                        .map(Some)
                                        .unwrap_or_else(|fallback_err| {
                                            eprintln!(
                                                "Failed to start fallback microphone detector: {}",
                                                fallback_err
                                            );
                                            None
                                        })
                                    }
                                }
                            }
                        };
                    }
                    Ok(DetectorCmd::Stop) => {
                        stop_runtime(&mut runtime);
                    }
                    Err(_) => {
                        stop_runtime(&mut runtime);
                        break;
                    }
                }
            }
        });

        DetectorHandle { cmd_tx }
    }

    pub fn start(&self, mode: DetectionMode, threshold: f32, cooldown_ms: u64) {
        self.cmd_tx
            .send(DetectorCmd::Start {
                mode,
                threshold,
                cooldown_ms,
            })
            .ok();
    }

    pub fn stop(&self) {
        self.cmd_tx.send(DetectorCmd::Stop).ok();
    }
}

pub fn accelerometer_available() -> bool {
    discover_sensor_device().is_some()
}

fn stop_runtime(runtime: &mut Option<ActiveDetector>) {
    if let Some(active) = runtime.take() {
        match active {
            ActiveDetector::Microphone { stream, running } => {
                running.store(false, Ordering::SeqCst);
                drop(stream);
            }
            ActiveDetector::Accelerometer { stop_tx, join } => {
                let _ = stop_tx.send(());
                let _ = join.join();
            }
        }
    }
}

fn build_microphone_detector(
    threshold: f32,
    cooldown_ms: u64,
    on_slap: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<ActiveDetector, String> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or("No input device found")?;

    let supported = device
        .default_input_config()
        .map_err(|e| format!("Failed to get input config: {}", e))?;

    let channels = supported.channels() as usize;
    let config: cpal::StreamConfig = supported.into();

    let last_trigger = Arc::new(Mutex::new(
        Instant::now() - Duration::from_secs(10),
    ));
    let running = Arc::new(AtomicBool::new(true));
    let running_ref = running.clone();

    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if !running_ref.load(Ordering::Relaxed) {
                    return;
                }

                let ch = channels.max(1);
                let count = data.len() / ch;
                if count == 0 {
                    return;
                }

                let sum: f32 = data.iter().step_by(ch).map(|s| s * s).sum();
                let rms = (sum / count as f32).sqrt();

                if rms > threshold {
                    let now = Instant::now();
                    let mut lt = last_trigger.lock().unwrap();
                    let elapsed = now.duration_since(*lt).as_millis() as u64;
                    if elapsed >= cooldown_ms {
                        *lt = now;
                        drop(lt);
                        let intensity = (rms / 0.5).min(1.0);
                        on_slap(intensity);
                    }
                }
            },
            |err| eprintln!("Audio input error: {}", err),
            None,
        )
        .map_err(|e| format!("Failed to build stream: {}", e))?;

    stream
        .play()
        .map_err(|e| format!("Failed to play: {}", e))?;

    Ok(ActiveDetector::Microphone { stream, running })
}

fn build_accelerometer_detector(
    threshold: f32,
    cooldown_ms: u64,
    on_slap: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<ActiveDetector, String> {
    if !accelerometer_available() {
        return Err("No compatible accelerometer or motion sensor found".to_string());
    }

    let (stop_tx, stop_rx) = mpsc::channel::<()>();

    let join = thread::spawn(move || {
        if let Err(err) = run_accelerometer_loop(threshold, cooldown_ms, stop_rx, on_slap) {
            eprintln!("Accelerometer detector stopped: {}", err);
        }
    });

    Ok(ActiveDetector::Accelerometer { stop_tx, join })
}

fn run_accelerometer_loop(
    threshold: f32,
    cooldown_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_slap: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<(), String> {
    let api = HidApi::new().map_err(|e| format!("Failed to initialize HID API: {}", e))?;
    let device = discover_sensor_device_with_api(&api)
        .ok_or_else(|| "No compatible accelerometer or motion sensor found".to_string())?;

    let mut last_sample: Option<[f32; 3]> = None;
    let mut last_trigger = Instant::now() - Duration::from_secs(10);

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        if let Some(sample) = read_sensor_sample(&device) {
            if let Some(prev) = last_sample {
                let motion = vector_delta(prev, sample);
                let trigger_threshold = threshold.max(0.02);

                if motion >= trigger_threshold {
                    let now = Instant::now();
                    let elapsed = now.duration_since(last_trigger).as_millis() as u64;
                    if elapsed >= cooldown_ms {
                        last_trigger = now;
                        let intensity = (motion / 0.35).clamp(0.15, 1.0);
                        on_slap(intensity);
                    }
                }
            }

            last_sample = Some(sample);
        }

        thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}

fn discover_sensor_device() -> Option<HidDevice> {
    let api = HidApi::new().ok()?;
    discover_sensor_device_with_api(&api)
}

fn discover_sensor_device_with_api(api: &HidApi) -> Option<HidDevice> {
    let mut candidates = Vec::new();

    for device in api.device_list() {
        let product = device.product_string().unwrap_or("").to_lowercase();
        let manufacturer = device.manufacturer_string().unwrap_or("").to_lowercase();

        let sensor_hint = [
            "accelerometer",
            "sensor",
            "motion",
            "orientation",
            "lid",
        ]
        .iter()
        .any(|needle| product.contains(needle) || manufacturer.contains(needle));

        if sensor_hint {
            candidates.push(device);
        }
    }

    if candidates.is_empty() {
        for device in api.device_list() {
            let product = device.product_string().unwrap_or("").to_lowercase();
            if product.contains("internal") || product.contains("apple") {
                candidates.push(device);
            }
        }
    }

    for device_info in candidates {
        if let Ok(device) = device_info.open_device(api) {
            if read_sensor_sample(&device).is_some() {
                return Some(device);
            }
        }
    }

    None
}

fn read_sensor_sample(device: &HidDevice) -> Option<[f32; 3]> {
    for report_id in [1_u8, 0_u8, 2_u8] {
        let mut buf = [0u8; 64];
        buf[0] = report_id;

        if let Ok(len) = device.get_feature_report(&mut buf) {
            if let Some(sample) = decode_sensor_sample(&buf[..len]) {
                return Some(sample);
            }
        }
    }

    None
}

fn decode_sensor_sample(report: &[u8]) -> Option<[f32; 3]> {
    let payload = report.get(1..)?;

    if payload.len() >= 6 {
        let x = i16::from_le_bytes([payload[0], payload[1]]) as f32 / 16384.0;
        let y = i16::from_le_bytes([payload[2], payload[3]]) as f32 / 16384.0;
        let z = i16::from_le_bytes([payload[4], payload[5]]) as f32 / 16384.0;
        Some([x, y, z])
    } else if payload.len() >= 2 {
        let value = u16::from_le_bytes([payload[0], payload[1]]) as f32 / 100.0;
        Some([value, 0.0, 0.0])
    } else if payload.len() == 1 {
        Some([payload[0] as f32 / 10.0, 0.0, 0.0])
    } else {
        None
    }
}

fn vector_delta(prev: [f32; 3], curr: [f32; 3]) -> f32 {
    let dx = curr[0] - prev[0];
    let dy = curr[1] - prev[1];
    let dz = curr[2] - prev[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}
