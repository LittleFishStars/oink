//! 剪贴板端到端冒烟：与系统 wl-copy/wl-paste 互操作。
//! 无 WAYLAND_DISPLAY 或无 wl-clipboard 时跳过。

use oink_foundation::clipboard::{ClipData, Sensitivity};
use oink_wayland::Wayland;

fn have_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
}

/// 系统剪贴板全局唯一：两个测试串行，避免互相覆盖 selection。
static CLIPBOARD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn write_then_system_read() {
    let _guard = CLIPBOARD_LOCK.lock().unwrap();
    if !have_wayland() || !std::path::Path::new("/usr/bin/wl-paste").exists() {
        eprintln!("跳过：无 Wayland 会话或 wl-clipboard");
        return;
    }
    let wl = Wayland::connect().expect("connect");
    assert!(wl.has_data_control(), "应支持 zwlr_data_control_v1");
    let mut backend = oink_clipboard::platform_backend().expect("platform clipboard");
    let text = "oink-clipboard-write-smoke";
    let serial = backend
        .write(&[ClipData::Text(text.into())], Sensitivity::Normal)
        .expect("write");
    assert!(serial > 0);
    // 自己的 write 返回的计数与 change_count 一致。
    assert_eq!(backend.change_count(), serial);
    // 系统侧读回。
    let out = std::process::Command::new("wl-paste")
        .output()
        .expect("wl-paste");
    assert!(out.status.success());
    let pasted = String::from_utf8_lossy(&out.stdout);
    assert_eq!(pasted.trim_end_matches('\n'), text, "系统剪贴板应读到我们写入的文本");
}

#[test]
fn system_write_then_read() {
    let _guard = CLIPBOARD_LOCK.lock().unwrap();
    if !have_wayland() || !std::path::Path::new("/usr/bin/wl-copy").exists() {
        eprintln!("跳过：无 Wayland 会话或 wl-clipboard");
        return;
    }
    let mut backend = oink_clipboard::platform_backend().expect("platform clipboard");
    let text = "oink-clipboard-read-smoke";
    let before = backend.change_count();
    // wl-copy 需要进程存活以持有 selection 所有权（退出即放弃）。
    let mut child = std::process::Command::new("wl-copy")
        .arg(text)
        .spawn()
        .expect("wl-copy 启动失败");
    std::thread::sleep(std::time::Duration::from_millis(400));
    assert!(backend.change_count() > before, "外来 selection 应增加变化计数");
    let snapshot = backend.read().expect("read");
    let text_item = snapshot.items.iter().find_map(|i| match i {
        ClipData::Text(t) => Some(t.clone()),
        _ => None,
    });
    assert_eq!(text_item.as_deref(), Some(text), "read 应取到系统剪贴板文本");
    let _ = child.kill();
    let _ = child.wait();
}
