use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

pub enum DetectorCmd {
    Start { threshold: f32, cooldown_ms: u64 },
    Stop,
}

/// Thread-safe handle to control the slap detector.
pub struct DetectorHandle {
    cmd_tx: mpsc::Sender<DetectorCmd>,
}

impl DetectorHandle {
    /// Spawn the detector on a dedicated thread.
    pub fn spawn(on_slap: impl Fn(f32) + Send + Sync + 'static) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel::<DetectorCmd>();
        let on_slap = Arc::new(on_slap);

        std::thread::spawn(move || {
            let mut stream: Option<cpal::Stream> = None;
            let running = Arc::new(AtomicBool::new(false));

            loop {
                match cmd_rx.recv() {
                    Ok(DetectorCmd::Start {
                        threshold,
                        cooldown_ms,
                    }) => {
                        running.store(false, Ordering::SeqCst);
                        drop(stream.take());

                        match build_stream(
                            threshold,
                            cooldown_ms,
                            running.clone(),
                            on_slap.clone(),
                        ) {
                            Ok(s) => {
                                running.store(true, Ordering::SeqCst);
                                stream = Some(s);
                            }
                            Err(e) => eprintln!("Failed to start detector: {}", e),
                        }
                    }
                    Ok(DetectorCmd::Stop) => {
                        running.store(false, Ordering::SeqCst);
                        drop(stream.take());
                    }
                    Err(_) => break,
                }
            }
        });

        DetectorHandle { cmd_tx }
    }

    pub fn start(&self, threshold: f32, cooldown_ms: u64) {
        self.cmd_tx
            .send(DetectorCmd::Start {
                threshold,
                cooldown_ms,
            })
            .ok();
    }

    pub fn stop(&self) {
        self.cmd_tx.send(DetectorCmd::Stop).ok();
    }
}

fn build_stream(
    threshold: f32,
    cooldown_ms: u64,
    running: Arc<AtomicBool>,
    on_slap: Arc<dyn Fn(f32) + Send + Sync>,
) -> Result<cpal::Stream, String> {
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
        Instant::now() - std::time::Duration::from_secs(10),
    ));

    // Build stream that processes f32 samples (cpal converts internally on WASAPI)
    let stream = device
        .build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if !running.load(Ordering::Relaxed) {
                    return;
                }

                let ch = channels.max(1);
                let count = data.len() / ch;
                if count == 0 {
                    return;
                }

                let sum: f32 = data
                    .iter()
                    .step_by(ch)
                    .map(|s| s * s)
                    .sum();

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

    Ok(stream)
}
