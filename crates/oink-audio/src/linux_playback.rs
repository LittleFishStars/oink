//! Linux output playback via cpal (ALSA host; PulseAudio/PipeWire through the
//! ALSA plugin). The realtime data callback only calls `NativeRender::fill` and
//! `complete` — no logging, allocation or I/O (see the allocation audit tests).

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Device, DeviceId, SampleFormat, Stream, StreamConfig, SupportedBufferSize};
use serde::Serialize;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::playback::NativeRender;

#[derive(Clone, Copy)]
pub struct NativeOutputTiming {
    pub period_frames: u32,
    pub native_gap_frames: Option<u64>,
    pub callback_at: Instant,
    pub playback_at: Option<Instant>,
    pub sample_rate: u32,
    pub channels: u16,
    pub padding_frames: u32,
    pub buffer_frames: u32,
}

#[derive(Clone, Serialize)]
pub struct NativeOutputInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub sample_format: &'static str,
    pub period_frames: u32,
    pub backend: &'static str,
}

/// 选一个 48kHz / stereo / f32 的输出配置。cpal ALSA 可能把请求缓冲帧数
/// 舍入到驱动支持范围；实际值从 config 回填给 timing/info。
fn choose_config(device: &Device, requested: Option<u32>) -> Result<(StreamConfig, SampleFormat), String> {
    let supported = device
        .supported_output_configs()
        .map_err(|e| format!("query output configs: {e}"))?;

    // 优先 48k/stereo/f32；f32 让 data callback 直接把 &[f32] 交给 render。
    for cfg in supported {
        let min = cfg.min_sample_rate();
        let max = cfg.max_sample_rate();
        if cfg.channels() != 2 || cfg.sample_format() != SampleFormat::F32 || !(min..=max).contains(&48_000) {
            continue;
        }
        let mut stream = StreamConfig {
            channels: 2,
            sample_rate: 48_000,
            buffer_size: cpal::BufferSize::Default,
        };
        if let Some(want) = requested {
            stream.buffer_size = match cfg.buffer_size() {
                SupportedBufferSize::Range { min, max } => {
                    cpal::BufferSize::Fixed(want.clamp(*min, *max))
                }
                SupportedBufferSize::Unknown => cpal::BufferSize::Default,
            };
        }
        return Ok((stream, SampleFormat::F32));
    }
    Err("no 48kHz stereo f32 output config".into())
}

fn frames_of(config: &StreamConfig, default: u32) -> u32 {
    match config.buffer_size {
        cpal::BufferSize::Fixed(n) => n,
        cpal::BufferSize::Default => default,
    }
}

pub struct NativePlayback {
    stream: Option<Stream>,
    /// pacat 子进程（虚拟麦克风等 pulse: 后端），普通 cpal 播放为 None。
    pacat: Option<std::process::Child>,
    worker: Option<std::thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    info: NativeOutputInfo,
}

impl NativePlayback {
    pub fn start(
        id: &str,
        requested: Option<u32>,
        mut render: Box<dyn NativeRender>,
        mut on_error: Box<dyn FnMut(String) + Send>,
    ) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));

        // pulse:<sink>：cpal 的 ALSA host 看不到 pactl 逻辑 sink，改用 pacat
        // 子进程播放到指定 sink（虚拟麦克风的写入端）。worker 线程周期性调用
        // render.fill 产出 PCM 写 pacat stdin，复用同一条解码/抖动管线。
        if let Some(sink) = id.strip_prefix("pulse:") {
            return Self::start_pacat(sink, render, on_error, stop);
        }

        let host = cpal::default_host();
        let device = if id.is_empty() || id == "default" {
            host.default_output_device()
                .ok_or("no default output device")?
        } else {
            host.output_devices()
                .map_err(|e| format!("enumerate output devices: {e}"))?
                .find(|d| d.id().ok().is_some_and(|did| device_id_eq(&did, id)))
                .ok_or_else(|| format!("output device not found: {id}"))?
        };

        let (config, format) = choose_config(&device, requested)?;
        let sample_rate = config.sample_rate;
        let channels = config.channels;
        let period = frames_of(&config, 1024);

        // 预分配的 timing scratch 在回调里只更新、不分配；NativeOutputTiming 是 Copy。
        let mut prev_callback: Option<Instant> = None;
        let stream = device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let now = Instant::now();
                    let native_gap = prev_callback.map(|p| now.saturating_duration_since(p).as_secs_f64() * sample_rate as f64 / period as f64).map(|v| v.round() as u64);
                    prev_callback = Some(now);
                    let timing = NativeOutputTiming {
                        period_frames: data.len() as u32 / channels as u32,
                        native_gap_frames: native_gap,
                        callback_at: now,
                        playback_at: None, // cpal/ALSA 无硬件呈现时间戳，绝不伪造
                        sample_rate,
                        channels,
                        padding_frames: 0,
                        buffer_frames: period,
                    };
                    let work_started = Instant::now();
                    render.fill(data, timing);
                    render.complete(work_started.elapsed());
                },
                move |err| on_error(format!("cpal output error: {err}")),
                None,
            )
            .map_err(|e| format!("build output stream: {e}"))?;
        stream.play().map_err(|e| format!("start output stream: {e}"))?;

        Ok(Self {
            stream: Some(stream),
            pacat: None,
            worker: None,
            stop,
            info: NativeOutputInfo {
                sample_rate,
                channels,
                sample_format: match format {
                    SampleFormat::F32 => "f32",
                    _ => "unknown",
                },
                period_frames: period,
                backend: "cpal-alsa",
            },
        })
    }

    /// pacat-backed playback for a named PulseAudio sink (virtual microphone).
    fn start_pacat(
        sink: &str,
        mut render: Box<dyn NativeRender>,
        mut on_error: Box<dyn FnMut(String) + Send>,
        stop: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        use std::io::Write;
        use std::time::Duration;

        const RATE: u32 = 48_000;
        const CHANNELS: u16 = 2;
        const FRAMES: usize = 960; // 20 ms

        let mut child = std::process::Command::new("pacat")
            .args([
                "--playback",
                "--device", sink,
                "--format=float32le", "--rate=48000", "--channels=2", "--latency-msec=20",
            ])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("pacat spawn: {e}"))?;
        let mut stdin = child.stdin.take().ok_or("pacat stdin missing")?;

        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name("oink-vmic-playback".into())
            .spawn(move || {
                let mut pcm = [0.0f32; FRAMES * CHANNELS as usize];
                let mut interval = std::time::Instant::now();
                let mut first_error = true;
                while !worker_stop.load(Ordering::Acquire) {
                    let now = std::time::Instant::now();
                    let timing = NativeOutputTiming {
                        period_frames: FRAMES as u32,
                        native_gap_frames: None,
                        callback_at: now,
                        playback_at: None,
                        sample_rate: RATE,
                        channels: CHANNELS,
                        padding_frames: 0,
                        buffer_frames: FRAMES as u32,
                    };
                    let work_started = std::time::Instant::now();
                    render.fill(&mut pcm, timing);
                    render.complete(work_started.elapsed());
                    let bytes: Vec<u8> = pcm.iter().flat_map(|v| v.to_le_bytes()).collect();
                    if stdin.write_all(&bytes).is_err() {
                        if first_error {
                            first_error = false;
                            on_error("pacat stdin closed".into());
                        }
                        break;
                    }
                    // 20ms 节拍，超限不补帧（与实时回调语义一致，宁可欠载不堆积）。
                    interval += Duration::from_millis(20);
                    let wait = interval.saturating_duration_since(std::time::Instant::now());
                    if wait > Duration::ZERO {
                        std::thread::sleep(wait);
                    } else {
                        interval = std::time::Instant::now();
                    }
                }
            })
            .map_err(|e| format!("spawn pacat playback worker: {e}"))?;

        Ok(Self {
            stream: None,
            pacat: Some(child),
            worker: Some(worker),
            stop,
            info: NativeOutputInfo {
                sample_rate: RATE,
                channels: CHANNELS,
                sample_format: "f32",
                period_frames: FRAMES as u32,
                backend: "pulse-pacat",
            },
        })
    }

    pub fn shutdown(&mut self) -> bool {
        self.stop.store(true, Ordering::Release);
        if let Some(stream) = self.stream.take() {
            let _ = stream.pause();
            drop(stream);
        }
        if let Some(mut child) = self.pacat.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        true
    }

    pub fn info(&self) -> NativeOutputInfo {
        self.info.clone()
    }
}

impl Drop for NativePlayback {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// cpal 的 DeviceId 没有稳定的字符串形态；outputs() 用的是 to_string()，
/// 这里按同样的展示形式比较，兼容 ALSA "hw:CARD=...,DEV=..." 这类 id。
fn device_id_eq(id: &DeviceId, target: &str) -> bool {
    id.to_string() == target
}
