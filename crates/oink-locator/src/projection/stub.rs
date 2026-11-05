//! 其他平台的 projection stub：无原生叠加窗口，后端不可用，渲染线程直接退出。
use std::sync::Arc;

use super::Frame;
use crate::diagnostics::Health;

pub(super) struct Backend;

impl Backend {
    pub(super) fn new(_health: Arc<Health>) -> Option<Self> {
        None
    }

    pub(super) fn present(&mut self, _frames: Vec<Frame>, _editable: bool) {}

    pub(super) fn hide(&mut self) {}

    pub(super) fn pump(&mut self) {}

    pub(super) fn escape_down(&self) -> bool {
        false
    }
}

pub(super) fn annotation_unit(_screen: oink_foundation::Rect) -> f64 {
    1.
}
