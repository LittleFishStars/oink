//! HID usage ↔ evdev 键码表与 xkb keymap。
//!
//! `oink-foundation` 的 `KeyCode` 是 HID usage（`page << 16 | usage`）；
//! `zwp_virtual_keyboard_v1` 的 key 事件吃 evdev 键码（`input-event-codes.h`）。
//! 本模块提供映射，并把编译后的 XKB keymap 写入 memfd 供虚拟键盘协议上传。

use std::io::{Seek, SeekFrom, Write};

use oink_foundation::input::KeyCode;

/// HID keyboard 页 usage → evdev 键码（Linux `input-event-codes.h`）。
pub fn hid_to_evdev(code: KeyCode) -> Option<u32> {
    if code.page() == KeyCode::PAGE_CONSUMER {
        return Some(match code.usage() {
            0x92 => 158, // MEDIA_STOP
            0xB3 => 164, // MEDIA_PLAYPAUSE
            0xB5 => 163, // MEDIA_NEXT
            0xB6 => 165, // MEDIA_PREVIOUS
            0xE2 => 113, // MUTE
            0xE9 => 115, // VOLUMEUP
            0xEA => 114, // VOLUMEDOWN
            _ => return None,
        });
    }
    if code.page() != KeyCode::PAGE_KEYBOARD {
        return None;
    }
    Some(match code.usage() {
        // 字母（QWERTY 行位置）
        0x04 => 30,  // A
        0x05 => 48,  // B
        0x06 => 46,  // C
        0x07 => 32,  // D
        0x08 => 18,  // E
        0x09 => 33,  // F
        0x0a => 34,  // G
        0x0b => 35,  // H
        0x0c => 23,  // I
        0x0d => 36,  // J
        0x0e => 37,  // K
        0x0f => 38,  // L
        0x10 => 50,  // M
        0x11 => 49,  // N
        0x12 => 24,  // O
        0x13 => 25,  // P
        0x14 => 16,  // Q
        0x15 => 19,  // R
        0x16 => 31,  // S
        0x17 => 20,  // T
        0x18 => 22,  // U
        0x19 => 47,  // V
        0x1a => 17,  // W
        0x1b => 45,  // X
        0x1c => 21,  // Y
        0x1d => 44,  // Z
        // 数字行（1..9,0）
        0x1e => 2,
        0x1f => 3,
        0x20 => 4,
        0x21 => 5,
        0x22 => 6,
        0x23 => 7,
        0x24 => 8,
        0x25 => 9,
        0x26 => 10,
        0x27 => 11,
        // 控制与标点
        0x28 => 28, // Return
        0x29 => 1,  // Escape
        0x2a => 14, // BackSpace
        0x2b => 15, // Tab
        0x2c => 57, // Space
        0x2d => 12, // -
        0x2e => 13, // =
        0x2f => 26, // [
        0x30 => 27, // ]
        0x31 => 43, // \
        0x32 => 86, // 102nd (non-US backslash)
        0x33 => 39, // ;
        0x34 => 40, // '
        0x35 => 41, // `
        0x36 => 51, // ,
        0x37 => 52, // .
        0x38 => 53, // /
        0x39 => 58, // CapsLock
        // F1–F12
        0x3a => 59,
        0x3b => 60,
        0x3c => 61,
        0x3d => 62,
        0x3e => 63,
        0x3f => 64,
        0x40 => 65,
        0x41 => 66,
        0x42 => 67,
        0x43 => 68,
        0x44 => 87,
        0x45 => 88,
        // 编辑区
        0x46 => 210, // PrintScreen
        0x47 => 70,  // ScrollLock
        0x48 => 119, // Pause
        0x49 => 110, // Insert
        0x4a => 102, // Home
        0x4b => 104, // PageUp
        0x4c => 111, // Delete
        0x4d => 107, // End
        0x4e => 109, // PageDown
        0x4f => 106, // Right
        0x50 => 105, // Left
        0x51 => 108, // Down
        0x52 => 103, // Up
        // 小键盘
        0x53 => 69, // NumLock
        0x54 => 98, // KP_Divide
        0x55 => 55, // KP_Multiply
        0x56 => 74, // KP_Subtract
        0x57 => 78, // KP_Add
        0x58 => 96, // KP_Enter
        0x59 => 79, // KP_1
        0x5a => 80,
        0x5b => 81,
        0x5c => 89,
        0x5d => 90,
        0x5e => 91,
        0x5f => 92,
        0x60 => 93,
        0x61 => 94,
        0x62 => 82, // KP_0
        0x63 => 83, // KP_Decimal
        0x67 => 111, // KP_Delete (ForwardDelete)
        0x68 => 172, // KP_102nd
        // 修饰键
        0xe0 => 29,  // LeftCtrl
        0xe1 => 42,  // LeftShift
        0xe2 => 56,  // LeftAlt
        0xe3 => 125, // LeftMeta
        0xe4 => 97,  // RightCtrl
        0xe5 => 54,  // RightShift
        0xe6 => 100, // RightAlt
        0xe7 => 126, // RightMeta
        _ => return None,
    })
}

/// 供虚拟键盘上传的 keymap：memfd 中的编译后 XKB 文本。
#[derive(Debug)]
pub struct KeymapFile {
    file: std::fs::File,
    size: u32,
}

impl KeymapFile {
    /// 用合成器键盘布局（`XKB_DEFAULT_LAYOUT`，缺省 us）编译 keymap。
    pub fn current() -> Option<Self> {
        use xkbcommon::xkb;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let layout = std::env::var("XKB_DEFAULT_LAYOUT").unwrap_or_else(|_| "us".into());
        let variant = std::env::var("XKB_DEFAULT_VARIANT").unwrap_or_default();
        let keymap = xkb::Keymap::new_from_names(
            &ctx,
            "",
            "",
            &layout,
            if variant.is_empty() { "" } else { &variant },
            None,
            xkb::COMPILE_NO_FLAGS,
        )?;
        let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
        Self::from_bytes(text.as_bytes()).ok()
    }

    fn from_bytes(data: &[u8]) -> std::io::Result<Self> {
        let name = std::ffi::CString::new("oink-xkb").unwrap();
        // SAFETY: memfd_create 只创建匿名文件；fd 立即接管为 File。
        let raw = unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: raw 是刚创建的有效 fd。
        let mut file = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(raw) };
        file.write_all(data)?;
        file.flush()?;
        let size = file.seek(SeekFrom::End(0))? as u32;
        file.seek(SeekFrom::Start(0))?;
        Ok(Self { file, size })
    }

    pub fn fd(&self) -> std::os::fd::BorrowedFd<'_> {
        use std::os::fd::AsFd;
        self.file.as_fd()
    }

    pub fn size(&self) -> u32 {
        self.size
    }
}



/// evdev 键码 → HID keyboard 页 usage（[`hid_to_evdev`] 的逆）。
/// 鼠标按键（BTN_* ≥ 0x110）不在这里，由捕获层单独翻译成 `MouseButton`。
pub fn evdev_to_hid(keycode: u32) -> Option<KeyCode> {
    Some(KeyCode::keyboard(match keycode {
        30 => 0x04, // A
        48 => 0x05, // B
        46 => 0x06, // C
        32 => 0x07, // D
        18 => 0x08, // E
        33 => 0x09, // F
        34 => 0x0a, // G
        35 => 0x0b, // H
        23 => 0x0c, // I
        36 => 0x0d, // J
        37 => 0x0e, // K
        38 => 0x0f, // L
        50 => 0x10, // M
        49 => 0x11, // N
        24 => 0x12, // O
        25 => 0x13, // P
        16 => 0x14, // Q
        19 => 0x15, // R
        31 => 0x16, // S
        20 => 0x17, // T
        22 => 0x18, // U
        47 => 0x19, // V
        17 => 0x1a, // W
        45 => 0x1b, // X
        21 => 0x1c, // Y
        44 => 0x1d, // Z
        2 => 0x1e, 3 => 0x1f, 4 => 0x20, 5 => 0x21, 6 => 0x22,
        7 => 0x23, 8 => 0x24, 9 => 0x25, 10 => 0x26, 11 => 0x27,
        28 => 0x28, // Return
        1 => 0x29,  // Escape
        14 => 0x2a, // BackSpace
        15 => 0x2b, // Tab
        57 => 0x2c, // Space
        12 => 0x2d, 13 => 0x2e, 26 => 0x2f, 27 => 0x30,
        43 => 0x31, 86 => 0x32, 39 => 0x33, 40 => 0x34,
        41 => 0x35, 51 => 0x36, 52 => 0x37, 53 => 0x38,
        58 => 0x39, // CapsLock
        59 => 0x3a, 60 => 0x3b, 61 => 0x3c, 62 => 0x3d,
        63 => 0x3e, 64 => 0x3f, 65 => 0x40, 66 => 0x41,
        67 => 0x42, 68 => 0x43, 87 => 0x44, 88 => 0x45,
        210 => 0x46, 70 => 0x47, 119 => 0x48, 110 => 0x49,
        102 => 0x4a, 104 => 0x4b, 111 => 0x4c, 107 => 0x4d,
        109 => 0x4e, 106 => 0x4f, 105 => 0x50, 108 => 0x51, 103 => 0x52,
        69 => 0x53, 98 => 0x54, 55 => 0x55, 74 => 0x56, 78 => 0x57,
        96 => 0x58, 79 => 0x59, 80 => 0x5a, 81 => 0x5b, 89 => 0x5c,
        90 => 0x5d, 91 => 0x5e, 92 => 0x5f, 93 => 0x60, 94 => 0x61,
        82 => 0x62, 83 => 0x63,
        29 => 0xe0, 42 => 0xe1, 56 => 0xe2, 125 => 0xe3,
        97 => 0xe4, 54 => 0xe5, 100 => 0xe6, 126 => 0xe7,
        _ => return evdev_consumer_to_hid(keycode),
    }))
}

/// Consumer 页媒体键（evdev KEY_* → HID consumer usage）。
fn evdev_consumer_to_hid(keycode: u32) -> Option<KeyCode> {
    Some(KeyCode::new(KeyCode::PAGE_CONSUMER, match keycode {
        113 => 0xE2, // Mute
        115 => 0xE9, // VolumeUp
        114 => 0xEA, // VolumeDown
        164 => 0xB3, // PlayPause
        163 => 0xB5, // Next
        165 => 0xB6, // Previous
        158 => 0x92, // Stop
        _ => return None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_map_to_evdev() {
        assert_eq!(hid_to_evdev(KeyCode::keyboard(0x04)), Some(30)); // A→KEY_A
        assert_eq!(hid_to_evdev(KeyCode::keyboard(0x1a)), Some(17)); // W→KEY_W
        assert_eq!(hid_to_evdev(KeyCode::keyboard(0x1c)), Some(21)); // Y→KEY_Y
    }

    #[test]
    fn modifiers_map() {
        assert_eq!(hid_to_evdev(KeyCode::LEFT_CTRL), Some(29));
        assert_eq!(hid_to_evdev(KeyCode::LEFT_GUI), Some(125));
    }

    #[test]
    fn consumer_volume_maps() {
        let up = KeyCode::new(KeyCode::PAGE_CONSUMER, 0xE9);
        assert_eq!(hid_to_evdev(up), Some(115));
    }

    #[test]
    fn evdev_roundtrip_letters_and_modifiers() {
        for usage in [0x04u16, 0x1a, 0x1c, 0xe0, 0xe1, 0xe2, 0xe3] {
            let hid = KeyCode::keyboard(usage);
            let evdev = hid_to_evdev(hid).expect("has evdev");
            assert_eq!(evdev_to_hid(evdev), Some(hid), "usage {usage:#x} 应往返一致");
        }
    }

    #[test]
    fn unmapped_pages_return_none() {
        assert_eq!(hid_to_evdev(KeyCode::new(0xFF, 3)), None); // APPLE_FN
    }
}
