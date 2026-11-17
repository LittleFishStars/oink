//! Minimal Linux DnD backend.
//!
//! Wayland cannot originate a native drag from a background process (see
//! `docs/linux.md`): pointer focus is compositor-owned and only physical input
//! moves it, so `wl_data_device.start_drag` can never satisfy the required
//! implicit-grab serial. This backend therefore installs the DnD pipeline so
//! the agent's routing/protocol halves run, but performs no native drag:
//! incoming peer files are landed by `dnd::landing` inside `DndGlue::begin`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::dnd::{DndBackend, DndSink, DragOperation, DragSpec, Injector, PlatformDndEvent};
use crate::ClipboardError;
use oink_foundation::Point;

pub(crate) fn backend() -> Result<Box<dyn DndBackend>, ClipboardError> {
    Ok(Box::new(LinuxDnd::default()))
}

#[derive(Default)]
pub(crate) struct LinuxDnd {
    started: AtomicBool,
}

impl std::fmt::Debug for LinuxDnd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinuxDnd").finish_non_exhaustive()
    }
}

impl DndBackend for LinuxDnd {
    fn start(&self, _sink: DndSink, _inject: Injector) -> Result<(), ClipboardError> {
        // No native surface: nothing to set up. The pipeline still needs this
        // backend so routing and protocol work continues.
        self.started.store(true, Ordering::Release);
        Ok(())
    }

    fn probe(&self, _at: Point) {}

    fn unprobe(&self) {}

    fn release(&self) -> bool {
        // No native drop target exists; nothing to confirm.
        false
    }

    /// Native drag origin is impossible here. `DndGlue::begin` lands peer files
    /// directly and never reaches this on Linux, but keep the contract honest.
    fn begin(&self, _spec: DragSpec) -> Result<(), ClipboardError> {
        Err(ClipboardError::Unsupported)
    }

    fn source_requires_export(&self, _operation: DragOperation, _windows_target: bool) -> bool {
        false
    }

    fn source_completion_is_async(&self) -> bool {
        false
    }

    /// Nothing to tear down: no native session was ever started.
    fn cancel(&self) -> bool {
        false
    }
}

/// Keeps `Arc` import meaningful when `DndSink` is a type alias over it.
#[allow(dead_code)]
fn _sink_is_arc(_: Arc<()>) {}

/// Unused event sink helper: Linux never emits native drag events.
#[allow(dead_code)]
fn _no_events(_: PlatformDndEvent) {}
