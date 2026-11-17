//! evdev 设备线程：把物理输入组装成 `CapturedEvent` 并驱动路由。

use std::sync::Arc;
use std::time::{Duration, Instant};

use oink_foundation::input::{CapturedEvent, InputEvent, ScrollPhase, ScrollUnit, SourceKind};
use oink_foundation::Point;

use super::capture::{
    CaptureShared, DeviceKind, EventAssembler, RelAxis, button_from_evdev, rel_axis_from_evdev,
    scroll_dy, speed_of,
};

/// 回调：设备线程在事件组装完后同步调用（与 `EventHandler` 一致的形态）。
pub type Sink = Arc<dyn Fn(&CapturedEvent) + Send + Sync>;

/// Stable nonzero device tag from its /dev/input path (for contact samples).
fn path_id(path: &std::path::Path) -> u32 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() as u32).max(1)
}

/// 每个设备的处理线程。Local 时只观察光标位移（不 grab、不上报）；
/// Remote 时 grab 设备并上报全部事件。
pub fn device_thread(
    path: std::path::PathBuf,
    kind: DeviceKind,
    shared: CaptureShared,
    sink: Sink,
) {
    let mut device = match evdev::Device::open(&path) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(path = %path.display(), %e, "打开 evdev 设备失败");
            return;
        }
    };
    if device.set_nonblocking(true).is_err() {
        tracing::warn!(path = %path.display(), "无法设置 evdev 非阻塞");
        return;
    }
    let mut grabbed = false;
    let mut asm = EventAssembler::default();
    // 多指手势识别（仅触摸板用到；真鼠标/键盘不产生 ABS_MT）。
    let mut gesture = crate::linux::gesture::GestureRecognizer::default();
    // 触点前导：接收端（Windows）按是否有 contacts 决定手势注入路径；
    // 只有 Gesture 没有 contacts 时会被 Native path 过滤掉。
    let mut last_fingers = 0usize;
    let mut frame = Vec::new();
    let control = shared.control.clone();
    let ledger = shared.ledger.clone();

    while !control.stop.load(std::sync::atomic::Ordering::Acquire) {
        let remote = control.remote.load(std::sync::atomic::Ordering::Acquire);
        // 模式切换：进入 Remote 时 grab；回 Local 时 ungrab。
        if remote != grabbed {
            if remote {
                if let Err(e) = device.grab() {
                    tracing::warn!(path = %path.display(), %e, "evdev grab 失败（需要 input 组权限？）");
                    shared.grabbed.store(false, std::sync::atomic::Ordering::Release);
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
                shared.grabbed.store(true, std::sync::atomic::Ordering::Release);
            } else {
                let _ = device.ungrab();
            }
            grabbed = remote;
        }

        // Local 也要持续读取：路由层需要物理事件做屏幕边缘穿越判定
        // （源机把鼠标推向另一台设备的方向时移交焦点）。事件不 grab、
        // 合成器照常处理，捕获层只观察。Remote 才 grab 并真正接管。
        let events = match device.fetch_events() {
            Ok(it) => it.collect::<Vec<_>>(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(4));
                continue;
            }
            Err(e) => {
                if e.raw_os_error() == Some(19) {
                    // ENODEV：蓝牙 HID 空闲断连等正常消失——热插拔扫描会在
                    // 设备重连时重新接入，不算故障。
                    tracing::info!(path = %path.display(), "evdev 设备已断连，设备线程退出");
                } else {
                    tracing::warn!(path = %path.display(), %e, "evdev 读取失败，设备线程退出");
                }
                return;
            }
        };

        let remote_now = remote;
        let speed = match kind {
            DeviceKind::Mouse => speed_of(&control.mouse_speed),
            DeviceKind::Touchpad => speed_of(&control.touchpad_speed),
            DeviceKind::Keyboard => 1.0,
        };
        let natural = control.natural_touchpad.load(std::sync::atomic::Ordering::Acquire)
            && kind == DeviceKind::Touchpad;

        for ev in events {
            match ev.event_type() {
                evdev::EventType::KEY if remote_now => {
                    let code = ev.code();
                    // evdev KEY value: 0=抬起 1=按下 2=自动重复。
                    // 重复的语义是「仍处于按下」，绝不能当成抬起——否则按住修饰键
                    // 期间的内核重复会让对端提前 KEYUP，组合键（Ctrl+C 等）失效。
                    // router 的 capture_reports_key_repeat=true 也要求重复按按下上报。
                    let pressed = ev.value() != 0;
                    if let Some(button) = button_from_evdev(code) {
                        frame.push(InputEvent::Button { button, pressed });
                    } else if let Some(key) = oink_wayland::keymap::evdev_to_hid(u32::from(code)) {
                        frame.push(InputEvent::Key { code: key, pressed });
                    }
                }
                evdev::EventType::KEY => {
                    // 触摸板抬起（BTN_TOUCH=0）：清 slot 锚点，下次按下
                    // 重新锚定，避免把按下位置与抬起位置的差当位移。
                    // 同时清空多指状态——不少驱动抬指只发 BTN_TOUCH 而不发
                    // ABS_MT_TRACKING_ID=-1，不清会让 fingers 永久残留、
                    // 手指数虚高导致手势状态卡死。
                    if ev.code() == evdev::KeyCode::BTN_TOUCH.0 && ev.value() == 0 {
                        asm.slots.clear();
                        asm.abs_x = None;
                        asm.abs_y = None;
                        asm.tracking.clear();
                        asm.fingers.clear();
                        asm.pending_x.clear();
                        asm.pending_y.clear();
                        asm.tracking_changes.clear();
                        for g in gesture.finish(oink_foundation::input::GesturePhase::Ended) {
                            if remote_now {
                                frame.push(InputEvent::Gesture(g));
                            }
                        }
                    }
                }
                evdev::EventType::ABSOLUTE => {
                    // 触摸板本体（event5 Touchpad）上报 ABS_MT 绝对坐标；
                    // 不处理就永远没有 PointerDelta（REL 只来自真鼠标）。
                    match ev.code() {
                        c if c == evdev::AbsoluteAxisCode::ABS_MT_SLOT.0 => asm.slot = ev.value(),
                        c if c == evdev::AbsoluteAxisCode::ABS_MT_POSITION_X.0 => {
                            let x = f64::from(ev.value());
                            asm.abs_x = Some(x);
                            asm.pending_x.insert(asm.slot, x);
                        }
                        c if c == evdev::AbsoluteAxisCode::ABS_MT_POSITION_Y.0 => {
                            let y = f64::from(ev.value());
                            asm.abs_y = Some(y);
                            asm.pending_y.insert(asm.slot, y);
                        }
                        // TRACKING_ID 是手指身份：-1 表示该 slot 抬起，>=0 是新增。
                        c if c == evdev::AbsoluteAxisCode::ABS_MT_TRACKING_ID.0 => {
                            asm.tracking_changes.push((asm.slot, ev.value()));
                        }
                        _ => {}
                    }
                }
                evdev::EventType::RELATIVE => {
                    let value = f64::from(ev.value());
                    match rel_axis_from_evdev(ev.code()) {
                        Some(RelAxis::X) => asm.dx += value * speed,
                        Some(RelAxis::Y) => asm.dy += value * speed,
                        Some(RelAxis::Wheel) => asm.wheel += value,
                        Some(RelAxis::HWheel) => asm.hwheel += value,
                        Some(RelAxis::WheelHiRes) => asm.wheel += value / 120.0,
                        Some(RelAxis::HWheelHiRes) => asm.hwheel += value / 120.0,
                        None => {}
                    }
                }
                evdev::EventType::SYNCHRONIZATION => {
                    // 一帧结束：先结算多指状态，再处理手势或普通位移。
                    asm.commit_fingers();
                    let fingers = asm.finger_count();
                    let centroid = asm.centroid();
                    let distance = asm.pinch_distance();
                    let outcome = gesture.update(fingers, centroid, distance);
                    let mut gesture_events = Vec::new();
                    let gesture_active = gesture.active();
                    match outcome {
                        crate::linux::gesture::Outcome::Events(events) => gesture_events = events,
                        crate::linux::gesture::Outcome::PassThrough => {}
                    }
                    // 触点前导：0→有手指时发一次 Boundary，随后每帧发 Sample。
                    // 这让 Windows 接收端走对手势注入路径（见 contacts.rs filter_multifinger）。
                    if remote_now {
                        if last_fingers == 0 && fingers > 0 {
                            frame.push(InputEvent::TouchpadBoundary);
                        }
                        if fingers > 0 {
                            if let Some(sample) = crate::linux::gesture::contact_sample(
                                path_id(&path), &asm.fingers, ledger.time_us(Instant::now()),
                            ) {
                                frame.push(InputEvent::TouchpadSample(sample));
                            }
                        }
                    }
                    last_fingers = fingers;
                    // 手势只在控制对端（remote）时转发；本机自用时不发，
                    // 避免把用户本地的触摸板手势变成远端动作。
                    if remote_now {
                        for g in gesture_events {
                            frame.push(InputEvent::Gesture(g));
                        }
                    }
                    // 手势进行中时，这一帧的位移属于手势，不再移动光标。
                    if !(gesture_active && remote_now) {
                        asm.absorb_abs_motion(speed);
                    } else {
                        asm.abs_x = None;
                        asm.abs_y = None;
                    }
                    if asm.dx != 0.0 || asm.dy != 0.0 {
                        let pos = ledger.advance(asm.dx, asm.dy);
                        // PointerDelta 是路由的正常移动通道：Local（本机作为源机
                        // 推向另一台设备边缘）与 Remote 都要上报——边缘穿越判定
                        // （local_motion → gap_local_motion）只认 PointerDelta，
                        // 漏发会让本机鼠标永远穿不过去（Linux 移植遗漏）。
                        // RawPointerDelta 是游戏模式的相对观察流，另算。
                        frame.push(InputEvent::PointerDelta { dx: asm.dx, dy: asm.dy });
                        frame.push(InputEvent::RawPointerDelta { dx: asm.dx, dy: asm.dy });
                        // 两者都带 cursor（ledger 虚拟光标）：Wayland 读不到全局
                        // 光标，ledger 是唯一的绝对位置来源，local_motion 无 cursor
                        // 直接 Pass。
                    }
                    if remote_now && (asm.wheel != 0.0 || asm.hwheel != 0.0) {
                        frame.push(InputEvent::Scroll {
                            dx: -asm.hwheel,
                            dy: scroll_dy(asm.wheel, natural),
                            unit: ScrollUnit::Line,
                            phase: ScrollPhase::None,
                            wheel_steps: Some(Point::new(asm.hwheel, asm.wheel)),
                        });
                    }
                    asm.dx = 0.0;
                    asm.dy = 0.0;
                    asm.wheel = 0.0;
                    asm.hwheel = 0.0;
                    for event in frame.drain(..) {
                        sink(&CapturedEvent {
                            physical: true,
                            event,
                            source: match kind {
                                DeviceKind::Keyboard => SourceKind::Keyboard,
                                DeviceKind::Mouse => SourceKind::Mouse,
                                DeviceKind::Touchpad => SourceKind::Touchpad,
                            },
                            time_us: ledger.time_us(Instant::now()),
                            cursor: Some(ledger.position()),
                        });
                    }
                }
                _ => {}
            }
        }
        if !frame.is_empty() {
            // SYN 缺失的驱动（少见）：按已收集事件上报，避免积压。
            for event in frame.drain(..) {
                sink(&CapturedEvent {
                    physical: true,
                    event,
                    source: SourceKind::Unknown,
                    time_us: ledger.time_us(Instant::now()),
                    cursor: Some(ledger.position()),
                });
            }
        }
    }
    if grabbed {
        let _ = device.ungrab();
    }
}
