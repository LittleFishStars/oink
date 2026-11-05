//! Linux/Wayland 的显示器枚举（经 `oink_wayland` 的 wl_output）。
//!
//! 逻辑布局取 wl_output 的 geometry/scale/mode（与 niri `msg outputs` 的 logical 一致）；
//! 输出变化经 watch 回调通知路由。物理尺寸毫米用于内置屏识别。

use std::sync::Arc;

use oink_foundation::display::DisplayInfo;
use oink_foundation::Rect;
use oink_wayland::Wayland;

use crate::{ChangeHandler, DisplayBackend, DisplayError};

pub struct LinuxDisplays {
    wayland: Arc<Wayland>,
}

impl std::fmt::Debug for LinuxDisplays {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxDisplays").finish_non_exhaustive()
    }
}

impl LinuxDisplays {
    pub fn connect() -> Result<Self, DisplayError> {
        let wayland =
            Wayland::connect().map_err(|e| DisplayError::Platform(e.to_string()))?;
        Ok(Self { wayland })
    }
}

impl DisplayBackend for LinuxDisplays {
    fn displays(&self) -> Result<Vec<DisplayInfo>, DisplayError> {
        let outputs = self.wayland.outputs();
        if outputs.is_empty() {
            return Err(DisplayError::Platform("合成器未报告任何输出".into()));
        }
        let mut infos: Vec<DisplayInfo> = outputs
            .iter()
            .map(|o| {
                let diag_mm = ((o.phys_mm.0 * o.phys_mm.0 + o.phys_mm.1 * o.phys_mm.1) as f64).sqrt();
                DisplayInfo {
                    key: o.name.clone(),
                    name: if o.name.is_empty() { format!("Wayland 输出 {}", o.id) } else { o.name.clone() },
                    bounds: Rect::new(o.x as f64, o.y as f64, o.width as f64, o.height as f64),
                    pixel_width: o.pixel_width as u32,
                    pixel_height: o.pixel_height as u32,
                    reference_extent: None,
                    rotation_deg: 0,
                    is_primary: o.x == 0 && o.y == 0,
                    is_builtin: o.phys_mm != (0, 0) && diag_mm < 450.0,
                    is_virtual: false,
                }
            })
            .collect();
        if !infos.iter().any(|d| d.is_primary) {
            if let Some(first) = infos.first_mut() {
                first.is_primary = true;
            }
        }
        Ok(infos)
    }

    fn watch(&self, on_change: ChangeHandler) -> Result<(), DisplayError> {
        // watch_outputs 的回调在事件泵线程上；实现只负责通知。
        self.wayland.watch_outputs(on_change);
        Ok(())
    }
}
