//! Linux/Wayland 平台层（niri、Hyprland、Sway 等实现 wlr 协议系的合成器）。
//!
//! - [`Wayland`]：合成器连接与事件泵线程，维护输出几何与所需全局对象。
//! - [`inject`]：虚拟指针（`zwlr_virtual_pointer_v1`）与虚拟键盘（`zwp_virtual_keyboard_v1`）注入。
//!   只用标准协议扩展，不依赖任何合成器私有补丁。
//! - [`keymap`]：HID usage ↔ evdev 键码表与 xkb keymap。
//!
//! 坐标约定与 `oink-foundation` 的本机全局坐标一致：取 Wayland 的**逻辑像素**
//! 全局空间（wl_output logical 布局，与 niri `msg outputs` 的 `logical` 一致）。

pub mod inject;
pub mod keymap;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

use wayland_client::protocol::wl_compositor::{self, WlCompositor};
use wayland_client::protocol::wl_output::{self, WlOutput};
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_registry;
use wayland_client::protocol::wl_seat::{self, Capability, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::{
    self, ZwpVirtualKeyboardManagerV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::{
    self, ZwlrDataControlManagerV1,
};
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::{
    self, ZwlrVirtualPointerManagerV1,
};

/// 一块输出（屏幕）在合成器全局逻辑空间中的位置与尺寸。
#[derive(Clone, Debug, PartialEq)]
pub struct OutputInfo {
    /// wl_output proxy 的 object id（进程内稳定）。
    pub id: u32,
    /// registry global name；GlobalRemove 用它匹配。
    pub(crate) global_name: u32,
    pub name: String,
    /// 全局逻辑坐标原点。
    pub x: i32,
    pub y: i32,
    /// 逻辑像素尺寸（模式像素 / scale）。
    pub width: i32,
    pub height: i32,
    /// 当前模式的物理像素尺寸。
    pub pixel_width: i32,
    pub pixel_height: i32,
    pub scale: f64,
    /// wl_output 通告的物理尺寸（毫米），0 表示未知。
    pub phys_mm: (i32, i32),
    pub enabled: bool,
}

impl OutputInfo {
    pub fn contains(&self, x: f64, y: f64) -> bool {
        self.enabled
            && x >= self.x as f64
            && y >= self.y as f64
            && x < (self.x + self.width) as f64
            && y < (self.y + self.height) as f64
    }
}

#[derive(Debug, thiserror::Error)]
pub enum WaylandError {
    #[error("无法连接 Wayland 合成器：{0}")]
    Connect(String),
}

pub struct State {
    /// 输出几何 + 持有 WlOutput proxy（dropping 会失去事件）。
    pub outputs: Mutex<Vec<(WlOutput, OutputInfo)>>,
    /// 输出几何代数；注入器据此重建每输出指针。
    pub outputs_generation: AtomicU64,
    pub seat_has_pointer: AtomicBool,
    pub seat_has_keyboard: AtomicBool,
    pub watchers: Mutex<Vec<Arc<dyn Fn() + Send + Sync>>>,
    pub globals: Mutex<Globals>,
}

pub struct Globals {
    pub compositor: Option<WlCompositor>,
    pub shm: Option<WlShm>,
    pub seat: Option<WlSeat>,
    pub vp_manager: Option<ZwlrVirtualPointerManagerV1>,
    pub vk_manager: Option<ZwpVirtualKeyboardManagerV1>,
    pub dc_manager: Option<ZwlrDataControlManagerV1>,
}

pub type Shared = Arc<State>;
pub struct Client;

impl Dispatch<wl_registry::WlRegistry, Shared> for Client {
    fn event(
        _: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        shared: &Shared,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global { name, interface, version } => {
                let mut globals = shared.globals.lock().unwrap();
                match interface.as_str() {
                    "wl_compositor" if globals.compositor.is_none() => {
                        globals.compositor = Some(registry.bind(name, version.min(4), qh, shared.clone()));
                    }
                    "wl_shm" if globals.shm.is_none() => {
                        globals.shm = Some(registry.bind(name, 1, qh, shared.clone()));
                    }
                    "wl_seat" if globals.seat.is_none() => {
                        globals.seat = Some(registry.bind(name, version.min(5), qh, shared.clone()));
                    }
                    "zwlr_virtual_pointer_manager_v1" if globals.vp_manager.is_none() => {
                        globals.vp_manager = Some(registry.bind(name, version.min(2), qh, shared.clone()));
                    }
                    "zwp_virtual_keyboard_manager_v1" if globals.vk_manager.is_none() => {
                        globals.vk_manager = Some(registry.bind(name, 1, qh, shared.clone()));
                    }
                    "zwlr_data_control_manager_v1" if globals.dc_manager.is_none() => {
                        globals.dc_manager = Some(registry.bind(name, version.min(2), qh, shared.clone()));
                    }
                    "wl_output" => {
                        let output = registry.bind::<WlOutput, _, _>(name, version.min(4), qh, shared.clone());
                        let placeholder = OutputInfo {
                            id: output.id().protocol_id(),
                            global_name: name,
                            name: String::new(),
                            x: 0,
                            y: 0,
                            width: 0,
                            height: 0,
                            pixel_width: 0,
                            pixel_height: 0,
                            scale: 1.0,
                            phys_mm: (0, 0),
                            enabled: false,
                        };
                        shared.outputs.lock().unwrap().push((output, placeholder));
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                let removed = {
                    let mut outs = shared.outputs.lock().unwrap();
                    let before = outs.len();
                    outs.retain(|(_, o)| o.global_name != name);
                    outs.len() != before
                };
                if removed {
                    bump_outputs(shared);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlOutput, Shared> for Client {
    fn event(
        _: &mut Self,
        output: &WlOutput,
        event: wl_output::Event,
        shared: &Shared,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let id = output.id().protocol_id();
        let mut changed = false;
        {
            let mut outs = shared.outputs.lock().unwrap();
            let Some((_, o)) = outs.iter_mut().find(|(_, o)| o.id == id) else { return };
            match event {
                wl_output::Event::Geometry { x, y, physical_width, physical_height, .. } => {
                    o.x = x;
                    o.y = y;
                    o.phys_mm = (physical_width, physical_height);
                }
                wl_output::Event::Mode { width, height, .. } => {
                    o.pixel_width = width;
                    o.pixel_height = height;
                }
                wl_output::Event::Scale { factor } => {
                    o.scale = factor as f64;
                }
                wl_output::Event::Name { ref name } => {
                    o.name = name.clone();
                }
                wl_output::Event::Done => {
                    if o.scale > 0.0 && o.pixel_width > 0 {
                        o.width = (o.pixel_width as f64 / o.scale).round() as i32;
                        o.height = (o.pixel_height as f64 / o.scale).round() as i32;
                    }
                    let enabled = o.width > 0 && o.height > 0;
                    changed = enabled != o.enabled;
                    o.enabled = enabled;
                }
                _ => {}
            }
        }
        if changed {
            bump_outputs(shared);
        }
    }
}

impl Dispatch<WlSeat, Shared> for Client {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        event: wl_seat::Event,
        shared: &Shared,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            if let WEnum::Value(caps) = capabilities {
                shared.seat_has_pointer.store(caps.contains(Capability::Pointer), Ordering::Relaxed);
                shared.seat_has_keyboard.store(caps.contains(Capability::Keyboard), Ordering::Relaxed);
            }
        }
    }
}

macro_rules! noop_dispatch {
    ($t:ty, $ev:path) => {
        impl Dispatch<$t, Shared> for Client {
            fn event(_: &mut Self, _: &$t, _: $ev, _: &Shared, _: &Connection, _: &QueueHandle<Self>) {}
        }
    };
}
noop_dispatch!(WlCompositor, wl_compositor::Event);
noop_dispatch!(WlShm, wl_shm::Event);
noop_dispatch!(WlPointer, wl_pointer::Event);
noop_dispatch!(ZwlrVirtualPointerManagerV1, zwlr_virtual_pointer_manager_v1::Event);
noop_dispatch!(ZwpVirtualKeyboardManagerV1, zwp_virtual_keyboard_manager_v1::Event);
noop_dispatch!(ZwlrDataControlManagerV1, zwlr_data_control_manager_v1::Event);

fn bump_outputs(shared: &Shared) {
    shared.outputs_generation.fetch_add(1, Ordering::AcqRel);
    let watchers: Vec<Arc<dyn Fn() + Send + Sync>> = shared.watchers.lock().unwrap().clone();
    for w in watchers {
        w();
    }
}

/// 合成器连接 + 事件泵。可多处共享（`Arc`）。
pub struct Wayland {
    conn: Connection,
    qh: QueueHandle<Client>,
    shared: Shared,
    pump_done: Arc<AtomicBool>,
    pump: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl std::fmt::Debug for Wayland {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Wayland").finish_non_exhaustive()
    }
}

impl Wayland {
    /// 连接合成器、注册全局，启动事件泵线程。
    pub fn connect() -> Result<Arc<Self>, WaylandError> {
        let conn = Connection::connect_to_env().map_err(|e| WaylandError::Connect(e.to_string()))?;
        let display = conn.display();
        let mut queue = conn.new_event_queue::<Client>();
        let qh = queue.handle();
        let shared: Shared = Arc::new(State {
            outputs: Mutex::new(Vec::new()),
            outputs_generation: AtomicU64::new(0),
            seat_has_pointer: AtomicBool::new(false),
            seat_has_keyboard: AtomicBool::new(false),
            watchers: Mutex::new(Vec::new()),
            globals: Mutex::new(Globals {
                compositor: None,
                shm: None,
                seat: None,
                vp_manager: None,
                vk_manager: None,
                dc_manager: None,
            }),
        });
        display.get_registry(&qh, shared.clone());
        let mut client = Client;
        queue.roundtrip(&mut client).map_err(|e| WaylandError::Connect(e.to_string()))?;
        // wl_output 的 geometry/mode/done 分批到达：轮询直到几何落定（最多 400ms）。
        for _ in 0..80 {
            if shared.outputs.lock().unwrap().iter().any(|(_, o)| o.enabled) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
            queue.roundtrip(&mut client).map_err(|e| WaylandError::Connect(e.to_string()))?;
        }
        // 事件泵：dispatch_pending + 短睡。输出/seat 事件量极低；注入请求的
        // flush 由调用方完成，不依赖本泵。
        let done = Arc::new(AtomicBool::new(false));
        let pump_done = done.clone();
        let pump_conn = conn.clone();
        let handle = std::thread::Builder::new()
            .name("oink-wayland-pump".into())
            .spawn(move || {
                let mut client = Client;
                while !pump_done.load(Ordering::Acquire) {
                    pump_conn.flush().ok();
                    // dispatch_pending 一次排空全部已到达事件；无新事件返回 Ok(0)。
                    if queue.dispatch_pending(&mut client).is_err() {
                        tracing::warn!("Wayland 事件泵停止（连接错误或协议错误）");
                        return;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            })
            .map_err(|e| WaylandError::Connect(e.to_string()))?;
        Ok(Arc::new(Self {
            conn,
            qh,
            shared,
            pump_done: done,
            pump: Mutex::new(Some(handle)),
        }))
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
    pub fn qh(&self) -> &QueueHandle<Client> {
        &self.qh
    }
    pub fn shared(&self) -> &Shared {
        &self.shared
    }

    pub fn with_globals<R>(&self, f: impl FnOnce(&Globals) -> R) -> R {
        f(&self.shared.globals.lock().unwrap())
    }

    /// 启用的输出（按 (y, x) 排序）。
    pub fn outputs(&self) -> Vec<OutputInfo> {
        let mut outs: Vec<OutputInfo> = self
            .shared
            .outputs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, o)| o.enabled)
            .map(|(_, o)| o.clone())
            .collect();
        refine_with_niri_ipc(&mut outs);
        outs.sort_by_key(|o| (o.y, o.x));
        outs
    }

    /// 找到包含该全局逻辑坐标的输出 id。
    pub fn output_at(&self, x: f64, y: f64) -> Option<OutputInfo> {
        self.outputs().into_iter().find(|o| o.contains(x, y))
    }

    /// 全局外接矩形 (x0, y0, x1, y1)，逻辑像素。
    pub fn global_bounds(&self) -> Option<(i32, i32, i32, i32)> {
        let outs = self.outputs();
        if outs.is_empty() {
            return None;
        }
        let x0 = outs.iter().map(|o| o.x).min().unwrap();
        let y0 = outs.iter().map(|o| o.y).min().unwrap();
        let x1 = outs.iter().map(|o| o.x + o.width).max().unwrap();
        let y1 = outs.iter().map(|o| o.y + o.height).max().unwrap();
        Some((x0, y0, x1, y1))
    }

    pub fn outputs_generation(&self) -> u64 {
        self.shared.outputs_generation.load(Ordering::Acquire)
    }

    /// 注册输出变化监听（回调在事件泵线程上）。
    pub fn watch_outputs(&self, on_change: Arc<dyn Fn() + Send + Sync>) {
        self.shared.watchers.lock().unwrap().push(on_change);
    }

    pub fn has_virtual_pointer(&self) -> bool {
        self.with_globals(|g| g.vp_manager.is_some())
    }
    pub fn has_virtual_keyboard(&self) -> bool {
        self.with_globals(|g| g.vk_manager.is_some())
    }
    pub fn has_data_control(&self) -> bool {
        self.with_globals(|g| g.dc_manager.is_some())
    }
}

/// 用 niri 官方 IPC（`niri msg --json outputs`）精化输出几何：
/// wl_output 的 scale 只有整数（1.5 会被报成 2），logical 布局以 IPC 为准。
/// niri 不在 PATH 或不在 niri 会话时静默回退。
fn refine_with_niri_ipc(outs: &mut [OutputInfo]) {
    use std::process::Command;
    let Ok(json) = Command::new("niri").args(["msg", "--json", "outputs"]).output() else {
        return;
    };
    if !json.status.success() {
        return;
    }
    let Ok(text) = String::from_utf8(json.stdout) else { return };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) else { return };
    let Some(map) = value.as_object() else { return };
    for o in outs.iter_mut() {
        let Some(entry) = map.get(&o.name).and_then(|v| v.as_object()) else { continue };
        let Some(logical) = entry.get("logical").and_then(|v| v.as_object()) else { continue };
        let num = |k: &str| logical.get(k).and_then(|v| v.as_f64());
        let (Some(x), Some(y), Some(w), Some(h)) = (num("x"), num("y"), num("width"), num("height"))
        else {
            continue;
        };
        o.x = x as i32;
        o.y = y as i32;
        o.width = w as i32;
        o.height = h as i32;
        if let Some(scale) = num("scale") {
            o.scale = scale;
        }
    }
}

impl Drop for Wayland {
    fn drop(&mut self) {
        self.pump_done.store(true, Ordering::Release);
        if let Some(handle) = self.pump.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}
