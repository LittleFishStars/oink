//! 端到端注入冒烟：连接真实合成器，移动虚拟指针并读回记账。
//! 无 WAYLAND_DISPLAY 时跳过（CI 友好）。

use oink_wayland::Wayland;

#[test]
fn inject_motion_roundtrip_on_live_compositor() {
    if std::env::var("WAYLAND_DISPLAY").is_err() {
        eprintln!("无 WAYLAND_DISPLAY，跳过");
        return;
    }
    let wl = Wayland::connect().expect("connect");
    assert!(wl.has_virtual_pointer(), "合成器应支持 zwlr_virtual_pointer_v1");
    let outs = wl.outputs();
    assert!(!outs.is_empty(), "至少一块输出");
    let inj = oink_wayland::inject::Injectors::connect(wl.clone()).expect("injectors");
    let first = &outs[0];
    let target = (first.x as f64 + 100.0, first.y as f64 + 100.0);
    inj.pointer.move_to(target.0, target.1);
    assert_eq!(inj.pointer.cursor(), Some(target));
    // 按键一次（按下+抬起），不验证应用侧效果，只验证协议不报错。
    inj.pointer.button(0x110, true);
    inj.pointer.button(0x110, false);
    // 键盘：一次 A 键按下抬起。
    inj.keyboard.key(30, true);
    inj.keyboard.key(30, false);
    inj.keyboard.release_all();
}
