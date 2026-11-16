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
                    if ev.code() == evdev::KeyCode::BTN_TOUCH.0 && ev.value() == 0 {
                        asm.slots.clear();
                        asm.abs_x = None;
                        asm.abs_y = None;
                    }
                }
                evdev::EventType::ABSOLUTE => {
                    // 触摸板本体（event5 Touchpad）上报 ABS_MT 绝对坐标；
                    // 不处理就永远没有 PointerDelta（REL 只来自真鼠标）。
                    match ev.code() {
                        c if c == evdev::AbsoluteAxisCode::ABS_MT_SLOT.0 => asm.slot = ev.value(),
                        c if c == evdev::AbsoluteAxisCode::ABS_MT_POSITION_X.0 => {
                            asm.abs_x = Some(f64::from(ev.value()));
                        }
                        c if c == evdev::AbsoluteAxisCode::ABS_MT_POSITION_Y.0 => {
                            asm.abs_y = Some(f64::from(ev.value()));
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
                    // 一帧结束：先结算触摸板 ABS 差分，再统一上报。
                    asm.absorb_abs_motion(speed);
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
