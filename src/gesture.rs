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
    /// One-frame deferred cursor motion. `dispatch_one` emits the
    /// previous frame's value and stages the current frame's; on
    /// transition out of `OneFinger` (most importantly to `Idle` on
    /// lift) the buffered value is discarded. Mirrors rmk's
    /// `TrackpadProcessor::pending_motion` — the chip's last
    /// with-finger frame commonly carries a centroid-shift artifact
    /// (the contact patch shrinks asymmetrically as the finger rolls
    /// off) that, if emitted, teleports the cursor on release. Costs
    /// ~one chip cycle of cursor latency for not getting that jump.
    pending_motion: Option<(f64, f64)>,
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
            pending_motion: None,
        }
    }

    pub fn on_frame(&mut self, frame: Frame) {
        self.on_frame_at(frame, Instant::now());
    }

    /// Like [`Self::on_frame`] but with an injected timestamp. Production
    /// uses `on_frame`; tests use this so tap/hold thresholds can be
    /// exercised deterministically without sleeping.
    pub fn on_frame_at(&mut self, frame: Frame, now: Instant) {
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
                // Drop any deferred cursor motion. On a transition to
                // Idle this is the chip's last with-finger frame's
                // motion (often a centroid-shift artifact); on a
                // transition to TwoFinger* it's stale single-finger
                // motion that's no longer meaningful.
                let dropped = self.pending_motion.take();
                if matches!(new_kind, GestureKind::Idle) {
                    let dur = now - self.started_at;
                    let max_move = self.max_move_sq.sqrt();
                    if dur < TAP_MAX_DURATION && max_move < TAP_MAX_MOVE {
                        log::debug!(
                            "1f tap: click Left (dur={}ms max_move={:.4}{})",
                            dur.as_millis(),
                            max_move,
                            if dropped.is_some() { ", dropped lift-frame motion" } else { "" },
                        );
                        self.out.click(MouseButton::Left);
                    } else {
                        log::debug!(
                            "1f lift, no tap: dur={}ms max_move={:.4} (limits dur<{}ms move<{:.4})",
                            dur.as_millis(),
                            max_move,
                            TAP_MAX_DURATION.as_millis(),
                            TAP_MAX_MOVE,
                        );
                    }
                }
            }
            GestureKind::TwoFingerPan => {
                log::debug!("scroll: ended");
                self.out.scroll(0.0, 0.0, Phase::Ended);
            }
            GestureKind::TwoFingerPinch => {
                log::debug!("pinch: ended");
                self.out.pinch(0.0, Phase::Ended);
            }
            GestureKind::TwoFingerRotate => {
                log::debug!("rotate: ended");
                self.out.rotate(0.0, Phase::Ended);
            }
            GestureKind::TwoFingerUnclassified => {
                if matches!(new_kind, GestureKind::Idle) {
                    let dur = now - self.started_at;
                    let max_move = self.max_move_sq.sqrt();
                    if dur < TAP_MAX_DURATION && max_move < TAP_MAX_MOVE {
                        log::debug!(
                            "2f tap: click Right (dur={}ms max_move={:.4})",
                            dur.as_millis(),
                            max_move,
                        );
                        self.out.click(MouseButton::Right);
                    } else {
                        log::debug!(
                            "2f lift, no tap: dur={}ms max_move={:.4}",
                            dur.as_millis(),
                            max_move,
                        );
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

    fn dispatch_one(&mut self, active: &[Contact]) {
        let c = active[0];
        let Some(tr) = self.contacts.get(&c.id) else {
            return;
        };
        let dx = tr.x - tr.prev_x;
        let dy = tr.y - tr.prev_y;
        // Emit the previous frame's deferred motion (if any), then
        // stash this frame's. On lift the `transition` arm clears
        // `pending_motion` without emitting it — that's what drops
        // the centroid-shift jump that capacitive trackpads commonly
        // report on the last with-finger frame.
        if let Some((bdx, bdy)) = self.pending_motion.take() {
            if bdx.abs() > MOTION_DEAD_ZONE || bdy.abs() > MOTION_DEAD_ZONE {
                log::debug!(
                    "cursor: emit deferred d=({:+.4},{:+.4}) (cur frame raw=({},{}))",
                    bdx, bdy, c.raw_x, c.raw_y,
                );
                self.out.move_cursor_by(bdx, bdy);
            }
        }
        self.pending_motion = Some((dx, dy));
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
                    GestureKind::TwoFingerPan => {
                        log::debug!("scroll: began (pan_score={:.2})", pan);
                        self.out.scroll(0.0, 0.0, Phase::Began);
                    }
                    GestureKind::TwoFingerPinch => {
                        log::debug!("pinch: began (pinch_score={:.2})", pinch);
                        self.out.pinch(0.0, Phase::Began);
                    }
                    GestureKind::TwoFingerRotate => {
                        log::debug!("rotate: began (rot_score={:.2})", rot);
                        self.out.rotate(0.0, Phase::Began);
                    }
                    _ => {}
                }
            }
        }

        match self.kind {
            GestureKind::TwoFingerPan => {
                let ddx = centroid.0 - base.last_centroid.0;
                let ddy = centroid.1 - base.last_centroid.1;
                if ddx.abs() > MOTION_DEAD_ZONE || ddy.abs() > MOTION_DEAD_ZONE {
                    log::debug!("scroll: d=({:+.4},{:+.4})", ddx, ddy);
                    self.out.scroll(ddx, ddy, Phase::Changed);
                }
            }
            GestureKind::TwoFingerPinch => {
                let scale = dist / base.initial_distance;
                let delta = scale - base.last_scale_emitted;
                if delta.abs() > 1e-4 {
                    log::debug!("pinch: delta={:+.4} scale={:.4}", delta, scale);
                    self.out.pinch(delta, Phase::Changed);
                    base.last_scale_emitted = scale;
                }
            }
            GestureKind::TwoFingerRotate => {
                let delta = angle_delta(ang, base.last_angle);
                if delta.abs() > 1e-4 {
                    log::debug!("rotate: delta={:+.2}deg", delta.to_degrees());
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
            log::debug!(
                "swipe: {:?} (n_fingers={} centroid_d=({:+.4},{:+.4}))",
                direction,
                active.len(),
                dx,
                dy,
            );
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
        // Cursor motion is deferred by one frame, so a 3-frame sequence
        // is needed for the second frame's motion to surface (the third
        // frame, with a finger still down, drains the buffer). A 2-frame
        // sequence would leave the motion in `pending_motion` and the
        // implicit lift on the next call would drop it.
        s.on_frame(frame(&[(1, 0.5, 0.5)]));
        s.on_frame(frame(&[(1, 0.6, 0.5)]));
        s.on_frame(frame(&[(1, 0.7, 0.5)]));
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

    // ── Scenarios ported from rmk's TrackpadProcessor tests ──
    //
    // These mirror the chip-side trackpad processor's behavioural
    // expectations, translated into normalized [0,1] coordinates and the
    // gesture-engine's coarser-grained API. Some are aspirational — they
    // describe behaviour the chip-side processor has but this engine still
    // lacks. Those are marked `#[ignore]` with a comment naming the gap.
    //
    // The chip-side processor's tap/hold thresholds (`TAP_DIST = 40` chip
    // units on a 3936-wide pad ≈ 0.010 normalized) are close to this
    // engine's `TAP_MAX_MOVE = 0.012`, so motion budgets translate roughly
    // 1:1 after dividing by the pad's logical max.

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// Single-finger touchdown then lift, well under TAP_MAX_DURATION and
    /// without moving — emits a left click.
    #[test]
    fn short_stationary_single_finger_tap_fires_left_click() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        s.on_frame_at(frame(&[(1, 0.5, 0.5)]), t0);
        s.on_frame_at(frame(&[(1, 0.5, 0.5)]), at(t0, 50));
        s.on_frame_at(frame(&[]), at(t0, 100));
        let log = r.pop();
        assert!(log.iter().any(|l| l.contains("click Left")), "{log:?}");
    }

    /// Two-finger touchdown then lift, short and stationary — right click.
    #[test]
    fn short_stationary_two_finger_tap_fires_right_click() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        s.on_frame_at(frame(&[(1, 0.4, 0.5), (2, 0.6, 0.5)]), t0);
        s.on_frame_at(frame(&[]), at(t0, 80));
        let log = r.pop();
        assert!(log.iter().any(|l| l.contains("click Right")), "{log:?}");
    }

    /// Touch held past TAP_MAX_DURATION with no motion — does not tap.
    /// (The chip-side processor would also latch a press-and-hold here;
    /// see `software_press_and_hold_*` tests below for that side.)
    #[test]
    fn long_touch_does_not_fire_tap() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        s.on_frame_at(frame(&[(1, 0.5, 0.5)]), t0);
        // Lift well past 220 ms.
        s.on_frame_at(frame(&[]), at(t0, 400));
        let log = r.pop();
        assert!(
            !log.iter().any(|l| l.starts_with("click")),
            "long touch must not tap ({log:?})",
        );
    }

    /// Single-finger touch with motion exceeding TAP_MAX_MOVE — does not
    /// tap on lift, only emits cursor motion.
    #[test]
    fn motion_laden_touch_does_not_fire_tap() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Move ~0.05 along x — well past TAP_MAX_MOVE = 0.012.
        s.on_frame_at(frame(&[(1, 0.50, 0.50)]), t0);
        s.on_frame_at(frame(&[(1, 0.52, 0.50)]), at(t0, 20));
        s.on_frame_at(frame(&[(1, 0.55, 0.50)]), at(t0, 40));
        s.on_frame_at(frame(&[]), at(t0, 60));
        let log = r.pop();
        assert!(
            !log.iter().any(|l| l.starts_with("click")),
            "motion-laden touch must not tap ({log:?})",
        );
        assert!(
            log.iter().any(|l| l.starts_with("move")),
            "cursor motion should still emit ({log:?})",
        );
    }

    /// Diagonal short touch where every contact stays within TAP_MAX_MOVE
    /// of its landing point still fires a tap. Mirrors rmk's
    /// `diagonal_short_touch_within_radius_fires_tap` — captures real-device
    /// pattern where a finger wobbles diagonally during a brisk tap.
    #[test]
    fn diagonal_short_touch_within_radius_fires_tap() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Series of small diagonal hops; final deviation from start
        // ≈ √(0.007² + 0.006²) ≈ 0.0092, well under TAP_MAX_MOVE = 0.012.
        s.on_frame_at(frame(&[(1, 0.500, 0.500)]), t0);
        s.on_frame_at(frame(&[(1, 0.502, 0.499)]), at(t0, 13));
        s.on_frame_at(frame(&[(1, 0.504, 0.497)]), at(t0, 26));
        s.on_frame_at(frame(&[(1, 0.506, 0.495)]), at(t0, 39));
        s.on_frame_at(frame(&[(1, 0.507, 0.494)]), at(t0, 52));
        s.on_frame_at(frame(&[]), at(t0, 75));
        let log = r.pop();
        assert!(
            log.iter().any(|l| l.contains("click Left")),
            "diagonal short touch should still tap ({log:?})",
        );
    }

    /// Two-finger touch that pans into a scroll then lifts — the lift must
    /// not also fire a right-click tap. Centroid moved well past
    /// TAP_MAX_MOVE so the tap branch on TwoFingerUnclassified→Idle
    /// shouldn't fire either.
    #[test]
    fn scroll_during_touch_does_not_fire_tap() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        s.on_frame_at(frame(&[(1, 0.40, 0.50), (2, 0.60, 0.50)]), t0);
        s.on_frame_at(frame(&[(1, 0.40, 0.55), (2, 0.60, 0.55)]), at(t0, 16));
        s.on_frame_at(frame(&[(1, 0.40, 0.60), (2, 0.60, 0.60)]), at(t0, 32));
        s.on_frame_at(frame(&[]), at(t0, 48));
        let log = r.pop();
        assert!(
            log.iter().any(|l| l.starts_with("scroll") && l.contains("Began")),
            "expected scroll Began ({log:?})",
        );
        assert!(
            !log.iter().any(|l| l.contains("click")),
            "scroll-then-lift must not fire a tap ({log:?})",
        );
    }

    // ── Aspirational specs (mark behaviours rmk has, this engine lacks) ──

    /// Press-and-hold should latch the left button after HOLD_TIME, then
    /// pass cursor motion through with the button held, releasing on lift.
    /// Currently *not* implemented in `gesture.rs` — there's no hold
    /// detection at all, so a >220 ms stationary touch produces nothing.
    /// Port of rmk's `software_press_and_hold_latches_button_then_drags_and_releases`.
    #[test]
    #[ignore = "press-and-hold drag not implemented in gesture.rs"]
    fn software_press_and_hold_latches_button_then_drags_and_releases() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Touch persists past the (yet-to-be-defined) hold threshold,
        // analogous to rmk's HOLD_TIME = 450 ms.
        s.on_frame_at(frame(&[(1, 0.50, 0.50)]), t0);
        s.on_frame_at(frame(&[(1, 0.50, 0.50)]), at(t0, 200));
        s.on_frame_at(frame(&[(1, 0.50, 0.50)]), at(t0, 460));
        // Drag motion under the held button.
        s.on_frame_at(frame(&[(1, 0.51, 0.50)]), at(t0, 475));
        s.on_frame_at(frame(&[(1, 0.52, 0.50)]), at(t0, 488));
        // Lift releases the button.
        s.on_frame_at(frame(&[]), at(t0, 501));
        let log = r.pop();
        // Expect: a synthesized button-1 press (NOT a one-shot click),
        // then move events, then a release. Today there's no API on the
        // Output trait for "press" vs. "click" — adding press-and-hold
        // will require extending `Output` with `mouse_down`/`mouse_up`
        // (or similar) and routing them from `gesture.rs`.
        assert!(
            log.iter().any(|l| l.contains("press") || l.contains("down")),
            "expected explicit button press from hold latch ({log:?})",
        );
        assert!(
            log.iter().any(|l| l.starts_with("move")),
            "expected drag motion under held button ({log:?})",
        );
        assert!(
            log.iter().any(|l| l.contains("release") || l.contains("up")),
            "expected button release on lift ({log:?})",
        );
    }

    /// Press-and-hold must not latch when the touch moves enough to
    /// disqualify, nor for two-finger sessions (those are reserved for
    /// scroll/right-click). Port of rmk's
    /// `software_press_and_hold_does_not_latch_with_motion_or_two_fingers`.
    #[test]
    #[ignore = "press-and-hold drag not implemented in gesture.rs"]
    fn software_press_and_hold_does_not_latch_with_motion_or_two_fingers() {
        // Motion past TAP_MAX_MOVE before the hold window — no latch.
        {
            let r = Recorder::default();
            let mut s = State::new(&r);
            let t0 = Instant::now();
            s.on_frame_at(frame(&[(1, 0.50, 0.50)]), t0);
            s.on_frame_at(frame(&[(1, 0.55, 0.55)]), at(t0, 30));
            s.on_frame_at(frame(&[(1, 0.55, 0.55)]), at(t0, 460));
            let log = r.pop();
            assert!(
                !log.iter().any(|l| l.contains("press") || l.contains("down")),
                "motion past TAP_MAX_MOVE must not latch a hold ({log:?})",
            );
        }

        // Two-finger sessions never latch a hold.
        {
            let r = Recorder::default();
            let mut s = State::new(&r);
            let t0 = Instant::now();
            s.on_frame_at(frame(&[(1, 0.40, 0.50), (2, 0.60, 0.50)]), t0);
            s.on_frame_at(frame(&[(1, 0.40, 0.50), (2, 0.60, 0.50)]), at(t0, 460));
            let log = r.pop();
            assert!(
                !log.iter().any(|l| l.contains("press") || l.contains("down")),
                "two-finger touch must not latch a hold ({log:?})",
            );
        }
    }

    /// On finger lift, the last frame's motion is commonly a centroid-shift
    /// artifact (the contact patch shrinks asymmetrically) and should not
    /// be emitted as cursor motion. The engine buffers `dispatch_one`
    /// motion by one frame and drops the buffered value on the lift
    /// transition.
    ///
    /// Port of rmk's `lift_suppresses_prior_frame_centroid_shift_jump`.
    #[test]
    fn lift_suppresses_prior_frame_centroid_shift_jump() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Normal tracking motion at 0.005/frame.
        s.on_frame_at(frame(&[(1, 0.500, 0.500)]), t0);
        s.on_frame_at(frame(&[(1, 0.505, 0.500)]), at(t0, 13));
        s.on_frame_at(frame(&[(1, 0.510, 0.500)]), at(t0, 26));
        // Final frame with finger reports a big centroid-shift jump.
        s.on_frame_at(frame(&[(1, 0.560, 0.500)]), at(t0, 39));
        // Lift.
        s.on_frame_at(frame(&[]), at(t0, 52));

        let log = r.pop();
        // Each emitted move dx should be ≤ 0.01 — i.e. the 0.05 jump on
        // the last-with-finger frame must be suppressed.
        for line in &log {
            if let Some(rest) = line.strip_prefix("move ") {
                let dx: f64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                assert!(
                    dx.abs() <= 0.01,
                    "lift-frame centroid jump leaked into cursor ({line})",
                );
            }
        }
    }

    /// When a second finger lands during a one-finger touch, finger 0
    /// commonly drifts as the hand settles into the scroll posture.
    /// Cursor must not jump on those settling frames — gesture mode
    /// transitions to TwoFingerUnclassified before the user actually
    /// commits to panning.
    ///
    /// Port of rmk's `pre_scroll_two_finger_settling_does_not_emit_cursor`.
    #[test]
    fn pre_scroll_two_finger_settling_does_not_emit_cursor() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();

        // Touchdown: 1 finger. Engine enters OneFinger mode; no motion yet.
        s.on_frame_at(frame(&[(1, 0.323, 0.535)]), t0);
        // Drain anything spurious before the second finger lands.
        let _ = r.pop();

        // Second finger lands. On this frame the engine transitions to
        // TwoFingerUnclassified — dispatch_one should NOT run for finger
        // 0's drift.
        s.on_frame_at(
            frame(&[(1, 0.323, 0.535), (2, 0.505, 0.453)]),
            at(t0, 25),
        );
        // Subsequent settling frames: finger 0 drifts, both fingers track
        // together but slowly; centroid hasn't moved enough to lock pan.
        s.on_frame_at(
            frame(&[(1, 0.322, 0.535), (2, 0.505, 0.452)]),
            at(t0, 41),
        );
        s.on_frame_at(
            frame(&[(1, 0.321, 0.534), (2, 0.504, 0.450)]),
            at(t0, 56),
        );
        let log = r.pop();
        assert!(
            !log.iter().any(|l| l.starts_with("move ")),
            "pre-scroll two-finger settling must not emit cursor motion ({log:?})",
        );
    }
}
