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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

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
        let inner =
            Inner { injectors, mode: Mutex::new(CaptureMode::Local), cursor: Mutex::new(cursor) };
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
        self.inner()?;
        self.started.store(true, Ordering::Release);
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let thread = std::thread::Builder::new()
            .name("oink-linux-input".into())
            .spawn(move || {
                let wake_thread = std::thread::current();
                let wake_thread2 = wake_thread.clone();
                owner.set_wake(Arc::new(move || wake_thread2.unpark()));
                // 本平台无物理输入捕获：路由的本地源一直不可用，注入不受影响。
                owner.capture_available(false);
                while !stopping.load(Ordering::Acquire) {
                    if owner.stopped() {
                        break;
                    }
                    let due = owner.poll(Instant::now()).unwrap_or(Instant::now() + Duration::from_secs(1));
                    let now = Instant::now();
                    if due > now {
                        std::thread::park_timeout(due - now);
                    }
                }
            })
            .map_err(|e| InputError::Platform(e.to_string()))?;
        *wake_holder().lock().unwrap() = Some(thread);
        Ok(())
    }

    fn stop(&self) {
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
        Some(false)
    }

    fn set_mode(&self, mode: CaptureMode) -> Result<(), InputError> {
        let inner = self.inner()?;
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
        InputPermissions {
            accessibility: PermissionState::NotApplicable,
            input_monitoring: PermissionState::NotApplicable,
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
}

/// 捕获线程句柄（stop 时唤醒并回收）。
fn wake_holder() -> &'static Mutex<Option<std::thread::JoinHandle<()>>> {
    static HOLDER: OnceLock<Mutex<Option<std::thread::JoinHandle<()>>>> = OnceLock::new();
    HOLDER.get_or_init(|| Mutex::new(None))
}

