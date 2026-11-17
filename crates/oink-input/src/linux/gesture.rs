//! Touchpad gesture recognition from multi-finger ABS_MT data.
//!
//! Wayland has no system gesture API for devices we do not own, so we recognise
//! the common ones ourselves from the raw evdev touch stream:
//! - two-finger horizontal / vertical swipe → [`Gesture::DockSwipe`];
//! - two-finger pinch (distance change) → [`Gesture::Magnify`].
//!
//! Recognition is threshold + phase based, mirroring what macOS hands us from
//! AppKit: a gesture begins once travel passes a slop, reports cumulative
//! progress while it moves, and ends on lift-off.

use oink_foundation::contacts::{Contact, Sample, Surface, MAX_CONTACTS};
use oink_foundation::input::{Gesture, GesturePhase, SwipeAxis};
use std::collections::BTreeMap;

/// Fixed reference surface for synthesised contact samples. evdev touchpad
/// coordinates (typically a few thousand units) are clamped into it; contacts
/// exist only to select the receiving side's native touchpad path, not for
/// precise positioning (motion still travels as PointerDelta/Gesture).
const SURFACE: Surface = Surface { width: 10_000, height: 10_000 };

/// Build a validated contact sample from the active slots. Returns None when
/// there are no fingers. `device` is a stable nonzero tag for this touchpad.
pub fn contact_sample(device: u32, fingers: &BTreeMap<i32, (f64, f64)>, now_us: u64) -> Option<Sample> {
    if fingers.is_empty() {
        return None;
    }
    let mut sample = Sample {
        device: if device == 0 { 1 } else { device },
        time_us: now_us,
        surface: SURFACE,
        len: 0,
        contacts: [Contact::default(); MAX_CONTACTS],
    };
    for (index, (_slot, (x, y))) in fingers.iter().enumerate().take(MAX_CONTACTS) {
        sample.contacts[index] = Contact {
            id: (index as u32) + 1,
            x: (x.clamp(0.0, SURFACE.width as f64) as i32),
            y: (y.clamp(0.0, SURFACE.height as f64) as i32),
        };
        sample.len += 1;
    }
    sample.valid().then_some(sample)
}

/// Travel (in touchpad units) before a gesture is recognised as such.
const SLOP: f64 = 60.0;
/// Cumulative travel that counts as one complete swipe (progress 1.0).
const SWIPE_SPAN: f64 = 420.0;
/// Pinch distance change that counts as 0.1 magnification.
const PINCH_UNIT: f64 = 260.0;

#[derive(Default)]
pub struct GestureRecognizer {
    active: Option<Active>,
    /// 上一帧质心。
    last: Option<(f64, f64)>,
    /// 上一帧两指间距。
    last_distance: Option<f64>,
}

struct Active {
    axis: SwipeAxis,
    /// 沿判定轴的累计位移（带符号）。
    travel: f64,
    /// 垂直/水平位移分量，用于决定轴。
    dx: f64,
    dy: f64,
    started_at: std::time::Instant,
    last_at: std::time::Instant,
    /// 捏合的累计距离变化。
    pinch: f64,
}

/// What a frame produced. The caller forwards these as `InputEvent::Gesture`.
pub enum Outcome {
    /// Gesture events to emit this frame, in order.
    Events(Vec<Gesture>),
    /// Multi-finger motion that is *not* a recognised gesture; the caller should
    /// keep treating it as ordinary pointer motion so the cursor still moves.
    PassThrough,
}

impl GestureRecognizer {
    /// Feed one SYN frame. `fingers` is the current active finger count,
    /// `centroid` their mean position, `distance` the mean pairwise distance.
    pub fn update(
        &mut self,
        fingers: usize,
        centroid: Option<(f64, f64)>,
        distance: Option<f64>,
    ) -> Outcome {
        // Zero or one finger: not a multi-finger gesture.
        if fingers < 2 {
            let events = self.finish(GesturePhase::Ended);
            self.last = centroid;
            self.last_distance = distance;
            return if events.is_empty() {
                Outcome::PassThrough
            } else {
                Outcome::Events(events)
            };
        }

        let Some(pos) = centroid else {
            return Outcome::PassThrough;
        };
        let now = std::time::Instant::now();
        let movement = self.last.map(|(px, py)| (pos.0 - px, pos.1 - py));
        let distance_delta = match (self.last_distance, distance) {
            (Some(prev), Some(now_d)) => Some(now_d - prev),
            _ => None,
        };
        self.last = Some(pos);
        self.last_distance = distance;

        match &mut self.active {
            None => {
                let Some((dx, dy)) = movement else {
                    return Outcome::PassThrough;
                };
                // 质心位移或两指间距变化任一越过 SLOP 即确认手势：
                // 捏合时质心几乎不动，只看位移会永远识别不到。
                let travel = (dx * dx + dy * dy).sqrt();
                let pinch_delta = distance_delta.unwrap_or(0.0).abs();
                if travel < SLOP && pinch_delta < SLOP {
                    return Outcome::PassThrough;
                }
                let axis = if pinch_delta > travel * 0.8 {
                    SwipeAxis::Pinch
                } else if dx.abs() > dy.abs() {
                    SwipeAxis::Horizontal
                } else {
                    SwipeAxis::Vertical
                };
                self.active = Some(Active {
                    axis,
                    travel: if axis == SwipeAxis::Pinch { 0.0 } else { travel },
                    dx,
                    dy,
                    started_at: now,
                    last_at: now,
                    pinch: if axis == SwipeAxis::Pinch { distance_delta.unwrap_or(0.0) } else { 0.0 },
                });
                let gesture = match axis {
                    SwipeAxis::Pinch => Gesture::Magnify {
                        phase: GesturePhase::Began,
                        amount: 0.0,
                    },
                    _ => Gesture::DockSwipe {
                        axis,
                        phase: GesturePhase::Began,
                        progress: 0.0,
                        velocity: 0.0,
                    },
                };
                Outcome::Events(vec![gesture])
            }
            Some(active) => {
                let mut events = Vec::new();
                match active.axis {
                    SwipeAxis::Pinch => {
                        let delta = distance_delta.unwrap_or(0.0);
                        active.pinch += delta;
                        let amount = delta / PINCH_UNIT;
                        if amount != 0.0 {
                            events.push(Gesture::Magnify {
                                phase: GesturePhase::Changed,
                                amount,
                            });
                        }
                    }
                    axis => {
                        let (dx, dy) = movement.unwrap_or((0.0, 0.0));
                        let along = match axis {
                            SwipeAxis::Horizontal => dx,
                            _ => dy,
                        };
                        active.travel += along;
                        active.dx += dx;
                        active.dy += dy;
                        active.last_at = now;
                        let progress = active.travel / SWIPE_SPAN;
                        events.push(Gesture::DockSwipe {
                            axis,
                            phase: GesturePhase::Changed,
                            progress,
                            velocity: 0.0,
                        });
                    }
                }
                if events.is_empty() {
                    Outcome::PassThrough
                } else {
                    Outcome::Events(events)
                }
            }
        }
    }

    /// End the current gesture (lift-off or cancellation) and emit its terminal
    /// event with the accumulated progress.
    pub fn finish(&mut self, phase: GesturePhase) -> Vec<Gesture> {
        let Some(active) = self.active.take() else {
            return Vec::new();
        };
        let elapsed = active
            .last_at
            .saturating_duration_since(active.started_at)
            .as_secs_f64();
        match active.axis {
            SwipeAxis::Pinch => vec![Gesture::Magnify { phase, amount: 0.0 }],
            axis => {
                let velocity = if elapsed > 0.0 { active.travel / elapsed } else { 0.0 };
                vec![Gesture::DockSwipe {
                    axis,
                    phase,
                    progress: active.travel / SWIPE_SPAN,
                    velocity,
                }]
            }
        }
    }

    /// True while a gesture is being reported (so the caller can suppress the
    /// cursor motion that belongs to it).
    pub fn active(&self) -> bool {
        self.active.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_finger_is_not_a_gesture() {
        let mut r = GestureRecognizer::default();
        assert!(matches!(r.update(1, Some((0.0, 0.0)), None), Outcome::PassThrough));
        assert!(!r.active());
    }

    #[test]
    fn horizontal_swipe_requires_slop_then_begins() {
        let mut r = GestureRecognizer::default();
        // 第一帧建立基准，位移不足以开始。
        assert!(matches!(r.update(2, Some((0.0, 0.0)), Some(100.0)), Outcome::PassThrough));
        assert!(matches!(r.update(2, Some((20.0, 0.0)), Some(100.0)), Outcome::PassThrough));
        match r.update(2, Some((100.0, 0.0)), Some(100.0)) {
            Outcome::Events(ev) => match ev.first() {
                Some(Gesture::DockSwipe { axis, phase, .. }) => {
                    assert_eq!(*axis, SwipeAxis::Horizontal);
                    assert_eq!(*phase, GesturePhase::Began);
                }
                other => panic!("expected DockSwipe Began, got {other:?}"),
            },
            _ => panic!("expected gesture to begin past slop"),
        }
        assert!(r.active());
        let end = r.finish(GesturePhase::Ended);
        assert!(matches!(end.first(), Some(Gesture::DockSwipe { phase: GesturePhase::Ended, .. })));
    }

    #[test]
    fn pinch_detected_when_distance_changes() {
        let mut r = GestureRecognizer::default();
        r.update(2, Some((0.0, 0.0)), Some(100.0));
        match r.update(2, Some((2.0, 0.0)), Some(160.0)) {
            Outcome::Events(ev) => {
                assert!(matches!(ev.first(), Some(Gesture::Magnify { phase: GesturePhase::Began, .. })))
            }
            _ => panic!("distance change should start a pinch"),
        }
    }
}

#[cfg(test)]
mod sample_tests {
    use super::*;

    #[test]
    fn builds_valid_sample_from_fingers() {
        let mut fingers = BTreeMap::new();
        fingers.insert(0, (100.0, 200.0));
        fingers.insert(1, (300.0, 400.0));
        let sample = contact_sample(7, &fingers, 1234).unwrap();
        assert_eq!(sample.len, 2);
        assert!(sample.valid());
        assert_eq!(sample.points()[0].id, 1);
        assert_eq!(sample.points()[1].id, 2);
    }

    #[test]
    fn clamps_out_of_range_coordinates() {
        let mut fingers = BTreeMap::new();
        fingers.insert(0, (99_999.0, -5.0));
        let sample = contact_sample(0, &fingers, 1).unwrap();
        assert!(sample.valid());
        assert_eq!(sample.device, 1);
        assert_eq!(sample.points()[0].x, 10_000);
        assert_eq!(sample.points()[0].y, 0);
    }
}
