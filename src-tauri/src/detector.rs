use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
#[cfg(not(target_os = "macos"))]
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
    #[cfg(target_os = "macos")]
    if apple_spu::probe() {
        return true;
    }

    #[cfg(target_os = "linux")]
    if discover_linux_iio_sensor().is_some() {
        return true;
    }

    #[cfg(not(target_os = "macos"))]
    {
        let api = match HidApi::new() {
            Ok(api) => api,
            Err(_) => return false,
        };
        if discover_sensor_device_with_api(&api).is_some() {
            return true;
        }
    }

    false
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
    #[cfg(not(target_os = "macos"))]
    GenericHid(HidDevice),
    #[cfg(target_os = "macos")]
    AppleSilicon(apple_spu::SpuAccel),
    #[cfg(target_os = "linux")]
    LinuxIio(IioSensor),
}

fn discover_accel_backend() -> Option<AccelBackend> {
    #[cfg(target_os = "macos")]
    {
        if let Some(spu) = apple_spu::SpuAccel::open() {
            return Some(AccelBackend::AppleSilicon(spu));
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(iio) = discover_linux_iio_sensor() {
            return Some(AccelBackend::LinuxIio(iio));
        }
    }

    #[cfg(not(target_os = "macos"))]
    {
        let api = HidApi::new().ok()?;
        let device = discover_sensor_device_with_api(&api)?;
        return Some(AccelBackend::GenericHid(device));
    }

    #[cfg(target_os = "macos")]
    None
}

struct AccelReading {
    sample: [f32; 3],
    peak_motion: f32,
}

fn read_backend_sample(backend: &AccelBackend) -> Option<AccelReading> {
    match backend {
        #[cfg(not(target_os = "macos"))]
        AccelBackend::GenericHid(device) => read_sensor_sample(device)
            .map(|s| AccelReading { sample: s, peak_motion: 0.0 }),
        #[cfg(target_os = "macos")]
        AccelBackend::AppleSilicon(spu) => spu.read(),
        #[cfg(target_os = "linux")]
        AccelBackend::LinuxIio(sensor) => read_iio_sample(sensor)
            .map(|s| AccelReading { sample: s, peak_motion: 0.0 }),
    }
}

// ── Apple Silicon (M1 Pro / M2+) ──────────────────────────────────
//
// Uses raw IOKit to talk to AppleSPUHIDDevice — the Bosch BMI286 IMU
// managed by the Sensor Processing Unit.  Reports are 22 bytes with
// x/y/z as int32 LE at offsets 6/10/14 (÷65536 → g).
//
// Before reading, the SPU driver must be woken by setting three
// IORegistry properties (ReportingState, PowerState, ReportInterval)
// on AppleSPUHIDDriver services.  hidapi can't do this — we call the
// IOKit C API directly.

#[cfg(target_os = "macos")]
mod apple_spu {
    use super::AccelReading;
    use std::ffi::{c_char, c_void};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // IOKit C types
    type IOReturn = i32;
    type MachPort = u32;
    type IOServiceT = u32;
    type IOIteratorT = u32;
    type IOHIDDeviceRef = *mut c_void;
    type CFAllocatorRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFNumberRef = *const c_void;
    type CFTypeRef = *const c_void;
    type CFRunLoopRef = *const c_void;
    type CFRunLoopMode = CFStringRef;

    const K_IO_MAIN_PORT_DEFAULT: MachPort = 0;
    const K_CF_ALLOCATOR_DEFAULT: CFAllocatorRef = std::ptr::null();
    const K_CF_NUMBER_SINT32_TYPE: i64 = 3;
    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const IMU_REPORT_LEN: usize = 22;
    const IMU_DATA_OFF: usize = 6;
    const ACCEL_USAGE_PAGE: u32 = 0xFF00;
    const ACCEL_USAGE: u32 = 3;
    const REPORT_BUF_SZ: usize = 256;

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
        fn IOServiceGetMatchingServices(
            main_port: MachPort,
            matching: CFDictionaryRef,
            existing: *mut IOIteratorT,
        ) -> IOReturn;
        fn IOIteratorNext(iterator: IOIteratorT) -> IOServiceT;
        fn IOObjectRelease(object: u32) -> IOReturn;
        fn IORegistryEntrySetCFProperty(
            entry: IOServiceT,
            key: CFStringRef,
            value: CFTypeRef,
        ) -> IOReturn;
        fn IORegistryEntryCreateCFProperty(
            entry: IOServiceT,
            key: CFStringRef,
            allocator: CFAllocatorRef,
            options: u32,
        ) -> CFTypeRef;
        fn IOHIDDeviceCreate(
            allocator: CFAllocatorRef,
            service: IOServiceT,
        ) -> IOHIDDeviceRef;
        fn IOHIDDeviceOpen(device: IOHIDDeviceRef, options: u32) -> IOReturn;
        fn IOHIDDeviceClose(device: IOHIDDeviceRef, options: u32) -> IOReturn;
        fn IOHIDDeviceRegisterInputReportCallback(
            device: IOHIDDeviceRef,
            report: *mut u8,
            report_length: usize,
            callback: unsafe extern "C" fn(
                context: *mut c_void,
                result: IOReturn,
                sender: *mut c_void,
                report_type: u32,
                report_id: u32,
                report: *mut u8,
                report_length: usize,
            ),
            context: *mut c_void,
        );
        fn IOHIDDeviceScheduleWithRunLoop(
            device: IOHIDDeviceRef,
            run_loop: CFRunLoopRef,
            mode: CFRunLoopMode,
        );
        fn IOHIDDeviceUnscheduleFromRunLoop(
            device: IOHIDDeviceRef,
            run_loop: CFRunLoopRef,
            mode: CFRunLoopMode,
        );
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRunLoopGetCurrent() -> CFRunLoopRef;
        fn CFRunLoopRunInMode(
            mode: CFRunLoopMode,
            seconds: f64,
            return_after_source_handled: u8,
        ) -> i32;
        #[allow(dead_code)]
        fn CFRunLoopStop(rl: CFRunLoopRef);
        fn CFStringCreateWithCString(
            alloc: CFAllocatorRef,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFNumberCreate(
            alloc: CFAllocatorRef,
            the_type: i64,
            value_ptr: *const c_void,
        ) -> CFNumberRef;
        fn CFNumberGetValue(
            number: CFNumberRef,
            the_type: i64,
            value_ptr: *mut c_void,
        ) -> u8;
        fn CFRelease(cf: CFTypeRef);
        static kCFRunLoopDefaultMode: CFRunLoopMode;
    }

    fn cfstr(s: &[u8]) -> CFStringRef {
        unsafe {
            CFStringCreateWithCString(
                K_CF_ALLOCATOR_DEFAULT,
                s.as_ptr() as *const c_char,
                K_CF_STRING_ENCODING_UTF8,
            )
        }
    }

    fn cfnum32(v: i32) -> CFNumberRef {
        unsafe {
            CFNumberCreate(
                K_CF_ALLOCATOR_DEFAULT,
                K_CF_NUMBER_SINT32_TYPE,
                &v as *const i32 as *const c_void,
            )
        }
    }

    fn get_u32_property(service: IOServiceT, key: &[u8]) -> Option<u32> {
        unsafe {
            let k = cfstr(key);
            let prop = IORegistryEntryCreateCFProperty(
                service, k, K_CF_ALLOCATOR_DEFAULT, 0,
            );
            CFRelease(k as CFTypeRef);
            if prop.is_null() {
                return None;
            }
            let mut val: i32 = 0;
            let ok = CFNumberGetValue(
                prop as CFNumberRef,
                K_CF_NUMBER_SINT32_TYPE,
                &mut val as *mut i32 as *mut c_void,
            );
            CFRelease(prop);
            if ok != 0 { Some(val as u32) } else { None }
        }
    }

    /// Wake all SPU drivers so the sensor starts streaming.
    fn wake_spu_drivers() {
        unsafe {
            let matching = IOServiceMatching(
                b"AppleSPUHIDDriver\0".as_ptr() as *const c_char,
            );
            if matching.is_null() {
                return;
            }
            let mut iter: IOIteratorT = 0;
            if IOServiceGetMatchingServices(
                K_IO_MAIN_PORT_DEFAULT, matching, &mut iter,
            ) != 0 {
                return;
            }
            loop {
                let svc = IOIteratorNext(iter);
                if svc == 0 { break; }
                let props: &[(&[u8], i32)] = &[
                    (b"SensorPropertyReportingState\0", 1),
                    (b"SensorPropertyPowerState\0", 1),
                    (b"ReportInterval\0", 1000),
                ];
                for &(key, val) in props {
                    let k = cfstr(key);
                    let v = cfnum32(val);
                    IORegistryEntrySetCFProperty(svc, k, v as CFTypeRef);
                    CFRelease(k as CFTypeRef);
                    CFRelease(v as CFTypeRef);
                }
                IOObjectRelease(svc);
            }
            IOObjectRelease(iter);
        }
    }

    /// Find the AppleSPUHIDDevice service for the accelerometer.
    fn find_accel_service() -> Option<IOServiceT> {
        unsafe {
            let matching = IOServiceMatching(
                b"AppleSPUHIDDevice\0".as_ptr() as *const c_char,
            );
            if matching.is_null() {
                return None;
            }
            let mut iter: IOIteratorT = 0;
            if IOServiceGetMatchingServices(
                K_IO_MAIN_PORT_DEFAULT, matching, &mut iter,
            ) != 0 {
                return None;
            }
            loop {
                let svc = IOIteratorNext(iter);
                if svc == 0 { break; }
                let page = get_u32_property(svc, b"PrimaryUsagePage\0");
                let usage = get_u32_property(svc, b"PrimaryUsage\0");
                if page == Some(ACCEL_USAGE_PAGE) && usage == Some(ACCEL_USAGE) {
                    IOObjectRelease(iter);
                    return Some(svc);
                }
                IOObjectRelease(svc);
            }
            IOObjectRelease(iter);
            None
        }
    }

    /// Check if the Apple Silicon accelerometer is available.
    pub fn probe() -> bool {
        wake_spu_drivers();
        let svc = match find_accel_service() {
            Some(s) => s,
            None => return false,
        };
        unsafe {
            let hid = IOHIDDeviceCreate(K_CF_ALLOCATOR_DEFAULT, svc);
            IOObjectRelease(svc);
            if hid.is_null() {
                return false;
            }
            let kr = IOHIDDeviceOpen(hid, 0);
            if kr == 0 {
                IOHIDDeviceClose(hid, 0);
            }
            CFRelease(hid as CFTypeRef);
            kr == 0
        }
    }

    struct CallbackState {
        latest: [f32; 3],
        prev: [f32; 3],
        peak_motion: f32,
        has_data: bool,
    }

    unsafe extern "C" fn report_callback(
        context: *mut c_void,
        _result: IOReturn,
        _sender: *mut c_void,
        _report_type: u32,
        _report_id: u32,
        report: *mut u8,
        report_length: usize,
    ) {
        if report_length < IMU_REPORT_LEN || context.is_null() {
            return;
        }
        let state = &*(context as *const Mutex<CallbackState>);
        let data = std::slice::from_raw_parts(report, report_length);

        let x = i32::from_le_bytes([
            data[IMU_DATA_OFF], data[IMU_DATA_OFF + 1],
            data[IMU_DATA_OFF + 2], data[IMU_DATA_OFF + 3],
        ]) as f32 / 65536.0;
        let y = i32::from_le_bytes([
            data[IMU_DATA_OFF + 4], data[IMU_DATA_OFF + 5],
            data[IMU_DATA_OFF + 6], data[IMU_DATA_OFF + 7],
        ]) as f32 / 65536.0;
        let z = i32::from_le_bytes([
            data[IMU_DATA_OFF + 8], data[IMU_DATA_OFF + 9],
            data[IMU_DATA_OFF + 10], data[IMU_DATA_OFF + 11],
        ]) as f32 / 65536.0;

        if let Ok(mut s) = state.lock() {
            if s.has_data {
                let dx = x - s.latest[0];
                let dy = y - s.latest[1];
                let dz = z - s.latest[2];
                let delta = (dx * dx + dy * dy + dz * dz).sqrt();
                s.peak_motion = s.peak_motion.max(delta);
            }
            s.prev = s.latest;
            s.latest = [x, y, z];
            s.has_data = true;
        }
    }

    pub struct SpuAccel {
        state: Arc<Mutex<CallbackState>>,
        stop: Arc<AtomicBool>,
        _thread: std::thread::JoinHandle<()>,
    }

    impl Drop for SpuAccel {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl SpuAccel {
        pub fn open() -> Option<Self> {
            wake_spu_drivers();
            let svc = find_accel_service()?;

            let state = Arc::new(Mutex::new(CallbackState {
                latest: [0.0; 3],
                prev: [0.0; 3],
                peak_motion: 0.0,
                has_data: false,
            }));
            let stop = Arc::new(AtomicBool::new(false));
            let state_clone = state.clone();
            let stop_clone = stop.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();

            let thread = std::thread::spawn(move || unsafe {
                let hid = IOHIDDeviceCreate(K_CF_ALLOCATOR_DEFAULT, svc);
                IOObjectRelease(svc);
                if hid.is_null() {
                    let _ = ready_tx.send(false);
                    return;
                }
                let kr = IOHIDDeviceOpen(hid, 0);
                if kr != 0 {
                    CFRelease(hid as CFTypeRef);
                    let _ = ready_tx.send(false);
                    return;
                }

                // Stable buffer that won't move — IOKit writes into this
                // pointer from its own internal thread.
                let report_buf = Box::into_raw(
                    vec![0u8; REPORT_BUF_SZ].into_boxed_slice(),
                ) as *mut u8;
                let state_ptr = &*state_clone as *const Mutex<CallbackState>
                    as *mut c_void;

                IOHIDDeviceRegisterInputReportCallback(
                    hid,
                    report_buf,
                    REPORT_BUF_SZ,
                    report_callback,
                    state_ptr,
                );

                let rl = CFRunLoopGetCurrent();
                IOHIDDeviceScheduleWithRunLoop(hid, rl, kCFRunLoopDefaultMode);

                let _ = ready_tx.send(true);

                while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                    CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.25, 0);
                }

                IOHIDDeviceUnscheduleFromRunLoop(hid, rl, kCFRunLoopDefaultMode);
                IOHIDDeviceRegisterInputReportCallback(
                    hid, report_buf, REPORT_BUF_SZ,
                    report_callback_noop, std::ptr::null_mut(),
                );
                IOHIDDeviceClose(hid, 0);
                CFRelease(hid as CFTypeRef);
                // Now safe to free the buffer — callback is unregistered.
                drop(Box::from_raw(std::slice::from_raw_parts_mut(
                    report_buf, REPORT_BUF_SZ,
                )));
            });

            match ready_rx.recv_timeout(Duration::from_secs(2)) {
                Ok(true) => Some(SpuAccel { state, stop, _thread: thread }),
                _ => None,
            }
        }

        pub fn read(&self) -> Option<AccelReading> {
            let mut s = self.state.lock().ok()?;
            if !s.has_data {
                return None;
            }
            let reading = AccelReading {
                sample: s.latest,
                peak_motion: s.peak_motion,
            };
            s.peak_motion = 0.0;
            Some(reading)
        }
    }

    /// No-op callback used to unregister the real callback before cleanup.
    unsafe extern "C" fn report_callback_noop(
        _: *mut c_void, _: IOReturn, _: *mut c_void,
        _: u32, _: u32, _: *mut u8, _: usize,
    ) {}

    use std::sync::atomic::AtomicBool;
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

#[cfg(not(target_os = "macos"))]
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
            let s1 = read_sensor_sample(&device);
            std::thread::sleep(Duration::from_millis(30));
            let s2 = read_sensor_sample(&device);
            if let (Some(sample), Some(sample2)) = (s1, s2) {
                let mag = (sample[0] * sample[0]
                    + sample[1] * sample[1]
                    + sample[2] * sample[2])
                .sqrt();
                // Real accelerometer: ~1g at rest AND data jitters between reads.
                // Static HID endpoints return identical bytes every time.
                let changed = sample[0] != sample2[0]
                    || sample[1] != sample2[1]
                    || sample[2] != sample2[2];
                if mag > 0.7 && mag < 1.5 && changed {
                    return Some(device);
                }
            }
        }
    }

    None
}

#[cfg(not(target_os = "macos"))]
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

#[cfg(not(target_os = "macos"))]
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

