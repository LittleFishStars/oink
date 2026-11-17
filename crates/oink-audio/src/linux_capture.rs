//! Linux system-output capture via PulseAudio/PipeWire `parec` on a sink
//! monitor. No FFI: we spawn `pactl`/`parec` (provided by pipewire-pulse).
//!
//! When `mute` is requested (normal forwarding), we route the default output
//! through a temporary null sink so the physical speakers stay silent, then
//! capture that sink's monitor. When `mute` is false (verify probe) we capture
//! the current default sink's monitor without touching routing.
//!
//! RAII: dropping Capture kills `parec`, unloads our null-sink module and
//! restores the original default sink. Crash residue is bounded by a fixed sink
//! name that is cleaned up on next start.

use crate::source::{Encoder, Stamp};
use crate::{CaptureInfo, media_pause::ForwardingPause};
use std::io::{BufReader, Read};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Instant;

const NULL_SINK_NAME: &str = "oink_capture_sink";
/// 20 ms of stereo f32 @48k = 960 frames = 1920 f32 = 7680 bytes.
const FRAME_F32: usize = 1920;
const BYTE_LEN: usize = FRAME_F32 * 4;

#[derive(Debug)]
pub struct Capture {
    child: Option<Child>,
    worker: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    error: Arc<Mutex<Option<String>>>,
    /// Our null-sink module index (Some only when we redirected routing).
    module: Option<String>,
    /// Original default sink to restore (Some only when we changed it).
    orig_sink: Option<String>,
    info: CaptureInfo,
}

fn run(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| format!("pactl {:?}: {args:?}", e))?;
    if !out.status.success() {
        return Err(format!(
            "pactl {:?} failed: {}",
            args[0],
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Remove a leftover null sink from a previous killed instance.
fn cleanup_stale() {
    let Ok(list) = run(&["list", "short", "modules"]) else {
        return;
    };
    for line in list.lines() {
        if line.contains(NULL_SINK_NAME) {
            let idx = line.split_whitespace().next().unwrap_or("");
            if !idx.is_empty() {
                let _ = run(&["unload-module", idx]);
            }
        }
    }
}

impl Capture {
    pub fn start_encoded(encoder: Encoder, mute: bool, _dir: std::path::PathBuf) -> Result<Self, String> {
        // pactl 可达性先探测；失败给出明确的安装提示。
        run(&["info"]).map_err(|_| "pactl 不可用：请安装 pulseaudio-utils / pipewire-pulse".to_string())?;
        cleanup_stale();

        let mut module = None;
        let mut orig_sink = None;
        let monitor = if mute {
            let orig = run(&["get-default-sink"])?;
            if orig.is_empty() {
                return Err("no default sink".into());
            }
            let idx = run(&[
                "load-module",
                "module-null-sink",
                &format!("sink_name={NULL_SINK_NAME}"),
                &format!("sink_properties=device.description={NULL_SINK_NAME}"),
            ])?;
            if let Err(e) = run(&["set-default-sink", NULL_SINK_NAME]) {
                let _ = run(&["unload-module", &idx]);
                return Err(e);
            }
            // 把在播流也迁过去（PipeWire 通常自动迁移，显式处理更稳妥）。
            if let Ok(inputs) = run(&["list", "short", "sink-inputs"]) {
                for line in inputs.lines() {
                    if let Some(id) = line.split_whitespace().next() {
                        let _ = run(&["move-sink-input", id, NULL_SINK_NAME]);
                    }
                }
            }
            module = Some(idx);
            orig_sink = Some(orig);
            format!("{NULL_SINK_NAME}.monitor")
        } else {
            let orig = run(&["get-default-sink"])?;
            if orig.is_empty() {
                return Err("no default sink".into());
            }
            format!("{orig}.monitor")
        };

        let mut child = Command::new("parec")
            .args([
                "--device", &monitor,
                "--format=float32le", "--rate=48000", "--channels=2", "--latency-msec=20",
            ])
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| {
                // 启动失败也要撤销已做的路由改动
                if let Some(idx) = &module {
                    let _ = run(&["unload-module", idx]);
                }
                if let Some(orig) = &orig_sink {
                    let _ = run(&["set-default-sink", orig]);
                }
                format!("parec spawn: {e}")
            })?;
        let stdout = child.stdout.take().ok_or("parec stdout missing")?;

        let stop = Arc::new(AtomicBool::new(false));
        let error = Arc::new(Mutex::new(None));
        let worker_stop = stop.clone();
        let worker_error = error.clone();
        let worker = std::thread::Builder::new()
            .name("oink-linux-capture".into())
            .spawn(move || {
                let mut reader = BufReader::new(stdout);
                let mut bytes = [0u8; BYTE_LEN];
                let mut encoder = encoder;
                let mut first_error = true;
                while !worker_stop.load(Ordering::Acquire) {
                    match reader.read_exact(&mut bytes) {
                        Ok(()) => {
                            let samples: Vec<f32> = bytes
                                .chunks_exact(4)
                                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                                .collect();
                            let stamp = Stamp {
                                first_sample_at: None,
                                callback_at: None,
                                delivered_at: Instant::now(),
                                source_frame_position: None,
                                discontinuity: false,
                            };
                            encoder.supply(&samples, stamp);
                            encoder.tick(Instant::now());
                        }
                        Err(e) => {
                            if worker_stop.load(Ordering::Acquire) {
                                break;
                            }
                            if first_error {
                                first_error = false;
                                *worker_error.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(format!("parec read: {e}"));
                            }
                            break;
                        }
                    }
                }
            })
            .map_err(|e| format!("spawn capture worker: {e}"))?;

        let info = CaptureInfo {
            backend: "pipewire-pulse-null-sink-monitor".into(),
            sample_rate: 48_000,
            actual_buffer_frames: FRAME_F32 as u32 / 2,
            timestamp_source: "delivery".into(),
            physical_output_id: Some(monitor),
            ..Default::default()
        };

        Ok(Self {
            child: Some(child),
            worker: Some(worker),
            stop,
            error,
            module,
            orig_sink,
            info,
        })
    }

    pub fn info(&self) -> CaptureInfo {
        self.info.clone()
    }

    pub fn error(&self) -> Option<String> {
        self.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn source_muted(&self) -> bool {
        self.module.is_some()
    }

    pub fn set_forwarding_pause(&self, _: Option<ForwardingPause>) {}
    pub fn default_output_changed(&self) -> bool {
        false
    }
    pub fn local_controls_changed(&self) -> bool {
        false
    }
    pub fn restart_reason(&self) -> Option<String> {
        None
    }

    /// 停止采集并恢复系统路由；返回清理过程中的错误（若有）。
    pub fn finish(&mut self) -> Option<String> {
        self.stop.store(true, Ordering::Release);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        let mut problem = None;
        if let Some(idx) = self.module.take() {
            if let Err(e) = run(&["unload-module", &idx]) {
                problem = Some(e);
            }
        }
        if let Some(orig) = self.orig_sink.take() {
            if let Err(e) = run(&["set-default-sink", &orig]) {
                problem = Some(problem.unwrap_or(e));
            }
        }
        problem
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        if let Some(idx) = self.module.take() {
            let _ = run(&["unload-module", &idx]);
        }
        if let Some(orig) = self.orig_sink.take() {
            let _ = run(&["set-default-sink", &orig]);
        }
    }
}
