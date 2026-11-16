//! Linux/Wayland 剪贴板后端（`zwlr_data_control_v1`，niri 支持 v2）。
//!
//! - 读：selection offer 的 `receive(mime, fd)` + 阻塞读到 EOF；支持
//!   text/plain / text/html / image/png；`x-kde-passwordManagerHint` 视为 Concealed。
//! - 写：create_data_source + offer 各 mime + `set_selection`；`send` 事件在泵线程
//!   把缓存字节写入调用方 fd。文件走 oink 自身的按需内容通道，系统剪贴板只放文本/HTML/图片。
//! - 变化：Wayland 没有变化计数，用「外来 selection 事件计数」；自己写入后的回显
//!   在 500ms 窗口内不计（与 macOS changeCount「写一次增一次」语义对齐）。
//! - 事件：offer 是合成器创建的对象，udata 与 device 一致（`Arc<RealState>`），
//!   mime 列表按 offer 的 object id 存表。

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use oink_foundation::clipboard::{ClipData, Sensitivity};
use oink_wayland::Wayland;
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_device_v1::{
    self, ZwlrDataControlDeviceV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_manager_v1::{
    self, ZwlrDataControlManagerV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_offer_v1::{
    self, ZwlrDataControlOfferV1,
};
use wayland_protocols_wlr::data_control::v1::client::zwlr_data_control_source_v1::{
    self, ZwlrDataControlSourceV1,
};

use crate::{ChangeHandler, ClipboardBackend, ClipboardError, ClipSnapshot};
use crate::content::{self, Provider, Translator};
use crate::model::{
    ContentItem, ContentSize, Inventory, ItemId, Medium, NativeFormat, REPRESENTATION_BYTES, Representation,
    RepresentationData, RepresentationId, StandardFormat, Unavailable,
};
use crate::{CapturedClipboard, FileReader};
use oink_foundation::DeviceId;
use oink_foundation::content::{ClipId, OfferId};
use oink_foundation::clipboard::ClipPlatform;
use std::io::Cursor;

const MIME_TEXT: &str = "text/plain;charset=utf-8";
const MIME_HTML: &str = "text/html";
const MIME_PNG: &str = "image/png";
const MIME_CONCEALED: &str = "x-kde-passwordManagerHint";
/// 自己写入后的回显窗口：窗口内外来 selection 不增加计数。
const OWN_ECHO_WINDOW: Duration = Duration::from_millis(500);

struct RealState {
    /// 当前 selection offer 的 mime 列表（selection 事件时落定）。
    mimes: Mutex<Vec<String>>,
    /// 保活当前 offer proxy（receive 需要；drop 即失效）。
    offer: Mutex<Option<ZwlrDataControlOfferV1>>,
    counter: AtomicU64,
    own_write_until: Mutex<Option<Instant>>,
    /// 本进程写入的 mime → 字节（send 事件按需取）。
    pending: Mutex<HashMap<String, Vec<u8>>>,
    /// offer id → 该 offer 的 mime 列表（合成器逐条推送）。
    offer_mimes: Mutex<HashMap<u32, OfferMimes>>,
    watchers: Mutex<Vec<ChangeHandler>>,
    device: Mutex<Option<ZwlrDataControlDeviceV1>>,
}

impl RealState {
    fn bump(&self) {
        self.counter.fetch_add(1, Ordering::AcqRel);
        let watchers = self.watchers.lock().unwrap().clone();
        for w in watchers {
            w();
        }
    }

    fn own_echo(&self) -> bool {
        self.own_write_until
            .lock()
            .unwrap()
            .map(|until| Instant::now() < until)
            .unwrap_or(false)
    }
}

pub struct LinuxClipboard {
    wl: Arc<Wayland>,
    /// 剪贴板独立事件队列的句柄（source/device 都挂在这里）。
    clip_qh: wayland_client::QueueHandle<ClipClient>,
    state: Arc<RealState>,
    pump_done: Arc<AtomicBool>,
    pump: Mutex<Option<std::thread::JoinHandle<()>>>,
}

/// 把 Wayland selection 的原生 mime 字节翻译成标准 ClipData。
/// 与 macOS/Windows translator 对称：同一平台的接收端不会调用它，
/// 只有发往异构平台的表示才按 standard 语义解码。
struct LinuxTranslator;
impl Translator for LinuxTranslator {
    fn standard(&self, rep: &Representation, bytes: &[u8]) -> std::io::Result<Vec<crate::ClipData>> {
        use crate::ClipData;
        let value = match rep.standard {
            Some(StandardFormat::Text) => {
                let text = String::from_utf8_lossy(bytes).into_owned();
                ClipData::Text(text)
            }
            Some(StandardFormat::Html) => {
                let text = String::from_utf8_lossy(bytes).into_owned();
                ClipData::Html(text)
            }
            Some(StandardFormat::Png) => ClipData::Png(bytes.to_vec()),
            _ => return Err(content::unavailable(Unavailable::Unsupported)),
        };
        Ok(vec![value])
    }
}

/// 当前 selection 的按需读取 provider。Wayland data_control 的 offer 在
/// selection 不变期间可多次 receive；变化由 check 闭包（change_count）发现。
struct LinuxClipProvider {
    wl: Arc<Wayland>,
    state: Arc<RealState>,
    inventory: Inventory,
    /// representation id → mime，open 时据此 receive_mime。
    mimes: HashMap<RepresentationId, String>,
}
impl Provider for LinuxClipProvider {
    fn inventory(&self) -> Inventory {
        self.inventory.clone()
    }
    fn open(&self, id: RepresentationId) -> std::io::Result<FileReader> {
        let mime = self.mimes.get(&id).ok_or_else(|| content::unavailable(Unavailable::NoSuchItem))?;
        let bytes = receive_mime_from(&self.wl, &self.state, mime)
            .ok_or_else(|| content::unavailable(Unavailable::ReadFailed))?;
        if bytes.len() as u64 > REPRESENTATION_BYTES {
            return Err(content::unavailable(Unavailable::LimitExceeded));
        }
        Ok(Box::new(Cursor::new(bytes)))
    }
    fn translator(&self) -> Arc<dyn Translator> {
        Arc::new(LinuxTranslator)
    }
}

/// 读一个 mime 的全部内容（receive + 阻塞读 EOF）。独立成自由函数，
/// 供后端 read() 与按需读取 provider 共用同一份 offer 读取逻辑。
fn receive_mime_from(wl: &Wayland, state: &RealState, mime: &str) -> Option<Vec<u8>> {
    let proxy = state.offer.lock().unwrap().clone()?;
    if !state.mimes.lock().unwrap().iter().any(|m| m == mime) {
        return None;
    }
    let (read_fd, write_fd) = rustix::pipe::pipe().ok()?;
    proxy.receive(mime.to_string(), write_fd.as_fd());
    drop(write_fd); // 合成器写完关闭写端后读端见 EOF
    wl.conn().flush().ok();
    let mut file = std::fs::File::from(read_fd);
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    if buf.is_empty() {
        return None;
    }
    Some(buf)
}


impl std::fmt::Debug for LinuxClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxClipboard").finish_non_exhaustive()
    }
}

impl LinuxClipboard {
    pub fn connect() -> Result<Self, ClipboardError> {
        let wl = Wayland::connect().map_err(|_| ClipboardError::Busy)?;
        if !wl.has_data_control() {
            return Err(ClipboardError::Unsupported);
        }
        let state = Arc::new(RealState {
            mimes: Mutex::new(Vec::new()),
            offer: Mutex::new(None),
            counter: AtomicU64::new(0),
            own_write_until: Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            offer_mimes: Mutex::new(HashMap::new()),
            watchers: Mutex::new(Vec::new()),
            device: Mutex::new(None),
        });
        let pump_done = Arc::new(AtomicBool::new(false));
        let pump = Mutex::new(None);
        let clip_qh = Self::spawn_pump(&wl, &state, &pump_done, &pump)?;
        Ok(Self { wl, state, pump_done, pump, clip_qh })
    }

    fn spawn_pump(
        wl: &Arc<Wayland>,
        state: &Arc<RealState>,
        pump_done: &Arc<AtomicBool>,
        pump: &Mutex<Option<std::thread::JoinHandle<()>>>,
    ) -> Result<wayland_client::QueueHandle<ClipClient>, ClipboardError> {
        // 独立连接 + 独立事件队列：selection/send 事件只在这里流动。
        let conn = wl.conn().clone();
        let mut queue = conn.new_event_queue::<ClipClient>();
        let qh = queue.handle();
        conn.display().get_registry(&qh, state.clone());
        let dc = wl
            .with_globals(|g| g.dc_manager.clone())
            .ok_or(ClipboardError::Unsupported)?;
        let seat = wl
            .with_globals(|g| g.seat.clone())
            .ok_or(ClipboardError::Busy)?;
        let device = dc.get_data_device(&seat, &qh, state.clone());
        *state.device.lock().unwrap() = Some(device);
        let mut client = ClipClient;
        queue.roundtrip(&mut client).map_err(|_| ClipboardError::Busy)?;
        let done = pump_done.clone();
        // blocking_dispatch：selection/send 是低频事件，阻塞泵即可；
        // Drop 只置标志并 detach（线程随进程生命周期结束，join 会卡在阻塞读上）。
        let handle = std::thread::Builder::new()
            .name("oink-clipboard-pump".into())
            .spawn(move || {
                while !done.load(Ordering::Acquire) {
                    conn.flush().ok();
                    if queue.blocking_dispatch(&mut client).is_err() {
                        return;
                    }
                }
            })
            .map_err(|_| ClipboardError::Busy)?;
        *pump.lock().unwrap() = Some(handle);
        Ok(qh)
    }

    /// 读一个 mime 的全部内容（receive + 阻塞读 EOF）。
    fn receive_mime(&self, mime: &str) -> Option<Vec<u8>> {
        receive_mime_from(&self.wl, &self.state, mime)
    }

    /// 把 mime→字节 的载荷发布为本机 selection（write 与 write_native_guarded 共用）。
    fn publish_payload(&self, payload: HashMap<String, Vec<u8>>, sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        let source = self
            .wl
            .with_globals(|g| g.dc_manager.clone())
            .ok_or(ClipboardError::Unsupported)?
            .create_data_source(&self.clip_qh, self.state.clone());
        let mut mimes: Vec<&String> = payload.keys().collect();
        mimes.sort();
        for mime in &mimes {
            source.offer((*mime).clone());
        }
        if sensitivity == Sensitivity::Concealed {
            source.offer(MIME_CONCEALED.into());
        }
        // 先缓存内容再置 selection，避免 send 事件早于 pending 就绪。
        *self.state.pending.lock().unwrap() = payload;
        *self.state.own_write_until.lock().unwrap() = Some(Instant::now() + OWN_ECHO_WINDOW);
        if let Some(device) = self.state.device.lock().unwrap().as_ref() {
            device.set_selection(Some(&source));
        }
        self.wl.conn().flush().ok();
        Ok(self.state.counter.fetch_add(1, Ordering::AcqRel) + 1)
    }
}

impl ClipboardBackend for LinuxClipboard {
    fn change_count(&self) -> u64 {
        self.state.counter.load(Ordering::Acquire)
    }

    fn read(&self) -> Result<ClipSnapshot, ClipboardError> {
        let change_count = self.change_count();
        let concealed = self.state.mimes.lock().unwrap().iter().any(|m| m == MIME_CONCEALED);
        let mut items = Vec::new();
        if let Some(text) = self.receive_mime(MIME_TEXT) {
            if let Ok(s) = String::from_utf8(text) {
                items.push(ClipData::Text(s));
            }
        }
        if items.is_empty() {
            if let Some(png) = self.receive_mime(MIME_PNG) {
                items.push(ClipData::Png(png));
            }
        }
        if let Some(html) = self.receive_mime(MIME_HTML) {
            if let Ok(h) = String::from_utf8(html) {
                items.push(ClipData::Html(h));
            }
        }
        Ok(ClipSnapshot {
            items,
            sensitivity: if concealed { Sensitivity::Concealed } else { Sensitivity::Normal },
            change_count,
        })
    }

    /// Native capture: build an on-demand provider over the current selection so
    /// the agent can publish an offer to peers. Without this override the trait's
    /// default returns `content: None`, which makes the agent drop the offer and
    /// Linux→Windows clipboard sharing silently do nothing.
    fn capture(&self, _data: &std::path::Path) -> Result<CapturedClipboard, ClipboardError> {
        let count = self.change_count();
        let mimes = self.state.mimes.lock().unwrap().clone();
        let concealed = mimes.iter().any(|m| m == MIME_CONCEALED);

        // 只声明当前 offer 里确实存在、且我们支持按需读取的 mime。
        let supported: [(&str, StandardFormat); 3] = [
            (MIME_TEXT, StandardFormat::Text),
            (MIME_HTML, StandardFormat::Html),
            (MIME_PNG, StandardFormat::Png),
        ];
        let mut rep_id = 0u32;
        let mut rep_mimes: HashMap<RepresentationId, String> = HashMap::new();
        let mut representations = Vec::new();
        for (mime, standard) in supported {
            if !mimes.iter().any(|m| m == mime) {
                continue;
            }
            rep_id += 1;
            let id = RepresentationId(rep_id);
            rep_mimes.insert(id, mime.to_string());
            representations.push(Representation {
                id,
                // FormatId 要求 namespace:rest 形式；x11-mime 为自定义命名空间，
                // 真正的跨平台语义由 portable_format(standard) 承载。
                format: NativeFormat::Unknown(format!("x11-mime:{mime}")),
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard: Some(standard),
                data: RepresentationData::Reader { size: ContentSize::Unknown },
            });
        }
        if representations.is_empty() {
            return Err(ClipboardError::Unsupported);
        }
        let inventory = Inventory {
            id: OfferId {
                clip: ClipId { origin: DeviceId([0; 32]), seq: 0 },
                generation: count,
            },
            platform: ClipPlatform::Unknown,
            items: vec![ContentItem {
                id: ItemId(0),
                virtual_files: false,
                file: None,
                representations,
            }],
            roots: Vec::new(),
        };
        inventory.validate().map_err(ClipboardError::ContentUnavailable)?;

        let provider = LinuxClipProvider {
            wl: self.wl.clone(),
            state: self.state.clone(),
            inventory,
            mimes: rep_mimes,
        };
        let snapshot = ClipSnapshot {
            items: content::items(&provider.inventory()),
            sensitivity: if concealed { Sensitivity::Concealed } else { Sensitivity::Normal },
            change_count: count,
        };
        // 与 macOS/Windows 对齐：capture 期间 selection 若变化则放弃，
        // 避免把过期 offer 当作当前内容发布。
        if self.change_count() != count {
            return Err(ClipboardError::Busy);
        }
        Ok(CapturedClipboard {
            snapshot,
            content: Some(Arc::new(provider)),
        })
    }

    fn write(&self, items: &[ClipData], sensitivity: Sensitivity) -> Result<u64, ClipboardError> {
        let mut payload: HashMap<String, Vec<u8>> = HashMap::new();
        for item in items {
            match item {
                ClipData::Text(s) => {
                    payload.insert(MIME_TEXT.into(), s.clone().into_bytes());
                }
                ClipData::Html(h) => {
                    payload.insert(MIME_HTML.into(), h.clone().into_bytes());
                }
                ClipData::Png(b) => {
                    payload.insert(MIME_PNG.into(), b.clone());
                }
                // 文件走 oink 自身的按需内容通道（M4）；Rtf/Custom 无 Linux 对应。
                ClipData::Files(_) | ClipData::Rtf(_) | ClipData::Custom { .. }
                | ClipData::Native { .. } | ClipData::FileRoots(_) => {}
            }
        }
        if payload.is_empty() {
            return Err(ClipboardError::Busy);
        }
        self.publish_payload(payload, sensitivity)
    }

    /// 接收端：把对端提供的 NativeSpec 写入本机 Wayland selection。
    /// 远端字节经协议已归一化为 portable 标准格式（UTF-8 文本/HTML、PNG），
    /// 这里按 standard 直接映射到本机 mime，从 contents provider 按需读出后发布。
    fn write_native_guarded(
        &self,
        spec: crate::NativeSpec,
        guard: crate::content::ReaderGuard,
    ) -> Result<u64, ClipboardError> {
        guard().map_err(|_| ClipboardError::Busy)?;
        let mut payload: HashMap<String, Vec<u8>> = HashMap::new();
        for item in &spec.inventory.items {
            for rep in &item.representations {
                if matches!(rep.data, RepresentationData::Unavailable(_)) {
                    continue;
                }
                if rep.standard.is_none() {
                    continue;
                }
                let mut reader = spec
                    .contents
                    .open(rep.id)
                    .map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?;
                let mut bytes = Vec::new();
                reader
                    .read_to_end(&mut bytes)
                    .map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?;
                // 协议字节是 portable 标准格式，经 decode_standard 解码为 ClipData
                // （Text/Html 校验 UTF-8，Png 原样），再映射回本机 mime。
                let decoded = content::decode_standard(rep, &bytes)
                    .map_err(|e| ClipboardError::ContentUnavailable(content::reason(&e)))?;
                for data in decoded {
                    let (mime, value) = match data {
                        ClipData::Text(s) => (MIME_TEXT, s.into_bytes()),
                        ClipData::Html(h) => (MIME_HTML, h.into_bytes()),
                        ClipData::Png(p) => (MIME_PNG, p),
                        _ => continue,
                    };
                    if !value.is_empty() {
                        payload.insert(mime.to_string(), value);
                    }
                }
            }
        }
        if payload.is_empty() {
            return Err(ClipboardError::Unsupported);
        }
        // 阻塞读取期间 selection 可能已变化，发布前再校验一次。
        guard().map_err(|_| ClipboardError::Busy)?;
        self.publish_payload(payload, spec.sensitivity)
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), ClipboardError> {
        self.state.watchers.lock().unwrap().push(on_change);
        Ok(())
    }
}

impl Drop for LinuxClipboard {
    fn drop(&mut self) {
        self.pump_done.store(true, Ordering::Release);
        // 泵线程阻塞在 dispatch 上，detach 让它随进程退出。
        if let Some(handle) = self.pump.lock().unwrap().take() {
            let _ = handle;
        }
    }
}

// ---- 事件端 ----

struct ClipClient;

/// 每个合成器创建的 offer 的 udata：自己的 mime 列表。
type OfferMimes = Arc<Mutex<Vec<String>>>;

/// event_created_child! 需要静态构造 udata；这里给每个新 offer 一份空列表，
/// 由 DataOffer 事件把它登记进 RealState.offer_mimes（按 offer id）。
fn new_offer_mimes() -> OfferMimes {
    Arc::new(Mutex::new(Vec::new()))
}

impl Dispatch<wl_registry::WlRegistry, Arc<RealState>> for ClipClient {
    fn event(
        _: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        state: &Arc<RealState>,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "wl_seat" {
                let _ = registry.bind::<wayland_client::protocol::wl_seat::WlSeat, _, _>(
                    name,
                    version.min(5),
                    qh,
                    state.clone(),
                );
            }
        }
    }
}

macro_rules! noop {
    ($t:ty, $e:ty) => {
        impl Dispatch<$t, Arc<RealState>> for ClipClient {
            fn event(_: &mut Self, _: &$t, _: $e, _: &Arc<RealState>, _: &Connection, _: &QueueHandle<Self>) {}
        }
    };
}
noop!(wayland_client::protocol::wl_seat::WlSeat, wayland_client::protocol::wl_seat::Event);
noop!(ZwlrDataControlManagerV1, zwlr_data_control_manager_v1::Event);

impl Dispatch<ZwlrDataControlDeviceV1, Arc<RealState>> for ClipClient {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: zwlr_data_control_device_v1::Event,
        state: &Arc<RealState>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_data_control_device_v1::Event::DataOffer { id: offer } => {
                let mimes = offer.data::<OfferMimes>().cloned().unwrap_or_else(new_offer_mimes);
                state
                    .offer_mimes
                    .lock()
                    .unwrap()
                    .insert(offer.id().protocol_id(), mimes);
            }
            zwlr_data_control_device_v1::Event::Selection { id: offer } => {
                match offer {
                    Some(offer) => {
                        let mimes = state
                            .offer_mimes
                            .lock()
                            .unwrap()
                            .get(&offer.id().protocol_id())
                            .map(|m| m.lock().unwrap().clone())
                            .unwrap_or_default();
                        *state.mimes.lock().unwrap() = mimes;
                        *state.offer.lock().unwrap() = Some(offer);
                        if !state.own_echo() {
                            state.bump();
                        }
                    }
                    None => {
                        *state.mimes.lock().unwrap() = Vec::new();
                        *state.offer.lock().unwrap() = None;
                        if !state.own_echo() {
                            state.bump();
                        }
                    }
                }
            }
            zwlr_data_control_device_v1::Event::Finished => {
                *state.offer.lock().unwrap() = None;
                state.mimes.lock().unwrap().clear();
            }
            _ => {}
        }
    }

    // opcode 0 = data_offer：合成器创建的 offer 自带空 mime 列表。
    wayland_client::event_created_child!(ClipClient, ZwlrDataControlDeviceV1, [
        0 => (ZwlrDataControlOfferV1, new_offer_mimes()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, OfferMimes> for ClipClient {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlOfferV1,
        event: zwlr_data_control_offer_v1::Event,
        mimes: &OfferMimes,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_data_control_offer_v1::Event::Offer { mime_type } = event {
            mimes.lock().unwrap().push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, Arc<RealState>> for ClipClient {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlSourceV1,
        event: zwlr_data_control_source_v1::Event,
        state: &Arc<RealState>,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use std::io::Write;
        use std::os::fd::FromRawFd;
        match event {
            zwlr_data_control_source_v1::Event::Send { mime_type, fd } => {
                // SAFETY: 协议传入的有效 fd，写完立即接管并关闭。
                let mut file = std::fs::File::from(fd);
                if let Some(data) = state.pending.lock().unwrap().get(&mime_type) {
                    let _ = file.write_all(data);
                }
                // file drop 关闭写端，调用方见 EOF。
            }
            _ => {}
        }
    }
}
