//! Wayland DnD feasibility probe (receiver direction).
//!
//! Checks whether a background agent can originate a real drag:
//!   A. our layer surface receives `wl_pointer.button` with a serial
//!      (the implicit grab `wl_data_device.start_drag` requires);
//!   B. `start_drag` is then accepted by the compositor.
//!
//! ## Measured result on niri 25.x (2026-11-17)
//!
//! - globals all present (compositor/shm/seat/data_device_manager/layer_shell);
//! - layer surface maps fine and receives `wl_pointer.enter` once
//!   (from the *physical* pointer, before injection);
//! - virtual-pointer `motion` updates the cursor position (our ledger reads
//!   the injected point) but the compositor does NOT re-run pointer
//!   hit-testing or send a new `enter`;
//! - consequently no `button` (and no serial) ever reaches our surface,
//!   so step B cannot run.
//!
//! Conclusion: on Wayland the pointer focus is compositor-owned and only
//! physical input moves it. A client cannot redirect focus onto its own
//! surface with `zwlr_virtual_pointer`, so the implicit grab needed by
//! `start_drag` is unobtainable — originating a real drag from a background
//! agent is not possible. Falling back to save-to-disk + clipboard URI.
//!
//! Run: cargo run -p oink-wayland --example dnd_poc

use std::sync::{Arc, Mutex};

use wayland_client::protocol::wl_buffer::WlBuffer;
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_data_device_manager::WlDataDeviceManager;
use wayland_client::protocol::wl_pointer::{self, WlPointer};
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_seat::{self, WlSeat};
use wayland_client::protocol::wl_shm::{self, WlShm};
use wayland_client::protocol::wl_shm_pool::WlShmPool;
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};

use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_shell_v1::{self, ZwlrLayerShellV1};
use wayland_protocols_wlr::layer_shell::v1::client::zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1};

#[derive(Default)]
struct State {
    compositor: Option<WlCompositor>,
    shm: Option<WlShm>,
    seat: Option<WlSeat>,
    ddm: Option<WlDataDeviceManager>,
    layer_shell: Option<ZwlrLayerShellV1>,
    pointer: Option<WlPointer>,
    surface: Option<WlSurface>,
    configured: bool,
    button_serial: u32,
}

struct Client;

macro_rules! noop {
    ($t:ty, $e:ty) => {
        impl Dispatch<$t, Arc<Mutex<State>>> for Client {
            fn event(
                _: &mut Self,
                _: &$t,
                _: $e,
                _: &Arc<Mutex<State>>,
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    };
}

impl Dispatch<wayland_client::protocol::wl_registry::WlRegistry, Arc<Mutex<State>>> for Client {
    fn event(
        _: &mut Self,
        registry: &wayland_client::protocol::wl_registry::WlRegistry,
        event: wayland_client::protocol::wl_registry::Event,
        state: &Arc<Mutex<State>>,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wayland_client::protocol::wl_registry::Event::Global { name, interface, version } = event {
            let mut s = state.lock().unwrap();
            match interface.as_str() {
                "wl_compositor" if s.compositor.is_none() => {
                    s.compositor = Some(registry.bind(name, version.min(6), qh, state.clone()))
                }
                "wl_shm" if s.shm.is_none() => s.shm = Some(registry.bind(name, version.min(2), qh, state.clone())),
                "wl_seat" if s.seat.is_none() => s.seat = Some(registry.bind(name, version.min(7), qh, state.clone())),
                "wl_data_device_manager" if s.ddm.is_none() => {
                    s.ddm = Some(registry.bind(name, version.min(3), qh, state.clone()))
                }
                "zwlr_layer_shell_v1" if s.layer_shell.is_none() => {
                    s.layer_shell = Some(registry.bind(name, version.min(5), qh, state.clone()))
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlSeat, Arc<Mutex<State>>> for Client {
    fn event(
        _: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        state: &Arc<Mutex<State>>,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event
            && let WEnum::Value(caps) = capabilities
            && caps.contains(wl_seat::Capability::Pointer)
        {
            let mut s = state.lock().unwrap();
            if s.pointer.is_none() {
                s.pointer = Some(seat.get_pointer(qh, state.clone()));
            }
        }
    }
}

impl Dispatch<WlPointer, Arc<Mutex<State>>> for Client {
    fn event(
        _: &mut Self,
        _: &WlPointer,
        event: wl_pointer::Event,
        state: &Arc<Mutex<State>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { .. } => println!("[POC] >>> ENTER (serial={})", match &event { wl_pointer::Event::Enter{serial,..} => *serial, _ => 0 }),
            wl_pointer::Event::Motion { .. } => {} // 噪声，忽略
            wl_pointer::Event::Button { serial, button, state: btn, .. } => {
                println!("[POC] pointer BUTTON serial={serial} button={button} state={btn:?}");
                if matches!(btn, WEnum::Value(wl_pointer::ButtonState::Pressed)) {
                    state.lock().unwrap().button_serial = serial;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, Arc<Mutex<State>>> for Client {
    fn event(
        _: &mut Self,
        ls: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        state: &Arc<Mutex<State>>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            println!("[POC] layer_surface CONFIGURE serial={serial}");
            ls.ack_configure(serial);
            state.lock().unwrap().configured = true;
        }
    }
}

noop!(WlCompositor, wayland_client::protocol::wl_compositor::Event);
noop!(WlShm, wayland_client::protocol::wl_shm::Event);
noop!(WlShmPool, wayland_client::protocol::wl_shm_pool::Event);
noop!(WlBuffer, wayland_client::protocol::wl_buffer::Event);
noop!(WlSurface, wayland_client::protocol::wl_surface::Event);
noop!(WlRegion, wayland_client::protocol::wl_region::Event);
noop!(WlDataDeviceManager, wayland_client::protocol::wl_data_device_manager::Event);
noop!(ZwlrLayerShellV1, zwlr_layer_shell_v1::Event);
noop!(WlDataDevice, wayland_client::protocol::wl_data_device::Event);
noop!(WlDataSource, wayland_client::protocol::wl_data_source::Event);

fn make_buffer(shm: &WlShm, qh: &QueueHandle<Client>, state: &Arc<Mutex<State>>) -> (WlShmPool, WlBuffer) {
    use std::io::Write;
    use std::os::fd::AsFd;
    let (width, height) = (200i32, 100i32);
    let stride = width * 4;
    let size = (stride * height) as usize;
    let fd = rustix::fs::memfd_create("oink-poc", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    rustix::fs::ftruncate(&fd, size as u64).unwrap();
    {
        let mut f = std::fs::File::from(fd.try_clone().unwrap());
        f.write_all(&vec![0xFFu8; size]).unwrap();
    }
    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, state.clone());
    let buf = pool.create_buffer(0, width, height, stride, wl_shm::Format::Argb8888, qh, state.clone());
    (pool, buf)
}

fn main() {
    let conn = Connection::connect_to_env().expect("connect Wayland");
    let mut queue = conn.new_event_queue::<Client>();
    let qh = queue.handle();
    let mut client = Client;
    let state = Arc::new(Mutex::new(State::default()));
    conn.display().get_registry(&qh, state.clone());
    queue.roundtrip(&mut client).expect("registry roundtrip");

    let (has_comp, has_shm, has_seat, has_ddm, has_layer);
    {
        let s = state.lock().unwrap();
        has_comp = s.compositor.is_some();
        has_shm = s.shm.is_some();
        has_seat = s.seat.is_some();
        has_ddm = s.ddm.is_some();
        has_layer = s.layer_shell.is_some();
    }
    println!("[POC] globals: compositor={has_comp} shm={has_shm} seat={has_seat} ddm={has_ddm} layer_shell={has_layer}");
    if !(has_comp && has_shm && has_seat && has_layer) {
        println!("[POC] ABORT: missing required globals");
        return;
    }

    queue.roundtrip(&mut client).expect("seat roundtrip");
    let pointer_ready = state.lock().unwrap().pointer.is_some();
    println!("[POC] wl_pointer bound: {pointer_ready}");
    if !pointer_ready {
        println!("[POC] ABORT: no wl_pointer");
        return;
    }

    let surface;
    let layer;
    {
        let s = state.lock().unwrap();
        surface = s.compositor.as_ref().unwrap().create_surface(&qh, state.clone());
        layer = s.layer_shell.as_ref().unwrap().get_layer_surface(
            &surface,
            None,
            zwlr_layer_shell_v1::Layer::Overlay,
            "oink-dnd-poc".into(),
            &qh,
            state.clone(),
        );
        s.compositor
            .as_ref()
            .unwrap()
            .create_region(&qh, state.clone());
    }
    layer.set_size(200, 100);
    layer.set_anchor(zwlr_layer_surface_v1::Anchor::Bottom | zwlr_layer_surface_v1::Anchor::Left);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
    let region = {
        let s = state.lock().unwrap();
        s.compositor.as_ref().unwrap().create_region(&qh, state.clone())
    };
    region.add(0, 0, 200, 100);
    surface.set_input_region(Some(&region));
    surface.commit();

    for _ in 0..25 {
        queue.roundtrip(&mut client).expect("configure roundtrip");
        if state.lock().unwrap().configured {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let configured = state.lock().unwrap().configured;
    println!("[POC] layer surface configured: {configured}");
    if !configured {
        println!("[POC] ABORT: layer surface never configured (compositor refused?)");
        return;
    }

    let (pool, buf) = make_buffer(
        &state.lock().unwrap().shm.clone().unwrap(),
        &qh,
        &state,
    );
    surface.attach(Some(&buf), 0, 0);
    surface.commit();
    queue.roundtrip(&mut client).expect("attach roundtrip");
    println!("[POC] surface attached; injecting virtual pointer now...");

    // 等合成器完成 surface 映射并稳定（否则 cursor 命中不稳定）。
    for _ in 0..30 {
        queue.roundtrip(&mut client).ok();
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let wl = oink_wayland::Wayland::connect().expect("oink Wayland connect");
    let inj = oink_wayland::inject::PointerInjector::new(wl.clone()).expect("PointerInjector");
    // 输出 2080x1386 逻辑像素，surface 锚左下角 200x100 → 中心 (100, 1336)
    inj.move_to(100.0, 1336.0);
    // 让虚拟指针 motion 落地，再按下。
    for _ in 0..20 {
        queue.roundtrip(&mut client).ok();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    println!("[POC] pressing button now (focus should be ours)");
    inj.button(0x110, true);
    for _ in 0..50 {
        queue.dispatch_pending(&mut client).ok();
        std::thread::sleep(std::time::Duration::from_millis(20));
        if state.lock().unwrap().button_serial != 0 {
            break;
        }
    }
    let serial = state.lock().unwrap().button_serial;
    if serial == 0 {
        inj.button(0x110, false);
        println!("[POC] RESULT A: FALSE — no button serial reached our surface");
        println!("[POC] => receiver-direction real drag NOT feasible via this path");
        return;
    }
    println!("[POC] RESULT A: TRUE — implicit-grab serial = {serial}");

    let ddm = state.lock().unwrap().ddm.clone();
    let Some(ddm) = ddm else {
        println!("[POC] no wl_data_device_manager; cannot test start_drag");
        return;
    };
    let seat = state.lock().unwrap().seat.clone().unwrap();
    let ds = ddm.create_data_source(&qh, state.clone());
    ds.offer("text/uri-list".into());
    let dd = ddm.get_data_device(&seat, &qh, state.clone());
    println!("[POC] calling start_drag(serial={serial})...");
    dd.start_drag(Some(&ds), &surface, None, serial);
    let result = queue.roundtrip(&mut client);
    inj.button(0x110, false);
    match result {
        Ok(_) => {
            println!("[POC] RESULT B: TRUE — start_drag accepted, connection alive");
            println!("[POC] => REAL WAYLAND DnD FEASIBLE");
        }
        Err(e) => {
            println!("[POC] RESULT B: FALSE — start_drag rejected: {e:?}");
            println!("[POC] => fall back to save-to-disk + clipboard URI");
        }
    }
    drop((pool, buf));
}
