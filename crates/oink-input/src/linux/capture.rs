//! evdev 物理输入捕获（Linux 作为**源机/主控端**）。
//!
//! Wayland 客户端拿不到全局物理输入，因此走内核 evdev：枚举 `/dev/input/event*`，
//! 按能力分类键盘/鼠标/触摸板，`EVIOCGRAB` 独占读取，翻译成 `CapturedEvent` 交给路由。
//!
//! 模式语义与 macOS/Windows 后端对齐：
//! - `Local`：不 grab，本机照常处理；只更新虚拟光标位置，不上报事件。
//! - `Remote`：grab 全部设备（合成器与本机应用都收不到 → 本机光标天然冻结、无悬停），
//!   相对位移按速度系数换算成 `PointerDelta` 上报，滚轮按源机自然滚动方向约定换算。
//!
//! 本机光标：Wayland 读不到，捕获层自己维护一个**虚拟光标**（初值取主屏中心，
//! 随本地移动/远端注入更新），用于 `cursor_position` 与 `restore_local`。
//!
//! 权限：打开/grab `/dev/input/*` 需要用户在 `input` 组；缺失时
//! [`permissions`](InputBackend::permissions) 报 `Denied`，捕获不启动（仍可被控）。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use oink_foundation::input::MouseButton;
use oink_foundation::Point;

/// 打开 `input_backend` 时的设备角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Keyboard,
    Mouse,
    Touchpad,
}

/// 由设备能力判定角色。规则与 libinput/桌面惯例一致：
/// 有 REL_X 的指针设备是鼠标；多点 + POINTER 属性的是触摸板；
/// 能输出大量字母键且不是指针设备的是键盘。
pub fn classify(supports: &DeviceCaps) -> Option<DeviceKind> {
    if supports.is_pointer {
        if supports.relative_xy {
            return Some(DeviceKind::Mouse);
        }
        if supports.touch_multi {
            return Some(DeviceKind::Touchpad);
        }
    }
    if supports.alphabet && !supports.relative_xy && !supports.touch_multi {
        return Some(DeviceKind::Keyboard);
    }
    None
}

/// 判定用的能力摘要（一次读取，避免把 evdev::Device 带进单测）。
#[derive(Clone, Copy, Debug, Default)]
pub struct DeviceCaps {
    pub is_pointer: bool,
    pub relative_xy: bool,
    pub touch_multi: bool,
    pub alphabet: bool,
    pub scroll_wheel: bool,
}

/// evdev BTN_* → oink 鼠标键。
pub fn button_from_evdev(code: u16) -> Option<MouseButton> {
    Some(match code {
        0x110 => MouseButton::Left,
        0x111 => MouseButton::Right,
        0x112 => MouseButton::Middle,
        0x113 => MouseButton::Back,
        0x114 => MouseButton::Forward,
        n if (0x115..=0x120).contains(&n) => MouseButton::Other((n - 0x110) as u8),
        _ => return None,
    })
}

/// evdev 相对轴码。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelAxis {
    X,
    Y,
    Wheel,
    WheelHiRes,
    HWheel,
    HWheelHiRes,
}

pub fn rel_axis_from_evdev(code: u16) -> Option<RelAxis> {
    Some(match code {
        0x00 => RelAxis::X,
        0x01 => RelAxis::Y,
        0x08 => RelAxis::Wheel,
        0x06 => RelAxis::HWheel,
        0x0b => RelAxis::WheelHiRes,
        0x0c => RelAxis::HWheelHiRes,
        _ => return None,
    })
}

/// 滚动方向换算。
/// - 内核 `REL_WHEEL` 正值 = 滚轮向下（视图向文档底部），负值 = 向上。
/// - oink 约定 `dy > 0` 表示视图向文档顶部（已在源端换算，与源设备偏好无关）。
/// - 非自然滚动：`dy = -wheel`（向下滚 -1 格 → 视图向下 → dy<0）。
/// - 自然滚动（手指上滑内容下移）：方向取反，`dy = wheel`。
pub fn scroll_dy(wheel_steps: f64, natural: bool) -> f64 {
    if natural {
        wheel_steps
    } else {
        -wheel_steps
    }
}

/// 每设备共享的控制块：主线程写，设备线程读。
#[derive(Default)]
pub struct DeviceControl {
    /// 远端接管（需要 grab 并上报事件）。
    pub remote: AtomicBool,
    /// 鼠标速度系数（f32 位模式）。
    pub mouse_speed: AtomicU32,
    /// 触摸板速度系数（f32 位模式）。
    pub touchpad_speed: AtomicU32,
    /// 触摸板自然滚动。
    pub natural_touchpad: AtomicBool,
    /// 退出信号。
    pub stop: AtomicBool,
}

pub fn speed_of(bits: &AtomicU32) -> f64 {
    f64::from(f32::from_bits(bits.load(Ordering::Relaxed)))
}

pub fn set_speed(bits: &AtomicU32, value: f64) {
    bits.store((value.clamp(0.01, 20.0) as f32).to_bits(), Ordering::Relaxed);
}

/// 跨设备共享的虚拟光标与停靠点（全局逻辑坐标）。
#[derive(Clone)]
pub struct PointerLedger {
    /// 最近已知光标位置。
    pub cursor: Arc<Mutex<Point>>,
    /// 进入 Remote 时的停靠点（本机不移动光标，仅记账）。
    pub park: Arc<Mutex<Option<Point>>>,
    /// 单调时钟锚点。
    pub epoch: Instant,
    /// 待定的滚动累积（HiRes 轴需要凑整格）。
    pub wheel: Arc<Mutex<WheelAccum>>,
    /// 输出外接矩形 (x0, y0, x1, y1)，逻辑像素。真实光标被合成器钳在这个
    /// 矩形里；虚拟账本必须同样钳位——否则推向边缘时账本越界，路由
    /// `locate(cursor)` 找不到屏幕，边缘压力与穿越移交永远不触发。
    /// 0..0 表示未知（不钳位）。
    pub bounds: Arc<Mutex<(f64, f64, f64, f64)>>,
}

#[derive(Default)]
pub struct WheelAccum {
    pub v: f64,
    pub h: f64,
    pub last: Option<Instant>,
}

impl PointerLedger {
    pub fn new(initial: Point) -> Self {
        Self {
            cursor: Arc::new(Mutex::new(initial)),
            park: Arc::new(Mutex::new(None)),
            epoch: Instant::now(),
            wheel: Arc::new(Mutex::new(WheelAccum::default())),
            bounds: Arc::new(Mutex::new((0., 0., 0., 0.))),
        }
    }

    pub fn set_bounds(&self, x0: f64, y0: f64, x1: f64, y1: f64) {
        *self.bounds.lock().unwrap() = (x0, y0, x1, y1);
    }

    fn clamp(&self, p: Point) -> Point {
        let (x0, y0, x1, y1) = *self.bounds.lock().unwrap();
        if x1 > x0 && y1 > y0 {
            Point::new(p.x.clamp(x0, x1 - 1.), p.y.clamp(y0, y1 - 1.))
        } else {
            p
        }
    }

    pub fn time_us(&self, at: Instant) -> u64 {
        at.duration_since(self.epoch).as_micros() as u64
    }

    /// 移动虚拟光标并返回新位置（钳到输出外接矩形，与真实光标一致）。
    pub fn advance(&self, dx: f64, dy: f64) -> Point {
        let mut cursor = self.cursor.lock().unwrap();
        cursor.x += dx;
        cursor.y += dy;
        let clamped = self.clamp(*cursor);
        *cursor = clamped;
        clamped
    }

    pub fn position(&self) -> Point {
        *self.cursor.lock().unwrap()
    }

    pub fn place(&self, at: Point) {
        let clamped = self.clamp(at);
        *self.cursor.lock().unwrap() = clamped;
    }
}

/// 设备线程 → 路由回调需要的一致视图。
pub struct CaptureShared {
    pub control: Arc<DeviceControl>,
    pub ledger: PointerLedger,
    /// grab 成功后置真（供 capture_active 汇报）。
    pub grabbed: Arc<AtomicBool>,
}

/// 枚举并打开可捕获设备：返回 (path, kind)。
pub fn discover_devices(dir: &Path) -> Vec<(PathBuf, DeviceKind)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return out };
    let mut names: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("event")))
        .collect();
    names.sort();
    for path in names {
        if let Some(kind) = probe_kind(&path) {
            out.push((path, kind));
        }
    }
    out
}

/// 只读能力探测（不 grab）。权限不足或设备被占用时返回 None。
pub fn probe_kind(path: &Path) -> Option<DeviceKind> {
    let device = evdev::Device::open(path).ok()?;
    classify(&caps_of(&device))
}

fn caps_of(device: &evdev::Device) -> DeviceCaps {
    use evdev::{AbsoluteAxisCode as A, KeyCode as K, PropType, RelativeAxisCode as R};
    let mut caps = DeviceCaps::default();
    caps.is_pointer = device.properties().contains(PropType::POINTER);
    if let Some(axes) = device.supported_relative_axes() {
        caps.relative_xy = axes.contains(R::REL_X) && axes.contains(R::REL_Y);
        caps.scroll_wheel = axes.contains(R::REL_WHEEL)
            || axes.contains(R::REL_WHEEL_HI_RES)
            || axes.contains(R::REL_HWHEEL);
    }
    if let Some(axes) = device.supported_absolute_axes() {
        caps.touch_multi = axes.contains(A::ABS_MT_POSITION_X);
    }
    if let Some(keys) = device.supported_keys() {
        caps.alphabet = keys.contains(K::KEY_A) && keys.contains(K::KEY_Z) && keys.contains(K::KEY_ENTER);
    }
    caps
}

/// 设备线程的事件缓冲：把 evdev 的 EV_KEY/EV_RELATIVE/EV_ABS 组装成上报事件。
#[derive(Default)]
pub struct EventAssembler {
    /// 触摸板 slot → 上一个坐标。
    pub slots: BTreeMap<i32, (f64, f64)>,
    pub slot: i32,
    /// 触摸板当前 slot 的本次 ABS_MT 更新（SYN 时与 `slots` 差分）。
    pub abs_x: Option<f64>,
    pub abs_y: Option<f64>,
    /// 本帧的相对位移累积。
    pub dx: f64,
    pub dy: f64,
    pub wheel: f64,
    pub hwheel: f64,
}

impl EventAssembler {
    /// 触摸板 ABS_MT 位置差分：i2c-hid 触摸板上报的是绝对坐标（ABS_MT_*），
    /// 不转成相对位移就永远没有 PointerDelta——REL 只来自真鼠标，而
    /// 蓝牙鼠标还会休眠断连。多点 slot 各自跟踪；BTN_TOUCH 抬起时清表
    /// （下次按下重新锚定，避免把位置跳变当位移）。
    pub fn absorb_abs_motion(&mut self, speed: f64) {
        if let (Some(nx), Some(ny)) = (self.abs_x.take(), self.abs_y.take()) {
            if let Some((px, py)) = self.slots.insert(self.slot, (nx, ny)) {
                let (ddx, ddy) = (nx - px, ny - py);
                if ddx != 0.0 || ddy != 0.0 {
                    self.dx += ddx * speed;
                    self.dy += ddy * speed;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touchpad_abs_motion_becomes_relative_delta() {
        let mut asm = EventAssembler::default();
        asm.slot = 0;
        // 首次触达：锚定，不产生位移。
        asm.abs_x = Some(500.0);
        asm.abs_y = Some(300.0);
        asm.absorb_abs_motion(1.0);
        assert_eq!((asm.dx, asm.dy), (0.0, 0.0));
        // 滑动：差分为相对位移并按 speed 缩放。
        asm.abs_x = Some(540.0);
        asm.abs_y = Some(280.0);
        asm.absorb_abs_motion(2.0);
        assert_eq!((asm.dx, asm.dy), (80.0, -40.0));
        // 抬起清锚点：下次按下重新锚定，不跳变。
        asm.slots.clear();
        asm.abs_x = Some(100.0);
        asm.abs_y = Some(100.0);
        asm.absorb_abs_motion(2.0);
        assert_eq!((asm.dx, asm.dy), (80.0, -40.0));
    }

    #[test]
    fn classifies_keyboard_mouse_touchpad() {
        let keyboard = DeviceCaps { alphabet: true, ..Default::default() };
        assert_eq!(classify(&keyboard), Some(DeviceKind::Keyboard));
        let mouse = DeviceCaps {
            is_pointer: true,
            relative_xy: true,
            scroll_wheel: true,
            ..Default::default()
        };
        assert_eq!(classify(&mouse), Some(DeviceKind::Mouse));
        let touchpad = DeviceCaps {
            is_pointer: true,
            touch_multi: true,
            ..Default::default()
        };
        assert_eq!(classify(&touchpad), Some(DeviceKind::Touchpad));
        assert_eq!(classify(&DeviceCaps::default()), None);
    }

    #[test]
    fn buttons_and_axes_map() {
        assert_eq!(button_from_evdev(0x110), Some(MouseButton::Left));
        assert_eq!(button_from_evdev(0x111), Some(MouseButton::Right));
        assert_eq!(button_from_evdev(0x114), Some(MouseButton::Forward));
        assert_eq!(button_from_evdev(0x900), None);
        assert_eq!(rel_axis_from_evdev(0x00), Some(RelAxis::X));
        assert_eq!(rel_axis_from_evdev(0x0b), Some(RelAxis::WheelHiRes));
        assert_eq!(rel_axis_from_evdev(0x06), Some(RelAxis::HWheel));
    }

    #[test]
    fn natural_scroll_inverts_direction() {
        // evdev +1 = 滚轮向下 → 非自然滚动时视图向下 → dy<0。
        assert_eq!(scroll_dy(1.0, false), -1.0);
        // 自然滚动方向取反：滚轮向下 → 视图向上 → dy>0。
        assert_eq!(scroll_dy(1.0, true), 1.0);
        // evdev -1（向上滚）→ 非自然时 dy>0（视图向顶部）。
        assert_eq!(scroll_dy(-1.0, false), 1.0);
    }

    #[test]
    fn pointer_ledger_advances() {
        let ledger = PointerLedger::new(Point::new(100., 100.));
        let p = ledger.advance(12., -8.);
        assert_eq!(p.x, 112.);
        assert_eq!(p.y, 92.);
        assert_eq!(ledger.position(), p);
        ledger.place(Point::new(5., 5.));
        assert_eq!(ledger.position().x, 5.);
    }

    #[test]
    fn pointer_ledger_clamps_like_the_real_cursor() {
        let ledger = PointerLedger::new(Point::new(100., 100.));
        ledger.set_bounds(0., 0., 2080., 1386.);
        // 持续推向右侧边缘：越推越贴边，绝不越界（路由靠留在边缘的
        // cursor 做 locate + gap 压力判定）。
        let mut p = Point::new(100., 100.);
        for _ in 0..1000 {
            p = ledger.advance(50., 0.);
        }
        assert_eq!(p.x, 2079.);
        assert_eq!(ledger.position(), p);
        ledger.place(Point::new(99999., 99999.));
        assert_eq!(ledger.position().x, 2079.);
        assert_eq!(ledger.position().y, 1385.);
    }

    #[test]
    fn empty_device_dir_yields_no_devices() {
        let dir = std::env::temp_dir().join(format!("oink-no-dev-{}", std::process::id()));
        std::fs::create_dir_all(&dir).ok();
        assert!(discover_devices(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hid_key_roundtrip_through_evdev() {
        // 捕获层把 evdev → HID；注入层再 HID → evdev，必须闭环。
        for usage in [0x04u16, 0x1c, 0x28, 0xe0, 0xe3] {
            let hid = oink_foundation::input::KeyCode::keyboard(usage);
            let evdev = oink_wayland::keymap::hid_to_evdev(hid).expect("has evdev");
            assert_eq!(oink_wayland::keymap::evdev_to_hid(evdev), Some(hid));
        }
    }

    #[test]
    fn speed_roundtrip_clamps() {
        let bits = AtomicU32::new(0);
        set_speed(&bits, 2.5);
        assert_eq!(speed_of(&bits), 2.5);
        set_speed(&bits, 100.);
        assert!(speed_of(&bits) <= 20.0);
    }
}
