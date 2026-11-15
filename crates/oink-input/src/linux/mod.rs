//! Linux（Wayland 合成器，niri 等注入）的 `InputBackend` 实现。
//!
//! 第一阶段只做**被控端**：
//! - 注入：虚拟指针（绝对/相对/按键/滚轮）+ 虚拟键盘（XKB keymap + evdev 码），
//!   全部走标准 wlr 协议扩展（`oink_wayland`），无 root、无内核模块。
//! - 捕获：Wayland 下没有全局物理输入捕获的标准途径，`start*` 报告
//!   `capture_available(false)`（本机暂不能当源），事件循环仍保持运行以驱动路由定时器。
//! - 光标：注入即记账（`cursor_position` 返回最近注入点），与 Windows 后端的
//!   注入目标账本一致；Wayland 客户端读不到全局光标。
//! - 权限：注入不需要系统权限（合成器授权），两个权限位都报 `NotApplicable`。

mod capture;
mod device_loop;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use capture::{CaptureShared, DeviceControl, PointerLedger, discover_devices, set_speed};

use oink_foundation::input::{
    CaptureMode, FeelProfile, InjectEvent, KeyCode, MouseButton, ScrollPhase, ScrollUnit,
};
use oink_foundation::Point;
use oink_wayland::inject::Injectors;
use oink_wayland::Wayland;

use crate::capture_loop::CaptureLoop;
use crate::{InputBackend, InputError, InputPermissions, PermissionState};

/// 鼠标键 evdev BTN_* 码。
fn button_code(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => 0x110,   // BTN_LEFT
        MouseButton::Right => 0x111,  // BTN_RIGHT
        MouseButton::Middle => 0x112, // BTN_MIDDLE
        MouseButton::Back => 0x113,   // BTN_SIDE
        MouseButton::Forward => 0x114, // BTN_EXTRA
        MouseButton::Other(n) => 0x110 + u32::from(n),
    })
}

struct Inner {
    injectors: Injectors,
    mode: Mutex<CaptureMode>,
    /// 最近一次注入位置；注入前用主屏中心兜底（restore_local 的落点）。
    cursor: Mutex<Point>,
    /// 物理输入捕获控制（设备可用性由 capture_allowed 决定）。
    capture: CaptureShared,
    /// 可捕获设备清单（空 = 权限不足或沙箱无 /dev/input）。
    devices: Vec<(std::path::PathBuf, capture::DeviceKind)>,
}

pub struct LinuxInput {
    inner: OnceLock<Inner>,
    wayland: Arc<Wayland>,
    started: AtomicBool,
}

impl std::fmt::Debug for LinuxInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxInput").finish_non_exhaustive()
    }
}

impl LinuxInput {
    /// 连接合成器并创建注入器。失败即无平台后端。
    pub fn connect() -> Result<Self, crate::InputError> {
        let wayland =
            Wayland::connect().map_err(|e| crate::InputError::Platform(e.to_string()))?;
        Ok(Self { inner: OnceLock::new(), wayland, started: AtomicBool::new(false) })
    }

    fn inner(&self) -> Result<&Inner, crate::InputError> {
        if let Some(inner) = self.inner.get() {
            return Ok(inner);
        }
        let injectors =
            Injectors::connect(self.wayland.clone()).map_err(|e| crate::InputError::Platform(e.to_string()))?;
        let bounds = self.wayland.global_bounds();
        let cursor = match bounds {
            Some((x0, y0, x1, y1)) => Point::new(((x0 + x1) / 2) as f64, ((y0 + y1) / 2) as f64),
            None => Point::new(0., 0.),
        };
        let (ledger, control, grabbed, devices) = {
            let control = Arc::new(DeviceControl::default());
            set_speed(&control.mouse_speed, 1.0);
            set_speed(&control.touchpad_speed, 1.0);
            control.natural_touchpad.store(natural_touchpad(), Ordering::Relaxed);
            let ledger = PointerLedger::new(cursor);
            if let Some((x0, y0, x1, y1)) = self.wayland.global_bounds() {
                ledger.set_bounds(x0 as f64, y0 as f64, x1 as f64, y1 as f64);
            }
            let grabbed = Arc::new(AtomicBool::new(false));
            let devices = discover_devices(Path::new("/dev/input"));
            (ledger, control, grabbed, devices)
        };
        let inner = Inner {
            injectors,
            mode: Mutex::new(CaptureMode::Local),
            cursor: Mutex::new(cursor),
            capture: CaptureShared {
                control,
                ledger,
                grabbed,
            },
            devices,
        };
        let _ = self.inner.set(inner);
        Ok(self.inner.get().unwrap())
    }

    fn wayland(&self) -> &Arc<Wayland> {
        &self.wayland
    }
}

impl InputBackend for LinuxInput {
    fn start(&self, _handler: crate::EventHandler) -> Result<(), InputError> {
        // 无物理输入捕获；占位成功，让后端可以装配（capture 状态见 capture_available）。
        self.inner()?;
        self.started.store(true, Ordering::Release);
        Ok(())
    }

    fn start_loop(&self, mut owner: Box<dyn CaptureLoop>) -> Result<(), InputError> {
        let inner = self.inner()?;
        self.started.store(true, Ordering::Release);
        let (tx, rx) = std::sync::mpsc::channel::<oink_foundation::input::CapturedEvent>();
        let control = inner.capture.control.clone();
        let can_capture = !inner.devices.is_empty();
        // 热插拔上下文：蓝牙 HID 空闲断连时 evdev 节点消失、重连时在
        // /dev/input 下重建——启动时的一次性枚举永远等不到重连后的
        // 新节点（无线/蓝牙鼠标的必经之路）。owner 循环周期性对比
        // discover_devices 与已知集合，新设备自动接入捕获。
        let hp_tx = tx.clone();
        let hp_control = control.clone();
        let hp_ledger = inner.capture.ledger.clone();
        let hp_grabbed = inner.capture.grabbed.clone();
        let hp_known: std::collections::HashSet<std::path::PathBuf> =
            inner.devices.iter().map(|(p, _)| p.clone()).collect();
        // start_loop 返回前必须同步安装 wake：InputThread::send 在启动序列里
        // 会立即 wake()()（OnceLock::get().expect），异步安装会留下竞态
        // （release 构建的调度差异让 observers 抢先于捕获线程）。
        {
            let wake_thread = std::thread::current();
            let wake_thread2 = wake_thread.clone();
            owner.set_wake(Arc::new(move || wake_thread2.unpark()));
            owner.capture_available(can_capture);
        }
        let owner_thread = std::thread::Builder::new()
            .name("oink-linux-input".into())
            .spawn(move || {
                let mut known = hp_known;
                let mut last_scan = Instant::now();
                while !control.stop.load(Ordering::Acquire) {
                    // 设备线程的事件串行喂给 owner（CaptureLoop 要求单线程调用）。
                    while let Ok(event) = rx.try_recv() {
                        owner.event(&event);
                    }
                    if owner.stopped() {
                        break;
                    }
                    if last_scan.elapsed() >= Duration::from_secs(2) {
                        last_scan = Instant::now();
                        let found = capture::discover_devices(Path::new("/dev/input"));
                        // 心跳：证明扫描在跑 + 报告当前可见设备数（蓝牙鼠标
                        // 休眠时节点消失，重连后这里必须能看到它）。
                        static LAST_BEAT: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        let now_ms = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| u64::try_from(d.as_millis()).unwrap_or(0))
                            .unwrap_or(0);
                        let last_beat = LAST_BEAT.load(std::sync::atomic::Ordering::Relaxed);
                        if now_ms.saturating_sub(last_beat) >= 30_000 {
                            LAST_BEAT.store(now_ms, std::sync::atomic::Ordering::Relaxed);
                            tracing::info!(
                                target: "oink_gap",
                                visible = found.len(),
                                known = known.len(),
                                "evdev 热插拔扫描心跳（30s 一条）"
                            );
                        }
                        for (path, kind) in found {
                            if known.insert(path.clone()) {
                                tracing::info!(
                                    target: "oink_gap",
                                    ?path,
                                    ?kind,
                                    "evdev 热插拔：新设备接入捕获"
                                );
                                let shared = CaptureShared {
                                    control: hp_control.clone(),
                                    ledger: hp_ledger.clone(),
                                    grabbed: hp_grabbed.clone(),
                                };
                                let hp_tx = hp_tx.clone();
                                let sink: device_loop::Sink = Arc::new(move |event| {
                                    let _ = hp_tx.send(event.clone());
                                });
                                let _ = std::thread::Builder::new()
                                    .name(format!(
                                        "oink-evdev-{}",
                                        path.file_name().unwrap_or_default().to_string_lossy()
                                    ))
                                    .spawn(move || device_loop::device_thread(path, kind, shared, sink));
                            }
                        }
                    }
                    let due = owner
                        .poll(Instant::now())
                        .unwrap_or(Instant::now() + Duration::from_millis(50));
                    let now = Instant::now();
                    if due > now {
                        std::thread::park_timeout((due - now).min(Duration::from_millis(20)));
                    }
                }
            })
            .map_err(|e| InputError::Platform(e.to_string()))?;
        *wake_holder().lock().unwrap() = Some(owner_thread);

        // 设备线程：每个可捕获物理设备一个（Local 不 grab 不上报，Remote grab 并上报）。
        if can_capture {
            let sink: device_loop::Sink = Arc::new(move |event| {
                let _ = tx.send(event.clone());
            });
            for (path, kind) in inner.devices.clone() {
                let shared = CaptureShared {
                    control: inner.capture.control.clone(),
                    ledger: inner.capture.ledger.clone(),
                    grabbed: inner.capture.grabbed.clone(),
                };
                let sink = sink.clone();
                let _ = std::thread::Builder::new()
                    .name(format!("oink-evdev-{}", path.file_name().unwrap_or_default().to_string_lossy()))
                    .spawn(move || device_loop::device_thread(path, kind, shared, sink));
            }
            for (path, kind) in &inner.devices {
                tracing::info!(target: "oink_gap", ?path, ?kind, "evdev 设备入捕获列表");
            }
            tracing::info!(count = inner.devices.len(), "evdev 捕获设备线程就绪");
        } else {
            tracing::warn!("无可用 evdev 设备或权限不足：本机只作为被控端（加入 input 组即可当源机）");
        }
        Ok(())
    }

    fn stop(&self) {
        if let Some(inner) = self.inner.get() {
            inner.capture.control.stop.store(true, Ordering::Release);
        }
        // 唤醒并结束捕获线程；注入器随进程退出清理。
        if let Some(handle) = wake_holder().lock().unwrap().take() {
            handle.thread().unpark();
            let _ = handle.join();
        }
        if let Some(inner) = self.inner.get() {
            inner.injectors.keyboard.release_all();
        }
    }

    fn capture_active(&self) -> Option<bool> {
        let inner = self.inner.get()?;
        Some(!inner.devices.is_empty() && inner.capture.grabbed.load(Ordering::Acquire))
    }

    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        let inner = self.inner()?;
        // 刷新外接矩形（插拔/改分辨率后）：虚拟光标钳位与真实光标一致。
        if let Some((x0, y0, x1, y1)) = self.wayland.global_bounds() {
            inner.capture.ledger.set_bounds(x0 as f64, y0 as f64, x1 as f64, y1 as f64);
        }
        let remote = matches!(mode, CaptureMode::Remote { .. });
        inner.capture.control.remote.store(remote, Ordering::Release);
        if let CaptureMode::Remote { park } = mode {
            *inner.capture.ledger.park.lock().unwrap() = Some(park);
        } else {
            *inner.capture.ledger.park.lock().unwrap() = None;
        }
        *inner.mode.lock().unwrap() = mode;
        Ok(())
    }

    fn inject(&self, event: &InjectEvent) -> Result<(), InputError> {
        let inner = self.inner()?;
        let injectors = &inner.injectors;
        match event {
            InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } => {
                injectors.pointer.move_to(pos.x, pos.y);
                let mut cursor = inner.cursor.lock().unwrap();
                *cursor = *pos;
                inner.capture.ledger.place(*pos);
            }
            InjectEvent::Button { button, pressed } => {
                let Some(code) = button_code(*button) else { return Ok(()) };
                injectors.pointer.button(code, *pressed);
            }
            InjectEvent::Scroll { dx, dy, unit, phase, .. } => {
                // Line 单位按格数离散；Pixel 单位连续。
                let discrete = match unit {
                    ScrollUnit::Line => Some(*dy),
                    ScrollUnit::Pixel => None,
                };
                injectors.pointer.scroll(*dx, *dy, discrete);
                if matches!(phase, ScrollPhase::Ended | ScrollPhase::Cancelled) {
                    // 连续流结束：发一次零位移的 stop 帧（合成器按 axis_stop 处理惯性）。
                    injectors.pointer.scroll(0.0, 0.0, None);
                }
            }
            InjectEvent::Key { code, pressed } => {
                if *code == KeyCode::MENU_MASK {
                    return Ok(()); // Windows 私有菜单遮罩键，无 Linux 对应
                }
                let Some(evdev) = oink_wayland::keymap::hid_to_evdev(*code) else { return Ok(()) };
                injectors.keyboard.key(evdev, *pressed);
            }
            InjectEvent::MoveRelative { dx, dy } => {
                injectors.pointer.move_relative(*dx, *dy);
                let mut cursor = inner.cursor.lock().unwrap();
                cursor.x += dx;
                cursor.y += dy;
                inner.capture.ledger.advance(*dx, *dy);
            }
            InjectEvent::TouchpadScroll { dx, dy, wheel, .. } => {
                let discrete = match wheel.unit {
                    ScrollUnit::Line => Some(wheel.dy),
                    ScrollUnit::Pixel => None,
                };
                injectors.pointer.scroll(*dx, *dy, discrete);
            }
            // 与 Key 相同，但不带 Win+L 补偿语义（Linux 不需要）。
            InjectEvent::KeyWithoutLock { code, pressed } => {
                if let Some(evdev) = oink_wayland::keymap::hid_to_evdev(*code) {
                    injectors.keyboard.key(evdev, *pressed);
                }
            }
            InjectEvent::TouchpadContacts(_) | InjectEvent::Gesture(_) | InjectEvent::Volume { .. } => {}
        }
        Ok(())
    }

    fn cursor_position(&self) -> Result<Point, InputError> {
        let inner = self.inner()?;
        Ok(*inner.cursor.lock().unwrap())
    }

    fn cursor_injection_target(&self, event: &InjectEvent) -> Option<(Point, f64)> {
        match event {
            InjectEvent::MoveTo { pos } | InjectEvent::MoveToWithDelta { pos, .. } => {
                Some((*pos, 0.))
            }
            _ => None,
        }
    }

    fn feel_profile(&self) -> FeelProfile {
        // niri 默认 25/s 重复；XKB 默认 delay 660ms、rate 25/s。
        FeelProfile {
            key_repeat_delay_ms: 660,
            key_repeat_interval_ms: 40,
            wheel_lines_per_notch: None,
            natural_scroll_mouse: false,
            natural_scroll_touchpad: false,
        }
    }

    fn permissions(&self) -> InputPermissions {
        let has_devices = self.inner.get().is_some_and(|i| !i.devices.is_empty());
        // 注入不需要系统权限；当源机需要能读 /dev/input（input 组）。
        let state = if has_devices { PermissionState::NotApplicable } else { PermissionState::Denied };
        InputPermissions {
            accessibility: state,
            input_monitoring: state,
            secure_input_active: false,
        }
    }

    fn declare_user_activity(&self) -> Result<bool, InputError> {
        Ok(true)
    }

    fn reset_user_idle(&self) -> Result<bool, InputError> {
        // 注入本身即活动；Wayland 无标准空闲重置接口。
        Ok(true)
    }

    fn injection_available(&self) -> Option<bool> {
        Some(self.wayland().has_virtual_pointer())
    }

    fn mark_dnd_native_available(&self) {
        if let Ok(inner) = self.inner() {
            inner
                .injectors
                .dnd_native_available
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }

    fn cursor_readback_needed(&self) -> bool {
        false
    }
    fn dnd_native_available(&self) -> bool {
        self.inner()
            .map(|inner| {
                inner.injectors.dnd_native_available.load(std::sync::atomic::Ordering::Acquire)
            })
            .unwrap_or(false)
    }
}

/// 读 niri 配置里的触摸板自然滚动开关（非 niri 环境回退 false）。
fn natural_touchpad() -> bool {
    let Ok(home) = std::env::var("HOME") else { return false };
    let Ok(text) = std::fs::read_to_string(format!("{home}/.config/niri/config.kdl")) else { return false };
    // 只看 touchpad 块内的 natural-scroll（简单括号深度扫描）。
    let mut depth = 0i32;
    let mut in_touchpad = false;
    for line in text.lines() {
        let trimmed = line.split("//").next().unwrap_or("").trim();
        if in_touchpad && trimmed == "natural-scroll" {
            return true;
        }
        if trimmed.starts_with("touchpad") && trimmed.contains('{') {
            in_touchpad = true;
            depth = 1;
            continue;
        }
        if in_touchpad {
            for ch in trimmed.chars() {
                if ch == '{' { depth += 1; }
                if ch == '}' {
                    depth -= 1;
                    if depth <= 0 { in_touchpad = false; }
                }
            }
        }
    }
    false
}

/// 捕获线程句柄（stop 时唤醒并回收）。
fn wake_holder() -> &'static Mutex<Option<std::thread::JoinHandle<()>>> {
    static HOLDER: OnceLock<Mutex<Option<std::thread::JoinHandle<()>>>> = OnceLock::new();
    HOLDER.get_or_init(|| Mutex::new(None))
}

