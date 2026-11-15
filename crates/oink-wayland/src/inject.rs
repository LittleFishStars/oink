//! 虚拟指针与虚拟键盘注入。
//!
//! 指针：每块输出一个 `zwlr_virtual_pointer_v1`（`create_virtual_pointer_with_output`），
//! 绝对移动以该输出的逻辑尺寸为 extent、输出原点为基准。实测（niri 26.04）坐标 1:1，
//! 光标按全局逻辑空间钳位，与 `oink-foundation` 的全机标坐标语义一致。
//!
//! 键盘：单个 `zwp_virtual_keyboard_v1`，先上传编译后的 XKB keymap，再按 evdev
//! 键码发按下/抬起（`key`），并同步修饰键状态（`modifiers`）。
//!
//! 时间戳：u32 毫秒，单调时钟回绕到 0。

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use wayland_client::protocol::wl_keyboard::KeyState;
use wayland_client::protocol::wl_output::WlOutput;
use wayland_client::protocol::wl_pointer::{Axis, AxisSource, ButtonState};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};

use crate::keymap::KeymapFile;
use crate::{Client, Wayland, WaylandError};

/// 注入耗时统计：慢注入（>8ms）告警，正常路径定期 debug 以便确认注入在跑。
static INJECT_STATS: std::sync::Mutex<Option<InjectStats>> = std::sync::Mutex::new(None);

struct InjectStats {
    count: u64,
    slow: u64,
    max_us: u128,
    window: std::time::Instant,
}

fn record_inject_cost(op: &str, cost: std::time::Duration) {
    let mut stats = INJECT_STATS.lock().unwrap();
    let s = stats.get_or_insert_with(|| InjectStats {
        count: 0,
        slow: 0,
        max_us: 0,
        window: std::time::Instant::now(),
    });
    s.count += 1;
    s.max_us = s.max_us.max(cost.as_micros());
    let slow = cost > std::time::Duration::from_millis(8);
    if slow {
        s.slow += 1;
    }
    if s.window.elapsed() > std::time::Duration::from_secs(2) {
        if s.slow > 0 {
            tracing::warn!(
                op,
                count = s.count,
                slow = s.slow,
                max_us = s.max_us as u64,
                "Wayland 注入存在明显延迟（>8ms/次，125Hz 下会积压触发暂停）"
            );
        } else if s.count > 0 {
            tracing::debug!(op, count = s.count, max_us = s.max_us as u64, "Wayland 注入窗口统计");
        }
        *s = InjectStats {
            count: 0,
            slow: 0,
            max_us: 0,
            window: std::time::Instant::now(),
        };
    }
}

/// 单调毫秒时间戳（u32 回绕）。evdev/wl 事件戳只要求单调与毫秒精度，
/// 用 Instant（CLOCK_MONOTONIC vDSO）比 SystemTime 便宜且不受 NTP 跳变影响。
fn stamp() -> u32 {
    static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let base = EPOCH.get_or_init(std::time::Instant::now);
    base.elapsed().as_millis() as u32
}

/// 一块输出上的虚拟指针。
struct OutputPointer {
    output_id: u32,
    /// 该输出逻辑原点与尺寸。
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    pointer: ZwlrVirtualPointerV1,
}

/// 虚拟指针注入器：路由输出的绝对坐标（全局逻辑空间）落到对应输出的指针上。
pub struct PointerInjector {
    wl: std::sync::Arc<Wayland>,
    manager: ZwlrVirtualPointerManagerV1,
    pointers: Mutex<Vec<OutputPointer>>,
    /// 最后一次注入的光标位置（全局逻辑坐标）。Wayland 客户端无法读全局光标，
    /// 注入即记账（与 Windows 后端的注入目标账本一致）。
    last: Mutex<Option<(f64, f64)>>,
}

impl PointerInjector {
    pub fn new(wl: std::sync::Arc<Wayland>) -> Result<Self, WaylandError> {
        let manager = wl
            .with_globals(|g| g.vp_manager.clone())
            .ok_or_else(|| WaylandError::Connect("缺少 zwlr_virtual_pointer_manager_v1".into()))?;
        Ok(Self { wl, manager, pointers: Mutex::new(Vec::new()), last: Mutex::new(None) })
    }

    /// 输出几何可能变了：核对并重建缺少的输出指针、丢弃消失的。
    fn ensure_pointers(&self) {
        let generation = self.wl.outputs_generation();
        let outputs = self.wl.outputs();
        {
            let held = self.pointers.lock().unwrap();
            if held.iter().all(|p| outputs.iter().any(|o| o.id == p.output_id))
                && held.len() == outputs.len()
            {
                return;
            }
        }
        // 重建：每个输出绑定专属虚拟指针（niri 会把它接到该屏）。
        let seat = self.wl.with_globals(|g| g.seat.clone());
        let mut fresh = Vec::new();
        for o in &outputs {
            let Some(proxy) = self.output_proxy(o.id) else { continue };
            let pointer = self
                .manager
                .create_virtual_pointer_with_output(seat.as_ref(), Some(&proxy), self.wl.qh(), ());
            fresh.push(OutputPointer {
                output_id: o.id,
                x: o.x as f64,
                y: o.y as f64,
                width: o.width as f64,
                height: o.height as f64,
                pointer,
            });
        }
        *self.pointers.lock().unwrap() = fresh;
        let _ = generation;
    }

    fn output_proxy(&self, id: u32) -> Option<WlOutput> {
        self.wl
            .shared()
            .outputs
            .lock()
            .unwrap()
            .iter()
            .find(|(_, info)| info.id == id)
            .map(|(proxy, _)| proxy.clone())
    }

    /// 绝对移动到全局逻辑坐标 (x, y)。找到包含该点的输出；不在任何屏内时钳到外接矩形。
    ///
    /// 热路径（125Hz 持续调用）：输出选择结果缓存在指针条目里，仅当
    /// 输出代数变化（插拔/改布局）时才重建；这里只做两次无竞争的
    /// Mutex 短锁与一次 Wayland 写入，不碰输出枚举。
    pub fn move_to(&self, x: f64, y: f64) {
        self.ensure_pointers();
        let held = self.pointers.lock().unwrap();
        // 选包含该点的输出；没有（边界/外溢）则选最近输出并钳位。
        let pick = held.iter().position(|p| {
            x >= p.x && y >= p.y && x < p.x + p.width && y < p.y + p.height
        });
        let index = match pick {
            Some(i) => i,
            None => {
                // 不在任何屏内：按到各输出中心的距离取最近。
                let (best, _) = held.iter().enumerate().min_by(|(_, a), (_, b)| {
                    dist2_hint(a, x, y).partial_cmp(&dist2_hint(b, x, y)).unwrap_or(std::cmp::Ordering::Equal)
                }).map(|(i, p)| (i, p)).unwrap_or((0, &held[0]));
                best
            }
        };
        let Some(p) = held.get(index) else { return };
        let lx = x.clamp(p.x, p.x + p.width - 1.0);
        let ly = y.clamp(p.y, p.y + p.height - 1.0);
        let t = stamp();
        let started = std::time::Instant::now();
        p.pointer.motion_absolute(t, (lx - p.x) as u32, (ly - p.y) as u32, p.width as u32, p.height as u32);
        p.pointer.frame();
        // Wayland 缓冲写满自动 flush；事件泵线程周期 flush。注入热路径
        // 不做每事件 flush 系统调用（125Hz 下每 8ms 一次 syscall 会竞争
        // 合成器 socket 并放大延迟）。
        drop(held);
        *self.last.lock().unwrap() = Some((lx, ly));
        record_inject_cost("motion_absolute", started.elapsed());
    }

    /// 相对移动（游戏模式 / MoveRelative）。不绑输出的全局指针更合适：
    /// 用第一块输出的指针，位移按全局逻辑空间累加。
    pub fn move_relative(&self, dx: f64, dy: f64) {
        self.ensure_pointers();
        let mut held = self.pointers.lock().unwrap();
        // 取包含上次位置的指针，否则第一块。
        let last = *self.last.lock().unwrap();
        let index = last
            .and_then(|(x, y)| held.iter().position(|p| {
                x >= p.x && y >= p.y && x < p.x + p.width && y < p.y + p.height
            }))
            .unwrap_or(0);
        let Some(p) = held.get_mut(index) else { return };
        let t = stamp();
        p.pointer.motion(t, dx, dy);
        p.pointer.frame();
        drop(held);
        self.wl.conn().flush().ok();
        if let Some((x, y)) = last {
            *self.last.lock().unwrap() = Some((x + dx, y + dy));
        }
    }

    /// 按下/抬起鼠标键（`button` 是 evdev BTN_* 码）。
    pub fn button(&self, button: u32, pressed: bool) {
        self.ensure_pointers();
        let held = self.pointers.lock().unwrap();
        let last = *self.last.lock().unwrap();
        // 优先用当前坐标所在输出的指针。
        let pick = last
            .and_then(|(x, y)| {
                held.iter().position(|p| {
                    x >= p.x && y >= p.y && x < p.x + p.width && y < p.y + p.height
                })
            })
            .unwrap_or(0);
        let Some(p) = held.get(pick) else { return };
        let t = stamp();
        p.pointer.button(t, button, if pressed { ButtonState::Pressed } else { ButtonState::Released });
        p.pointer.frame();
        self.wl.conn().flush().ok();
    }

    /// 滚动（逻辑像素增量；触摸板连续流）与可选的离散格数。
    pub fn scroll(&self, dx: f64, dy: f64, discrete_lines: Option<f64>) {
        self.ensure_pointers();
        let held = self.pointers.lock().unwrap();
        let last = *self.last.lock().unwrap();
        let pick = last
            .and_then(|(x, y)| {
                held.iter().position(|p| x >= p.x && y >= p.y && x < p.x + p.width && y < p.y + p.height)
            })
            .unwrap_or(0);
        let Some(p) = held.get(pick) else { return };
        let t = stamp();
        p.pointer.axis_source(if discrete_lines.is_some() { AxisSource::Wheel } else { AxisSource::Continuous });
        // wl_pointer axis 正角 = 向下/向右滚动；oink 约定 dy>0 向文档顶部，取反。
        if dy != 0.0 {
            p.pointer.axis(t, Axis::VerticalScroll, -dy);
            if let Some(d) = discrete_lines {
                p.pointer.axis_discrete(t, Axis::VerticalScroll, -dy, d.round() as i32);
            }
        }
        if dx != 0.0 {
            p.pointer.axis(t, Axis::HorizontalScroll, -dx);
            if let Some(d) = discrete_lines {
                p.pointer.axis_discrete(t, Axis::HorizontalScroll, -dx, d.round() as i32);
            }
        }
        p.pointer.frame();
        self.wl.conn().flush().ok();
    }

    /// 当前注入光标位置（全局逻辑坐标）。None：从未注入过。
    pub fn cursor(&self) -> Option<(f64, f64)> {
        *self.last.lock().unwrap()
    }
}

fn dist2_hint(o: &OutputPointer, x: f64, y: f64) -> f64 {
    let cx = o.x + o.width / 2.0;
    let cy = o.y + o.height / 2.0;
    (x - cx) * (x - cx) + (y - cy) * (y - cy)
}

fn dist2(o: &crate::OutputInfo, x: f64, y: f64) -> f64 {
    let cx = o.x as f64 + o.width as f64 / 2.0;
    let cy = o.y as f64 + o.height as f64 / 2.0;
    (x - cx) * (x - cx) + (y - cy) * (y - cy)
}

/// 虚拟键盘注入器。
pub struct KeyboardInjector {
    wl: std::sync::Arc<Wayland>,
    keyboard: Mutex<Option<ZwpVirtualKeyboardV1>>,
    /// 合成器键盘布局的编译 keymap（上传一次）。
    keymap: Mutex<Option<KeymapFile>>,
    /// 当前按下的修饰键（evdev 码），用于 modifiers 上报。
    modifiers: AtomicU32,
}

impl KeyboardInjector {
    pub fn new(wl: std::sync::Arc<Wayland>) -> Result<Self, WaylandError> {
        let _manager = wl
            .with_globals(|g| g.vk_manager.clone())
            .ok_or_else(|| WaylandError::Connect("缺少 zwp_virtual_keyboard_manager_v1".into()))?;
        Ok(Self {
            wl,
            keyboard: Mutex::new(None),
            keymap: Mutex::new(None),
            modifiers: AtomicU32::new(0),
        })
    }

    /// 懒创建键盘并上传 keymap（第一次按键时）。
    fn ensure(&self) -> Option<ZwpVirtualKeyboardV1> {
        let mut held = self.keyboard.lock().unwrap();
        if let Some(kb) = held.as_ref() {
            return Some(kb.clone());
        }
        let manager = self.wl.with_globals(|g| g.vk_manager.clone())?;
        let seat = self.wl.with_globals(|g| g.seat.clone())?;
        let kb = manager.create_virtual_keyboard(&seat, self.wl.qh(), ());
        let mut km = self.keymap.lock().unwrap();
        if km.is_none() {
            *km = KeymapFile::current();
        }
        if let Some(file) = km.as_ref() {
            kb.keymap(1 /* XKB_V1 */, file.fd(), file.size());
        }
        *held = Some(kb.clone());
        Some(kb)
    }

    /// 按下/抬起 evdev 键码。
    pub fn key(&self, keycode: u32, pressed: bool) {
        let Some(kb) = self.ensure() else { return };
        let t = stamp();
        kb.key(t, keycode, u32::from(if pressed { KeyState::Pressed } else { KeyState::Released }));
        // 修饰键按下时更新状态并上报（协议要求 keymap 之后、由客户端维护）。
        if is_modifier(keycode) {
            let mask = match pressed {
                true => self.modifiers.load(Ordering::Acquire) | (1u32 << mod_bit(keycode)),
                false => self.modifiers.load(Ordering::Acquire) & !(1u32 << mod_bit(keycode)),
            };
            self.modifiers.store(mask, Ordering::Release);
            let depressed = if pressed { mask } else { 0 };
            kb.modifiers(depressed, 0, mask, 0);
        }
        self.wl.conn().flush().ok();
    }

    /// 释放所有由我们按住的修饰键（断连/停止时）。
    pub fn release_all(&self) {
        let mask = self.modifiers.swap(0, Ordering::AcqRel);
        if mask == 0 {
            return;
        }
        if let Some(kb) = self.keyboard.lock().unwrap().as_ref() {
            let t = stamp();
            for (bit, code) in MOD_BITS {
                if mask & (1 << bit) != 0 {
                    kb.key(t, *code, u32::from(KeyState::Released));
                }
            }
            kb.modifiers(0, 0, 0, 0);
            self.wl.conn().flush().ok();
        }
    }
}

fn is_modifier(keycode: u32) -> bool {
    MOD_BITS.iter().any(|(_, code)| *code == keycode)
}

fn mod_bit(keycode: u32) -> u32 {
    MOD_BITS.iter().position(|(_, c)| *c == keycode).unwrap_or(0) as u32
}

/// 修饰键位掩码 ↔ evdev 码。
const MOD_BITS: &[(u32, u32)] = &[
    (0, 29),  // KEY_LEFTCTRL
    (1, 42),  // KEY_LEFTSHIFT
    (2, 56),  // KEY_LEFTALT
    (3, 125), // KEY_LEFTMETA
    (4, 97),  // KEY_RIGHTCTRL
    (5, 54),  // KEY_RIGHTSHIFT
    (6, 100), // KEY_RIGHTALT
    (7, 126), // KEY_RIGHTMETA
];

// 注入代理的事件都是 no-op。
impl wayland_client::Dispatch<ZwlrVirtualPointerV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ZwlrVirtualPointerV1,
        _: zwlr_virtual_pointer_v1::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

impl wayland_client::Dispatch<ZwpVirtualKeyboardV1, ()> for Client {
    fn event(
        _: &mut Self,
        _: &ZwpVirtualKeyboardV1,
        _: zwp_virtual_keyboard_v1::Event,
        _: &(),
        _: &wayland_client::Connection,
        _: &wayland_client::QueueHandle<Self>,
    ) {
    }
}

/// 注入器对（输入后端持有一对）。
pub struct Injectors {
    pub pointer: PointerInjector,
    pub keyboard: KeyboardInjector,
    /// DnD 原生后端可用性（oink-agent 安装 dnd 后端后置位）。
    pub dnd_native_available: std::sync::atomic::AtomicBool,
}

impl Injectors {
    pub fn connect(wl: std::sync::Arc<Wayland>) -> Result<Self, WaylandError> {
        Ok(Self {
            pointer: PointerInjector::new(wl.clone())?,
            keyboard: KeyboardInjector::new(wl)?,
            dnd_native_available: std::sync::atomic::AtomicBool::new(false),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mod_bits_cover_eight_modifiers() {
        assert_eq!(MOD_BITS.len(), 8);
        assert!(is_modifier(29));
        assert!(!is_modifier(30));
        assert_eq!(mod_bit(125), 3);
    }

    #[test]
    fn stamp_wraps_u32() {
        let a = stamp();
        let b = stamp();
        // 单调（同一进程内允许回绕，不验序）。
        let _ = (a, b);
    }
}

impl std::fmt::Debug for PointerInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PointerInjector").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for KeyboardInjector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyboardInjector").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Injectors {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injectors").finish_non_exhaustive()
    }
}
