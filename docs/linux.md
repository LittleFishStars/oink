# Oink Linux（Wayland）移植

Oink 的 Linux 支持（niri + Wayland）在 `linux-niri` 分支上开发。本文档记录平台差异、已知限制与桌面配置建议。

## 功能状态

| 功能 | 状态 | 说明 |
|---|---|---|
| 托盘（StatusNotifier） | ✅ | 菜单「显示 Oink / 退出 Oink」，左键恢复主窗口 |
| 双向鼠标穿越 | ✅ | 触摸板（ABS_MT 差分）+ 物理鼠标（REL）+ 蓝牙鼠标（能力矩阵分类 + 热插拔） |
| win → Linux 控制 | ✅ | 注入热路径零 syscall，CursorRead 读回按平台条件跳过 |
| 关窗后台运行 | ✅ | 关窗仅隐藏窗口，agent 继续工作；托盘恢复；托盘菜单退出 |
| 剪贴板共享 | ✅ | Wayland data_control（text/html/png）；双向同步；本机→对端 capture、对端→本机 write_native_guarded。接收端复用 Mac 的 portable 异构路径，过期 offer 直接丢弃由新 offer 接管 |
| 组合键转发 | ✅ | evdev 自动重复(value=2)按「仍按下」上报，避免修饰键被提前释放 |
| 文件拖拽（DnD） | ❌ | Wayland 协议限制，见下节「文件拖拽为何不可行」 |
| 音频播放（对端→本机） | ✅ | cpal ALSA host 输出，48k/stereo/f32 |
| 系统音频捕获 | ✅ | pactl null sink 路由（转发时物理扬声器静音）+ parec 从 monitor 读 48k/stereo/f32，需 pulseaudio-utils/pipewire-pulse |
| 虚拟麦克风 | ✅ | pactl 建 null sink + remap-source（`oink_mic`），对端麦克风音频经 pacat 写入；demand 用 source-outputs 轮询；需 pulseaudio-utils/pipewire-pulse |
| 触控板手势 | ✅ | evdev ABS_MT_TRACKING_ID 多指跟踪 + 质心/间距识别：两指横扫/竖扫 → DockSwipe，两指捏合 → Magnify；仅在控制对端(remote)时转发 |
| 开机自启 / 自动更新 | ❌ | UnsupportedPlatform / OsUnsupported |

## 源机捕获（本机控制其他设备）

- 设备按能力矩阵分类（不依赖可选的 `INPUT_PROP_POINTER`）：`ABS_MT` 多点 → 触摸板；`REL_X/Y + BTN_LEFT` → 鼠标；字母键 → 键盘。
- evdev 热插拔：每 2 秒扫描 `/dev/input`，新设备节点自动接入（蓝牙鼠标断连重连后 ≤2s 恢复）。
- 触摸板上报 `ABS_MT_POSITION_*` 绝对坐标，按 slot 差分为相对位移；`BTN_TOUCH` 抬起重锚定。
- 已知限制：蓝牙 HID 由 bluez 空闲断连，重连期 1~2 秒的移动事件天然丢失（协议层无法避免）。
- 多指手势：按 `ABS_MT_TRACKING_ID` 跟踪每根手指（-1=抬起），SYN 时算活跃手指数、质心与
  两指平均间距。质心位移或间距变化任一越过 slop（60 单位）即确认手势：
  - 两指横扫/竖扫 → `Gesture::DockSwipe`（progress 以 420 单位为一整次）；
  - 两指捏合 → `Gesture::Magnify`（260 单位距离变化 ≈ 0.1 放大量）。
  手势仅在控制对端（remote）时转发，本机自用不产生远端动作；手势进行中的位移不再移动光标。
  Windows 目标会把手势映射为快捷键（任务视图/切换桌面/显示桌面）。

## 穿越（Linux 作为源机）

- 前提：`gap_travel` 已在布局初始化时默认开启；两台设备的屏幕在「布局」画布上相邻。
- 指针事件链：evdev → PointerDelta（带 ledger 虚拟光标）→ 路由 `local_motion` → gap 通道。
- 排查：日志级别调到「调试」后看 `target=oink_gap`（事件源 → 闸门 → 路由 → gap 各级打点）。

## 本机光标隐藏（穿越后）

Wayland 协议层客户端无法隐藏系统光标（`wp_locked_pointer` 需要 surface 持有指针焦点，后台 agent 无 surface；这也是安全设计）。推荐用 niri 的静止隐藏，与穿越场景完全匹配——光标穿越到其他设备后停在本机边缘，1 秒自动隐藏；niri 对程序化移动保持隐藏，物理移动/按键立刻重现：

```kdl
# ~/.config/niri/cursor.kdl（或 config.kdl 的 cursor 块内）
cursor {
    # 已有配置...
    hide-after-inactive-ms 1000
}
```

`niri msg action load-config-file` 热加载，无需重启会话。

## 文件拖拽为何不可行（Wayland 协议限制）

oink 的 DnD 需要由后台 agent **发起一次系统拖拽**（把对端文件放给本机应用）。Windows（OLE
`IDropSource` + `DoDragDrop`）与 macOS（`NSDraggingSource`）都支持，Wayland 不支持：

- `wl_data_device.start_drag` 要求 origin surface 持有**匹配 serial 的 implicit grab**（该
  surface 刚收到真实的 button press）。这是合成器强制的校验。
- 指针焦点归合成器所有，**只由物理输入驱动**。`zwlr_virtual_pointer` 的 motion 能改变光标
  坐标，但合成器不会因此重做 hit-test、也不会向我们的 surface 派发 `enter`/`button`。
- 因此拿不到 serial，`start_drag` 无从调用。

实测（`cargo run -p oink-wayland --example dnd_poc`，niri 25.x，2026-11-17）：
globals 齐全、layer surface 正常映射并收到一次 `enter`（来自物理指针）；注入 motion 后
ledger 坐标正确但无新 `enter`、无 `button`、无 serial → 判定不可行。

这与「光标隐藏不可行」是同一类安全设计：客户端不能互相夺走指针焦点。

**替代方案（已实现接收端）**：对端拖来的文件不再尝试原生放置，而是直接保存到 XDG 下载
目录（`$XDG_DOWNLOAD_DIR`，未设则 `~/Downloads`），保存后把路径写入系统剪贴板供粘贴。
- 文件名经 `safe_relative` 校验：绝对路径、`..`、反斜杠一律拒绝，防止逃逸下载目录；
- 同名文件自动加 `(1)` 后缀，不覆盖已有文件；
- 已保存时打 info 日志（`dnd landing: saved files`）。
- 实现：`crates/oink-agent/src/dnd/landing.rs` + `crates/oink-clipboard/src/linux_dnd.rs`
  （最小 Linux DndBackend，只为让管线建立，不做原生拖拽）。
- 未实现发送端：本机把文件拖给对端仍不可行（无法发起原生拖拽）。

## 窗口语义

- 点 X：隐藏到托盘（agent 继续运行）。
- 托盘左键 / 「显示 Oink」：恢复窗口。
- 托盘「退出 Oink」：停止后台 agent 并退出（唯一退出途径）。

## 编译

```bash
# 本机 rch shim 故障时用本地逃生通道
RCH_SHIM_LOCAL_IDE=1 cargo build --release -p oink-agent -p oink-ui --features custom-protocol
```

## 诊断

- agent 日志：`~/.local/state/oink/Logs/oink.<日期>.log`（手动启动）或 `~/.local/share/oink/agent.log`（UI 自动拉起）。
