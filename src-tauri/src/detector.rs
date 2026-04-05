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
    #[cfg(target_os = "linux")]
    if discover_linux_iio_sensor().is_some() {
        return true;
    }

    let api = match HidApi::new() {
        Ok(api) => api,
        Err(_) => return false,
    };

    #[cfg(target_os = "macos")]
    if discover_apple_silicon_sensor(&api).is_some() {
        return true;
    }

    discover_sensor_device_with_api(&api).is_some()
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

// ── Accelerometer Backends ─────────────────────────────────────────

#[cfg(target_os = "linux")]
struct IioSensor {
    base_path: std::path::PathBuf,
    scale: f32,
}

enum AccelBackend {
    GenericHid(HidDevice),
    #[cfg(target_os = "macos")]
    AppleSilicon(HidDevice),
    #[cfg(target_os = "linux")]
    LinuxIio(IioSensor),
}

/// Try platform-specific accelerometer APIs first, then fall back to generic
/// HID sensor discovery.
fn discover_accel_backend() -> Option<AccelBackend> {
    #[cfg(target_os = "linux")]
    {
        if let Some(iio) = discover_linux_iio_sensor() {
            return Some(AccelBackend::LinuxIio(iio));
        }
    }

    let api = HidApi::new().ok()?;

    #[cfg(target_os = "macos")]
    {
        if let Some(device) = discover_apple_silicon_sensor(&api) {
            return Some(AccelBackend::AppleSilicon(device));
        }
    }

    let device = discover_sensor_device_with_api(&api)?;
    Some(AccelBackend::GenericHid(device))
}

struct AccelReading {
    sample: [f32; 3],
    /// Pre-computed peak inter-sample delta from a high-frequency buffer
    /// (Apple Silicon IMU at 800 Hz).  0.0 means the caller should compute
    /// the delta from its own `last_sample`.
    peak_motion: f32,
}

fn read_backend_sample(backend: &AccelBackend) -> Option<AccelReading> {
    match backend {
        AccelBackend::GenericHid(device) => read_sensor_sample(device)
            .map(|s| AccelReading { sample: s, peak_motion: 0.0 }),
        #[cfg(target_os = "macos")]
        AccelBackend::AppleSilicon(device) => read_apple_silicon_reading(device),
        #[cfg(target_os = "linux")]
        AccelBackend::LinuxIio(sensor) => read_iio_sample(sensor)
            .map(|s| AccelReading { sample: s, peak_motion: 0.0 }),
    }
}

// ── Apple Silicon (M1 Pro / M2+) ──────────────────────────────────
//
// Uses the undocumented AppleSPUHIDDevice (Bosch BMI286 IMU managed by the
// Sensor Processing Unit).  The sensor is exposed as a vendor-specific HID
// device with usage page 0xFF00, usage 3.  Reports are 22 bytes with x/y/z
// as int32 little-endian at offsets 6, 10, 14 (divide by 65536 → g).
//
// Requires elevated privileges on most setups (the device is protected by
// the kernel).  If the open fails the detector falls back to generic HID
// and then to the microphone.

#[cfg(target_os = "macos")]
fn discover_apple_silicon_sensor(api: &HidApi) -> Option<HidDevice> {
    for info in api.device_list() {
        if info.usage_page() != 0xFF00 {
            continue;
        }

        let product = info.product_string().unwrap_or("?");
        let usage = info.usage();
        eprintln!(
            "[accel] candidate: usage_page=0xFF00 usage={} product={:?}",
            usage, product
        );

        let device = match info.open_device(api) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[accel]   open failed: {}", e);
                continue;
            }
        };

        // Read two reports and verify:
        // 1. Magnitude near 1g (real accelerometer at rest)
        // 2. Values actually change (real sensor has jitter, a static
        //    device returns the exact same bytes every time)
        let r1 = read_apple_silicon_reading(&device);
        // Small delay to let the sensor buffer a new report
        std::thread::sleep(Duration::from_millis(20));
        let r2 = read_apple_silicon_reading(&device);

        if let (Some(r1), Some(r2)) = (r1, r2) {
            let mag = (r1.sample[0] * r1.sample[0]
                + r1.sample[1] * r1.sample[1]
                + r1.sample[2] * r1.sample[2])
            .sqrt();

            let changed = r1.sample[0] != r2.sample[0]
                || r1.sample[1] != r2.sample[1]
                || r1.sample[2] != r2.sample[2];

            eprintln!(
                "[accel]   mag={:.3}g changed={} sample={:.4},{:.4},{:.4}",
                mag, changed, r1.sample[0], r1.sample[1], r1.sample[2]
            );

            // Real accelerometer: ~1g at rest, values jitter slightly.
            if mag > 0.7 && mag < 1.5 && changed {
                eprintln!("[accel]   ACCEPTED as accelerometer");
                return Some(device);
            }
        }
    }

    None
}

#[cfg(target_os = "macos")]
fn read_apple_silicon_reading(device: &HidDevice) -> Option<AccelReading> {
    let mut buf = [0u8; 64];
    let mut prev: Option<[f32; 3]> = None;
    let mut latest: Option<[f32; 3]> = None;
    let mut peak_motion: f32 = 0.0;

    // The IMU runs at ~800 Hz → ~13 reports per 16 ms polling interval.
    // Process ALL buffered reports and track the maximum inter-sample
    // delta.  A slap lasting <10 ms is captured in the 800 Hz data even
    // if it falls entirely between two 16 ms polling points.
    for i in 0..128u32 {
        let timeout = if i == 0 && latest.is_none() { 50 } else { 0 };
        match device.read_timeout(&mut buf, timeout) {
            Ok(len) if len >= 18 => {
                if let Some(sample) = decode_apple_silicon_report(&buf[..len]) {
                    if let Some(p) = prev {
                        peak_motion = peak_motion.max(vector_delta(p, sample));
                    }
                    prev = Some(sample);
                    latest = Some(sample);
                }
            }
            _ => break,
        }
    }

    latest.map(|sample| AccelReading { sample, peak_motion })
}

#[cfg(target_os = "macos")]
fn decode_apple_silicon_report(report: &[u8]) -> Option<[f32; 3]> {
    if report.len() < 18 {
        return None;
    }
    let x = i32::from_le_bytes(report[6..10].try_into().ok()?) as f32 / 65536.0;
    let y = i32::from_le_bytes(report[10..14].try_into().ok()?) as f32 / 65536.0;
    let z = i32::from_le_bytes(report[14..18].try_into().ok()?) as f32 / 65536.0;
    Some([x, y, z])
}

// ── Linux IIO subsystem ───────────────────────────────────────────
//
// Reads from /sys/bus/iio/devices/iio:deviceN/in_accel_{x,y,z}_raw.
// Common on convertibles (Bosch BMA, ST LIS3LV02D, Kionix KXCJ9) and
// some older HP/ThinkPad laptops with hard-drive protection sensors.

#[cfg(target_os = "linux")]
fn discover_linux_iio_sensor() -> Option<IioSensor> {
    let base = std::path::Path::new("/sys/bus/iio/devices");
    if !base.is_dir() {
        return None;
    }

    for entry in std::fs::read_dir(base).ok()?.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.join("in_accel_x_raw").exists()
            || !path.join("in_accel_y_raw").exists()
            || !path.join("in_accel_z_raw").exists()
        {
            continue;
        }

        // Scale converts raw ADC counts → m/s².
        let scale = std::fs::read_to_string(path.join("in_accel_scale"))
            .ok()
            .and_then(|s| s.trim().parse::<f32>().ok())
            .unwrap_or(0.009_576); // ≈ ±2 g / 16-bit default

        return Some(IioSensor {
            base_path: path,
            scale,
        });
    }

    None
}

#[cfg(target_os = "linux")]
fn read_iio_sample(sensor: &IioSensor) -> Option<[f32; 3]> {
    let x = read_iio_raw(&sensor.base_path, "in_accel_x_raw")?;
    let y = read_iio_raw(&sensor.base_path, "in_accel_y_raw")?;
    let z = read_iio_raw(&sensor.base_path, "in_accel_z_raw")?;
    // raw × scale → m/s² → ÷ 9.80665 → g
    let to_g = sensor.scale / 9.80665;
    Some([x * to_g, y * to_g, z * to_g])
}

#[cfg(target_os = "linux")]
fn read_iio_raw(base: &std::path::Path, filename: &str) -> Option<f32> {
    std::fs::read_to_string(base.join(filename))
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
}

// ── Microphone Detector ───────────────────────────────────────────

/// Mutable state shared across audio callbacks for noise-aware slap detection.
struct MicState {
    noise_floor: f32,
    last_trigger: Instant,
    callbacks_seen: u32,
    /// After a trigger, we suppress detection for this duration so the
    /// sound playing through the speakers doesn't re-trigger the detector
    /// (feedback loop).  This is longer than the user cooldown because
    /// moan sounds can last 3-5 seconds.
    suppress_until: Instant,
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

    let state = Arc::new(Mutex::new(MicState {
        noise_floor: threshold, // start at threshold to avoid false triggers during warmup
        last_trigger: Instant::now() - Duration::from_secs(10),
        callbacks_seen: 0,
        suppress_until: Instant::now(),
    }));
    let running = Arc::new(AtomicBool::new(true));
    let running_ref = running.clone();

    // ~20 callbacks ≈ 400 ms at 48 kHz / 1024-sample buffers
    const CALIBRATION_CALLBACKS: u32 = 20;
    // Crest factor (peak / RMS) threshold — a physical slap is extremely
    // impulsive (crest 4–10).  Speaker playback of moans/voice sits around
    // 1.5–2.0.  Setting this to 2.5 cleanly separates the two without
    // needing aggressive suppression.
    const CREST_FACTOR_MIN: f32 = 2.5;
    // The spike must exceed the noise floor by this factor.
    const NOISE_FLOOR_FACTOR: f32 = 3.0;

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

                // RMS of first channel
                let sum: f32 = data.iter().step_by(ch).map(|s| s * s).sum();
                let rms = (sum / count as f32).sqrt();

                // Peak amplitude (for crest-factor / transient detection)
                let peak: f32 = data
                    .iter()
                    .step_by(ch)
                    .map(|s| s.abs())
                    .fold(0.0f32, f32::max);

                let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                s.callbacks_seen = s.callbacks_seen.saturating_add(1);

                // ── Calibration: learn the noise floor without triggering ──
                if s.callbacks_seen <= CALIBRATION_CALLBACKS {
                    s.noise_floor = s.noise_floor.max(rms * 1.5).max(0.005);
                    return;
                }

                // ── Playback suppression: ignore mic input while our own
                //    sound is likely still playing through the speakers ──
                let now = Instant::now();
                if now < s.suppress_until {
                    // Freeze the noise floor during suppression — don't
                    // adapt it up (would block subsequent slaps) or down.
                    return;
                }

                // ── Adapt noise floor from non-spike samples (slow EMA) ──
                if rms < s.noise_floor * 2.5 {
                    s.noise_floor = s.noise_floor * 0.997 + rms * 0.003;
                    s.noise_floor = s.noise_floor.max(0.005);
                }

                // ── Trigger conditions — ALL must be true ──
                // 1. Above user-configured threshold
                let above_threshold = rms > threshold;
                // 2. Well above current ambient noise
                let above_noise = rms > s.noise_floor * NOISE_FLOOR_FACTOR;
                // 3. Impulsive shape (high crest factor)
                let crest = if rms > 0.0001 { peak / rms } else { 0.0 };
                let is_impulsive = crest > CREST_FACTOR_MIN;

                if above_threshold && above_noise && is_impulsive {
                    let elapsed = now.duration_since(s.last_trigger).as_millis() as u64;
                    if elapsed >= cooldown_ms {
                        s.last_trigger = now;
                        // Minimal suppression — just long enough for the
                        // speaker's initial pop to pass without re-triggering.
                        // The noise floor boost handles the rest.
                        s.suppress_until =
                            now + Duration::from_millis(cooldown_ms);
                        drop(s);
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

fn run_accelerometer_loop(
    threshold: f32,
    cooldown_ms: u64,
    stop_rx: mpsc::Receiver<()>,
    on_slap: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<(), String> {
    let backend = discover_accel_backend()
        .ok_or_else(|| "No compatible accelerometer or motion sensor found".to_string())?;

    // ── High-pass filter (same as spank) ──────────────────────────────
    // Single-pole IIR, α = 0.95, cutoff ≈ 0.8 Hz at 62.5 Hz polling.
    // Strips the constant gravity vector so only dynamic acceleration
    // (slaps, bumps) remains.  Without this, slowly tilting the laptop
    // rotates the gravity vector and floods the STA/LTA baseline.
    const HP_ALPHA: f32 = 0.95;
    let mut hp_prev_raw = [0.0f32; 3];
    let mut hp_prev_out = [0.0f32; 3];
    let mut hp_initialized = false;

    // ── STA/LTA (Short-Term Average / Long-Term Average) ──────────────
    let mut sta: f32 = 0.0;
    let mut lta: f32 = 0.0;
    let mut last_trigger = Instant::now() - Duration::from_secs(10);
    let mut warmup_count: u32 = 0;

    const WARMUP_SAMPLES: u32 = 30;
    const STA_ALPHA: f32 = 0.3;
    const LTA_ALPHA: f32 = 0.005;
    const STA_LTA_RATIO: f32 = 5.0;

    // Map mic threshold (0.01–0.5 RMS) → accelerometer threshold (g).
    let accel_threshold = (threshold * 0.6).clamp(0.04, 0.30);

    loop {
        if stop_rx.try_recv().is_ok() {
            break;
        }

        if let Some(reading) = read_backend_sample(&backend) {
            if warmup_count < 5 {
                eprintln!(
                    "[accel] sample={:.4},{:.4},{:.4} peak_motion={:.6}",
                    reading.sample[0], reading.sample[1], reading.sample[2],
                    reading.peak_motion
                );
            }

            // ── Apply high-pass filter to remove gravity ──────────
            if !hp_initialized {
                hp_prev_raw = reading.sample;
                hp_prev_out = [0.0; 3];
                hp_initialized = true;
                thread::sleep(Duration::from_millis(16));
                continue;
            }

            let filtered = [
                HP_ALPHA * (hp_prev_out[0] + reading.sample[0] - hp_prev_raw[0]),
                HP_ALPHA * (hp_prev_out[1] + reading.sample[1] - hp_prev_raw[1]),
                HP_ALPHA * (hp_prev_out[2] + reading.sample[2] - hp_prev_raw[2]),
            ];
            hp_prev_raw = reading.sample;
            hp_prev_out = filtered;

            // Filtered magnitude = dynamic acceleration only (no gravity).
            let filtered_mag = (filtered[0] * filtered[0]
                + filtered[1] * filtered[1]
                + filtered[2] * filtered[2])
            .sqrt();

            // Use the high-pass filtered signal for STA/LTA (gravity-free).
            // The 800 Hz peak_motion is a raw delta that still contains
            // gravity rotation, so we only use it as an extra trigger
            // condition, not as the STA/LTA input.
            let motion = filtered_mag;

            // ── STA/LTA on energy (mag²) — matches spank ────────
            let energy = motion * motion;
            sta = sta * (1.0 - STA_ALPHA) + energy * STA_ALPHA;
            lta = lta * (1.0 - LTA_ALPHA) + energy * LTA_ALPHA;

            warmup_count = warmup_count.saturating_add(1);
            if warmup_count < WARMUP_SAMPLES {
                lta = lta.max(energy * 0.5);
                thread::sleep(Duration::from_millis(16));
                continue;
            }

            let ratio = if lta > 0.000_001 { sta / lta } else { 0.0 };

            // Trigger if: STA/LTA spikes AND EITHER the filtered signal
            // or the 800 Hz peak delta exceeds the threshold.
            let effective_motion = motion.max(reading.peak_motion);

            if ratio > 2.0 || effective_motion > 0.02 {
                eprintln!(
                    "[accel] filtered={:.5} peak={:.5} ratio={:.2} thresh={:.3}",
                    motion, reading.peak_motion, ratio, accel_threshold
                );
            }

            if ratio > STA_LTA_RATIO && effective_motion >= accel_threshold {
                let now = Instant::now();
                let elapsed = now.duration_since(last_trigger).as_millis() as u64;
                if elapsed >= cooldown_ms {
                    last_trigger = now;
                    // Logarithmic volume scaling similar to spank:
                    // map [0.05, 0.80]g → intensity [0.15, 1.0]
                    let intensity = ((effective_motion / 0.05).ln()
                        / (0.80f32 / 0.05).ln())
                    .clamp(0.15, 1.0);
                    on_slap(intensity);
                }
            }
        }

        thread::sleep(Duration::from_millis(16));
    }

    Ok(())
}

fn discover_sensor_device_with_api(api: &HidApi) -> Option<HidDevice> {
    let mut candidates = Vec::new();

    for device in api.device_list() {
        let product = device.product_string().unwrap_or("").to_lowercase();
        let manufacturer = device.manufacturer_string().unwrap_or("").to_lowercase();

        // HID usage page 0x20 = Sensors, 0xFF00 = Vendor-specific (Apple SPU)
        let usage_page = device.usage_page();
        let is_sensor_page = usage_page == 0x20 || usage_page == 0xFF00;

        // Only match strongly sensor-specific keywords (not vague terms like
        // "internal" or manufacturer names that match keyboards/trackpads).
        let sensor_keyword = ["accelerometer", "motion sensor", "imu"]
            .iter()
            .any(|kw| product.contains(kw) || manufacturer.contains(kw));

        if is_sensor_page || sensor_keyword {
            candidates.push(device);
        }
    }

    for device_info in candidates {
        if let Ok(device) = device_info.open_device(api) {
            if let Some(sample) = read_sensor_sample(&device) {
                // Sanity check: a stationary accelerometer reads ~1 g.
                // Reject readings that are clearly not acceleration data.
                let mag = (sample[0] * sample[0]
                    + sample[1] * sample[1]
                    + sample[2] * sample[2])
                .sqrt();
                if mag > 0.5 && mag < 3.0 {
                    return Some(device);
                }
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

    // Only accept 3-axis data (6+ bytes).  The previous 1- and 2-byte
    // fallbacks silently parsed random HID reports (keyboards, trackpads)
    // as sensor values, causing phantom triggers.
    if payload.len() >= 6 {
        let x = i16::from_le_bytes([payload[0], payload[1]]) as f32 / 16384.0;
        let y = i16::from_le_bytes([payload[2], payload[3]]) as f32 / 16384.0;
        let z = i16::from_le_bytes([payload[4], payload[5]]) as f32 / 16384.0;
        Some([x, y, z])
    } else {
        None
    }
}

#[cfg(target_os = "macos")]
fn vector_delta(prev: [f32; 3], curr: [f32; 3]) -> f32 {
    let dx = curr[0] - prev[0];
    let dy = curr[1] - prev[1];
    let dz = curr[2] - prev[2];
    (dx * dx + dy * dy + dz * dz).sqrt()
}
