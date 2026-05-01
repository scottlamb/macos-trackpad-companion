//! Per-frame gesture classifier.
//!
//! Tracks contacts across frames, distinguishes 1-finger (cursor/tap),
//! 2-finger (pan/pinch/rotate, mode-locked on first significant motion),
//! 3-finger swipe, and 4-finger swipe. Pure logic — depends only on
//! [`crate::report::Frame`] and an [`Output`] sink — so the heuristics
//! can be unit-tested.

use crate::output::{MouseButton, Output, Phase, SwipeDirection};
use crate::report::{Contact, Frame};
use std::collections::HashMap;
use std::time::{Duration, Instant};

// ---- Tunables (all in normalized [0,1] coord space) ----

/// Max movement during a touch for it to count as a tap.
const TAP_MAX_MOVE: f64 = 0.012;
/// Max touch duration to count as a tap.
const TAP_MAX_DURATION: Duration = Duration::from_millis(220);
/// Centroid motion below this between frames is considered jitter.
const MOTION_DEAD_ZONE: f64 = 0.0005;

/// Centroid pan distance (normalized) needed to lock 2F mode = pan.
const PAN_LOCK: f64 = 0.005;
/// Distance-change ratio needed to lock 2F mode = pinch.
const PINCH_LOCK_RATIO: f64 = 0.04;
/// Angle change (radians) needed to lock 2F mode = rotate.
const ROTATE_LOCK_RAD: f64 = 6.0_f64 * std::f64::consts::PI / 180.0;

/// Centroid travel needed to fire a 3F or 4F swipe.
const SWIPE_TRIGGER: f64 = 0.06;

#[derive(Clone, Copy, Debug)]
struct Tracked {
    x: f64,
    y: f64,
    prev_x: f64,
    prev_y: f64,
    down_x: f64,
    down_y: f64,
    down_at: Instant,
    max_move_sq: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GestureKind {
    Idle,
    OneFinger,
    TwoFingerUnclassified,
    TwoFingerPan,
    TwoFingerPinch,
    TwoFingerRotate,
    ThreeFingerLive,
    FourFingerLive,
    /// Latched after a swipe fires, until all fingers lift.
    SwipeLatched,
}

#[derive(Clone, Copy, Debug)]
struct TwoFingerBaseline {
    initial_centroid: (f64, f64),
    initial_distance: f64,
    initial_angle: f64,
    last_centroid: (f64, f64),
    last_scale_emitted: f64,
    last_angle: f64,
}

#[derive(Clone, Copy, Debug)]
struct MultiBaseline {
    initial_centroid: (f64, f64),
}

pub struct State<O: Output> {
    out: O,
    contacts: HashMap<u8, Tracked>,
    kind: GestureKind,
    started_at: Instant,
    /// Worst-case movement of any contact since the gesture started.
    max_move_sq: f64,
    two_baseline: Option<TwoFingerBaseline>,
    multi_baseline: Option<MultiBaseline>,
}

impl<O: Output> State<O> {
    pub fn new(out: O) -> Self {
        let now = Instant::now();
        Self {
            out,
            contacts: HashMap::new(),
            kind: GestureKind::Idle,
            started_at: now,
            max_move_sq: 0.0,
            two_baseline: None,
            multi_baseline: None,
        }
    }

    pub fn on_frame(&mut self, frame: Frame) {
        let now = Instant::now();
        let active: Vec<Contact> = frame.contacts.iter().copied().filter(|c| c.tip).collect();

        // Refresh tracked-contact state (prev → current).
        let mut next: HashMap<u8, Tracked> = HashMap::with_capacity(active.len());
        for c in &active {
            let prev = self.contacts.get(&c.id).copied();
            let (prev_x, prev_y, down_x, down_y, down_at, prior_max) = match prev {
                Some(p) => (p.x, p.y, p.down_x, p.down_y, p.down_at, p.max_move_sq),
                None => (c.x, c.y, c.x, c.y, now, 0.0),
            };
            let dx = c.x - down_x;
            let dy = c.y - down_y;
            let m = (dx * dx + dy * dy).max(prior_max);
            next.insert(
                c.id,
                Tracked {
                    x: c.x,
                    y: c.y,
                    prev_x,
                    prev_y,
                    down_x,
                    down_y,
                    down_at,
                    max_move_sq: m,
                },
            );
            if m > self.max_move_sq {
                self.max_move_sq = m;
            }
        }
        self.contacts = next;

        let new_kind = self.classify(active.len());
        if new_kind != self.kind {
            self.transition(new_kind, &active, now);
        }
        if !active.is_empty() {
            self.dispatch(&active);
        }
    }

    fn classify(&self, n: usize) -> GestureKind {
        match n {
            0 => GestureKind::Idle,
            1 => GestureKind::OneFinger,
            2 => match self.kind {
                GestureKind::TwoFingerPan
                | GestureKind::TwoFingerPinch
                | GestureKind::TwoFingerRotate
                | GestureKind::TwoFingerUnclassified => self.kind,
                _ => GestureKind::TwoFingerUnclassified,
            },
            3 => match self.kind {
                GestureKind::ThreeFingerLive | GestureKind::SwipeLatched => self.kind,
                _ => GestureKind::ThreeFingerLive,
            },
            _ => match self.kind {
                GestureKind::FourFingerLive | GestureKind::SwipeLatched => self.kind,
                _ => GestureKind::FourFingerLive,
            },
        }
    }

    fn transition(
        &mut self,
        new_kind: GestureKind,
        active: &[Contact],
        now: Instant,
    ) {
        // Close out the old gesture.
        match self.kind {
            GestureKind::OneFinger => {
                if matches!(new_kind, GestureKind::Idle) {
                    let dur = now - self.started_at;
                    if dur < TAP_MAX_DURATION
                        && self.max_move_sq.sqrt() < TAP_MAX_MOVE
                    {
                        self.out.click(MouseButton::Left);
                    }
                }
            }
            GestureKind::TwoFingerPan => self.out.scroll(0.0, 0.0, Phase::Ended),
            GestureKind::TwoFingerPinch => self.out.pinch(0.0, Phase::Ended),
            GestureKind::TwoFingerRotate => self.out.rotate(0.0, Phase::Ended),
            GestureKind::TwoFingerUnclassified => {
                if matches!(new_kind, GestureKind::Idle) {
                    let dur = now - self.started_at;
                    if dur < TAP_MAX_DURATION
                        && self.max_move_sq.sqrt() < TAP_MAX_MOVE
                    {
                        self.out.click(MouseButton::Right);
                    }
                }
            }
            _ => {}
        }

        self.kind = new_kind;
        self.started_at = now;
        self.max_move_sq = 0.0;
        self.two_baseline = None;
        self.multi_baseline = None;

        match new_kind {
            GestureKind::TwoFingerUnclassified if active.len() == 2 => {
                let a = active[0];
                let b = active[1];
                let centroid = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
                let dx = b.x - a.x;
                let dy = b.y - a.y;
                let dist = (dx * dx + dy * dy).sqrt().max(1e-9);
                let ang = dy.atan2(dx);
                self.two_baseline = Some(TwoFingerBaseline {
                    initial_centroid: centroid,
                    initial_distance: dist,
                    initial_angle: ang,
                    last_centroid: centroid,
                    last_scale_emitted: 1.0,
                    last_angle: ang,
                });
            }
            GestureKind::ThreeFingerLive | GestureKind::FourFingerLive => {
                let cx: f64 = active.iter().map(|c| c.x).sum::<f64>() / active.len() as f64;
                let cy: f64 = active.iter().map(|c| c.y).sum::<f64>() / active.len() as f64;
                self.multi_baseline = Some(MultiBaseline {
                    initial_centroid: (cx, cy),
                });
            }
            _ => {}
        }
    }

    fn dispatch(&mut self, active: &[Contact]) {
        match self.kind {
            GestureKind::Idle | GestureKind::SwipeLatched => {}
            GestureKind::OneFinger => self.dispatch_one(active),
            GestureKind::TwoFingerUnclassified
            | GestureKind::TwoFingerPan
            | GestureKind::TwoFingerPinch
            | GestureKind::TwoFingerRotate => self.dispatch_two(active),
            GestureKind::ThreeFingerLive | GestureKind::FourFingerLive => self.dispatch_swipe(active),
        }
    }

    fn dispatch_one(&self, active: &[Contact]) {
        let c = active[0];
        let Some(tr) = self.contacts.get(&c.id) else {
            return;
        };
        let dx = tr.x - tr.prev_x;
        let dy = tr.y - tr.prev_y;
        if dx.abs() > MOTION_DEAD_ZONE || dy.abs() > MOTION_DEAD_ZONE {
            log::debug!(
                "cursor id={} norm_d=({:+.4},{:+.4}) raw=({},{})",
                c.id,
                dx,
                dy,
                c.raw_x,
                c.raw_y,
            );
            self.out.move_cursor_by(dx, dy);
        }
    }

    fn dispatch_two(&mut self, active: &[Contact]) {
        if active.len() != 2 {
            return;
        }
        let Some(mut base) = self.two_baseline else {
            return;
        };
        let a = active[0];
        let b = active[1];
        let centroid = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let dist = (dx * dx + dy * dy).sqrt().max(1e-9);
        let ang = dy.atan2(dx);

        // Lock mode if not yet locked.
        if matches!(self.kind, GestureKind::TwoFingerUnclassified) {
            let pan = ((centroid.0 - base.initial_centroid.0).powi(2)
                + (centroid.1 - base.initial_centroid.1).powi(2))
            .sqrt()
                / PAN_LOCK;
            let pinch = (dist / base.initial_distance - 1.0).abs() / PINCH_LOCK_RATIO;
            let rot = angle_delta(ang, base.initial_angle).abs() / ROTATE_LOCK_RAD;
            if pan >= 1.0 || pinch >= 1.0 || rot >= 1.0 {
                let max = pan.max(pinch).max(rot);
                let new_kind = if max == pan {
                    GestureKind::TwoFingerPan
                } else if max == pinch {
                    GestureKind::TwoFingerPinch
                } else {
                    GestureKind::TwoFingerRotate
                };
                self.kind = new_kind;
                match new_kind {
                    GestureKind::TwoFingerPan => self.out.scroll(0.0, 0.0, Phase::Began),
                    GestureKind::TwoFingerPinch => self.out.pinch(0.0, Phase::Began),
                    GestureKind::TwoFingerRotate => self.out.rotate(0.0, Phase::Began),
                    _ => {}
                }
            }
        }

        match self.kind {
            GestureKind::TwoFingerPan => {
                let ddx = centroid.0 - base.last_centroid.0;
                let ddy = centroid.1 - base.last_centroid.1;
                if ddx.abs() > MOTION_DEAD_ZONE || ddy.abs() > MOTION_DEAD_ZONE {
                    log::debug!("scroll norm_d=({:+.4},{:+.4})", ddx, ddy);
                    self.out.scroll(ddx, ddy, Phase::Changed);
                }
            }
            GestureKind::TwoFingerPinch => {
                let scale = dist / base.initial_distance;
                let delta = scale - base.last_scale_emitted;
                if delta.abs() > 1e-4 {
                    self.out.pinch(delta, Phase::Changed);
                    base.last_scale_emitted = scale;
                }
            }
            GestureKind::TwoFingerRotate => {
                let delta = angle_delta(ang, base.last_angle);
                if delta.abs() > 1e-4 {
                    self.out.rotate(delta.to_degrees(), Phase::Changed);
                }
            }
            _ => {}
        }

        base.last_centroid = centroid;
        base.last_angle = ang;
        self.two_baseline = Some(base);
    }

    fn dispatch_swipe(&mut self, active: &[Contact]) {
        let Some(base) = self.multi_baseline else {
            return;
        };
        let cx: f64 = active.iter().map(|c| c.x).sum::<f64>() / active.len() as f64;
        let cy: f64 = active.iter().map(|c| c.y).sum::<f64>() / active.len() as f64;
        let dx = cx - base.initial_centroid.0;
        let dy = cy - base.initial_centroid.1;

        let dir = if dx.abs() >= dy.abs() {
            if dx >= SWIPE_TRIGGER {
                Some(SwipeDirection::Right)
            } else if dx <= -SWIPE_TRIGGER {
                Some(SwipeDirection::Left)
            } else {
                None
            }
        } else if dy >= SWIPE_TRIGGER {
            Some(SwipeDirection::Down)
        } else if dy <= -SWIPE_TRIGGER {
            Some(SwipeDirection::Up)
        } else {
            None
        };

        if let Some(direction) = dir {
            self.out.swipe(direction);
            self.kind = GestureKind::SwipeLatched;
        }
    }
}

/// Smallest signed difference between two angles, in (-π, π].
fn angle_delta(a: f64, b: f64) -> f64 {
    let mut d = a - b;
    while d > std::f64::consts::PI {
        d -= 2.0 * std::f64::consts::PI;
    }
    while d <= -std::f64::consts::PI {
        d += 2.0 * std::f64::consts::PI;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Recorder {
        log: RefCell<Vec<String>>,
    }

    impl Recorder {
        fn pop(&self) -> Vec<String> {
            self.log.borrow_mut().drain(..).collect()
        }
    }

    impl Output for &Recorder {
        fn move_cursor_by(&self, dx: f64, dy: f64) {
            self.log.borrow_mut().push(format!("move {dx:.4} {dy:.4}"));
        }
        fn click(&self, button: MouseButton) {
            self.log.borrow_mut().push(format!("click {button:?}"));
        }
        fn scroll(&self, dx: f64, dy: f64, phase: Phase) {
            self.log
                .borrow_mut()
                .push(format!("scroll {dx:.4} {dy:.4} {phase:?}"));
        }
        fn pinch(&self, delta: f64, phase: Phase) {
            self.log
                .borrow_mut()
                .push(format!("pinch {delta:.4} {phase:?}"));
        }
        fn rotate(&self, delta: f64, phase: Phase) {
            self.log
                .borrow_mut()
                .push(format!("rotate {delta:.4} {phase:?}"));
        }
        fn swipe(&self, direction: SwipeDirection) {
            self.log.borrow_mut().push(format!("swipe {direction:?}"));
        }
    }

    fn frame(contacts: &[(u8, f64, f64)]) -> Frame {
        Frame {
            contacts: contacts
                .iter()
                .map(|&(id, x, y)| Contact {
                    id,
                    x,
                    y,
                    raw_x: 0,
                    raw_y: 0,
                    tip: true,
                    confidence: true,
                })
                .collect(),
            scan_time_100us: 0,
            button: false,
        }
    }

    #[test]
    fn one_finger_tap_emits_left_click() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        s.on_frame(frame(&[(1, 0.5, 0.5)]));
        s.on_frame(frame(&[]));
        let log = r.pop();
        assert!(log.iter().any(|l| l.contains("click Left")), "{log:?}");
    }

    #[test]
    fn two_finger_tap_emits_right_click() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        s.on_frame(frame(&[(1, 0.4, 0.5), (2, 0.6, 0.5)]));
        s.on_frame(frame(&[]));
        let log = r.pop();
        assert!(log.iter().any(|l| l.contains("click Right")), "{log:?}");
    }

    #[test]
    fn one_finger_drag_emits_cursor() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        s.on_frame(frame(&[(1, 0.5, 0.5)]));
        s.on_frame(frame(&[(1, 0.6, 0.5)]));
        let log = r.pop();
        assert!(log.iter().any(|l| l.starts_with("move ")), "{log:?}");
    }

    #[test]
    fn two_finger_pan_emits_scroll() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        s.on_frame(frame(&[(1, 0.4, 0.5), (2, 0.6, 0.5)]));
        s.on_frame(frame(&[(1, 0.4, 0.55), (2, 0.6, 0.55)]));
        s.on_frame(frame(&[(1, 0.4, 0.6), (2, 0.6, 0.6)]));
        s.on_frame(frame(&[]));
        let log = r.pop();
        assert!(
            log.iter().any(|l| l.starts_with("scroll") && l.contains("Began")),
            "{log:?}"
        );
        assert!(log.iter().any(|l| l.contains("Ended")), "{log:?}");
    }

    #[test]
    fn two_finger_spread_emits_pinch() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        s.on_frame(frame(&[(1, 0.45, 0.5), (2, 0.55, 0.5)]));
        s.on_frame(frame(&[(1, 0.4, 0.5), (2, 0.6, 0.5)]));
        s.on_frame(frame(&[(1, 0.3, 0.5), (2, 0.7, 0.5)]));
        s.on_frame(frame(&[]));
        let log = r.pop();
        assert!(
            log.iter().any(|l| l.starts_with("pinch") && l.contains("Began")),
            "{log:?}"
        );
    }

    #[test]
    fn two_finger_rotate_emits_rotate() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        s.on_frame(frame(&[(1, 0.4, 0.5), (2, 0.6, 0.5)]));
        // Rotate ~30° around centroid.
        s.on_frame(frame(&[(1, 0.413, 0.45), (2, 0.587, 0.55)]));
        s.on_frame(frame(&[(1, 0.45, 0.413), (2, 0.55, 0.587)]));
        s.on_frame(frame(&[]));
        let log = r.pop();
        assert!(
            log.iter().any(|l| l.starts_with("rotate") && l.contains("Began")),
            "{log:?}"
        );
    }

    #[test]
    fn three_finger_swipe_left() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        s.on_frame(frame(&[(1, 0.5, 0.5), (2, 0.55, 0.5), (3, 0.6, 0.5)]));
        s.on_frame(frame(&[(1, 0.4, 0.5), (2, 0.45, 0.5), (3, 0.5, 0.5)]));
        s.on_frame(frame(&[(1, 0.3, 0.5), (2, 0.35, 0.5), (3, 0.4, 0.5)]));
        let log = r.pop();
        assert!(log.iter().any(|l| l.contains("swipe Left")), "{log:?}");
    }
}
