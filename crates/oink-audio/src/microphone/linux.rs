//! Linux virtual microphone via PipeWire/PulseAudio: a null sink paired with a
//! remap-source becomes a virtual input device (`oink_mic`) that applications
//! such as Zoom can select. Peers' microphone audio is played into the null
//! sink; the remap-source exposes it as a capture device.
//!
//! Uses `pactl`/`parec` subprocesses (pipewire-pulse), no FFI — same strategy as
//! `linux_capture.rs`.

use super::{Demand, VirtualOutput};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

const NULL_SINK: &str = "oink_virtual_mic_sink";
const MIC_SOURCE: &str = "oink_mic";
const RECORDING_NAME: &str = "Oink Microphone";

fn run(args: &[&str]) -> Result<String, String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .map_err(|e| format!("pactl {args:?}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "pactl {} failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn source_exists(name: &str) -> bool {
    run(&["list", "short", "sources"])
        .map(|list| list.lines().any(|l| l.split_whitespace().nth(1) == Some(name)))
        .unwrap_or(false)
}

/// Ensure the null sink + remap source pair exists. Idempotent.
pub(super) fn ensure_virtual_mic() -> Result<(), String> {
    if source_exists(MIC_SOURCE) {
        return Ok(());
    }
    run(&["info"]).map_err(|_| "pactl 不可用：请安装 pulseaudio-utils / pipewire-pulse".to_string())?;
    // 清残留 sink（上次 SIGKILL 遗留）
    if let Ok(mods) = run(&["list", "short", "modules"]) {
        for line in mods.lines() {
            if line.contains(NULL_SINK) || line.contains(MIC_SOURCE) {
                if let Some(idx) = line.split_whitespace().next() {
                    let _ = run(&["unload-module", idx]);
                }
            }
        }
    }
    run(&[
        "load-module",
        "module-null-sink",
        &format!("sink_name={NULL_SINK}"),
        &format!("sink_properties=device.description={RECORDING_NAME}"),
    ])?;
    run(&[
        "load-module",
        "module-remap-source",
        &format!("master={NULL_SINK}.monitor"),
        &format!("source_name={MIC_SOURCE}"),
        &format!("source_properties=device.description={RECORDING_NAME}"),
    ])?;
    Ok(())
}

/// Query the virtual input device. Creates it on demand so it is visible to
/// applications. render_id is the null sink (where peer audio is played); the
/// cpal playback backend accepts it as an ALSA/Pulse device id.
pub(super) fn virtual_output() -> Result<VirtualOutput, String> {
    ensure_virtual_mic()?;
    // cpal 的 ALSA host 看不到 pactl 逻辑 sink（只能看到 alsa:pulse 插件），
    // 所以返回带 pulse: 前缀的特殊 id，由 linux_playback 设置 PULSE_SINK
    // 后打开 alsa:pulse，从而定向播放到我们的虚拟 sink。
    Ok(VirtualOutput::Ready {
        render_id: format!("pulse:{NULL_SINK}"),
        recording_name: RECORDING_NAME.into(),
    })
}

/// Whether an application is currently capturing our virtual mic (a
/// source-output attached to `oink_mic` from another process).
fn consumer_active() -> Result<bool, String> {
    let outs = run(&["list", "short", "source-outputs"])?;
    for line in outs.lines() {
        // pactl list short source-outputs: index  source  sample-spec  ...
        if line.split_whitespace().nth(1) == Some(MIC_SOURCE) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Poll-based demand monitor: revision bumps when consumer presence changes.
pub struct DemandMonitor {
    epoch: Arc<AtomicU64>,
    last_active: bool,
}

impl DemandMonitor {
    pub fn new() -> Result<Self, String> {
        Self::with_wake(Arc::new(|| {}))
    }

    pub fn with_wake(_wake: Arc<dyn Fn() + Send + Sync>) -> Result<Self, String> {
        Ok(Self {
            epoch: Arc::new(AtomicU64::new(1)),
            last_active: false,
        })
    }

    pub fn revision(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    pub fn sample(&mut self) -> Demand {
        match consumer_active() {
            Ok(active) => {
                if active != self.last_active {
                    self.last_active = active;
                    self.epoch.fetch_add(1, Ordering::AcqRel);
                }
                Demand {
                    active,
                    epoch: self.revision(),
                    error: None,
                }
            }
            Err(error) => Demand {
                active: false,
                epoch: self.revision(),
                error: Some(error),
            },
        }
    }
}
