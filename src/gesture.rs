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

// ---- Tunables ----
//
// Distance thresholds are in physical millimeters. They translate
// directly across pads of any density / aspect ratio because contacts
// arrive in mm (the decoder applies per-axis chip-px → mm scaling
// using `Layout::physical_*_max_mm`). Numbers are calibrated to human
// finger ergonomics, not pad fractions.

/// Max distance a contact may drift from its landing point during a
/// short touch and still count as a tap. Matches rmk's `TAP_DIST = 40`
/// chip units (≈ 0.66 mm on its 65 mm pad). Going looser added a
/// noticeable latency to scroll onset — every chip frame whose
/// per-contact drift hadn't yet crossed this threshold delayed the
/// `TwoFingerUnclassified → TwoFingerPan` lock — so we match rmk
/// even though it's slightly less tap-forgiving than macOS conventions.
const TAP_MAX_MOVE_MM: f64 = 0.66;
/// Max touch duration to count as a tap. Matches rmk's `TAP_TIME` for
/// the same reason as `TAP_MAX_MOVE_MM`: a longer window pushes scroll
/// onset out by the same amount on slow / barely-moving touches.
const TAP_MAX_DURATION: Duration = Duration::from_millis(150);
/// Centroid motion below this between frames is treated as jitter.
const MOTION_DEAD_ZONE_MM: f64 = 0.04;

/// Centroid pan distance needed to lock 2F mode = pan.
const PAN_LOCK_MM: f64 = 0.4;
/// Distance-change ratio needed to lock 2F mode = pinch (unitless).
const PINCH_LOCK_RATIO: f64 = 0.04;
/// Angle change (radians) needed to lock 2F mode = rotate.
const ROTATE_LOCK_RAD: f64 = 6.0_f64 * std::f64::consts::PI / 180.0;

/// Centroid travel needed to fire a 3F or 4F swipe.
const SWIPE_TRIGGER_MM: f64 = 5.0;

/// EMA weight on the freshest velocity sample during 2F pan, in [0, 1].
/// 0.4 ≈ 2.5-frame averaging window on a ~125 Hz pad — fast enough to
/// catch a flick, slow enough that one noisy chip frame doesn't dominate
/// the inertia seed. Mirrors rmk's `VEL_EMA_NUM/VEL_EMA_DEN = 96/256`.
const SCROLL_VELOCITY_ALPHA: f64 = 0.4;

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
    /// EMA-smoothed centroid velocity in mm/sec, sampled while in
    /// `TwoFingerPan`. Seeds inertia at lift via `Output::scroll_inertia`
    /// — modeled on rmk's `TrackpadProcessor` velocity track.
    scroll_velocity: (f64, f64),
    /// Time of the most recent scroll-event emission. Combined with the
    /// new event's timestamp to compute the per-frame dt that turns a
    /// per-frame mm delta into a mm/sec velocity sample.
    last_scroll_time: Option<Instant>,
}

#[derive(Clone, Copy, Debug)]
struct MultiBaseline {
    initial_centroid: (f64, f64),
}

/// Carry-over state from a 2F touch whose fingers lifted asynchronously
/// (one before the other). Captured at the
/// TwoFingerUnclassified → OneFinger transition; consumed at the
/// subsequent OneFinger → Idle transition. While set, the residual 1F
/// touch is not eligible to fire its own left-click — it's the tail of
/// the 2F gesture, not a fresh 1F tap.
#[derive(Clone, Copy, Debug)]
struct PendingTwoFingerTap {
    /// When the 2F gesture (not the residual 1F) began. Used to decide
    /// whether the right-click is still in the original tap window.
    started_at: Instant,
    /// Worst-case per-contact motion observed during the 2F phase.
    /// Combined (via max) with the residual 1F's `max_move_sq` to
    /// decide whether to fire the right-click.
    max_move_sq: f64,
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
    pending_two_finger_tap: Option<PendingTwoFingerTap>,
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
            pending_two_finger_tap: None,
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
            self.dispatch(&active, now);
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
        // First contact after Idle cancels any in-flight scroll inertia.
        // `SwipeLatched → Idle → ...` doesn't count: a deliberate new
        // touch has to come from no-fingers, and the user wants their
        // touch to stop a fling rather than blend into it.
        if matches!(self.kind, GestureKind::Idle)
            && !matches!(new_kind, GestureKind::Idle | GestureKind::SwipeLatched)
        {
            self.out.cancel_inertia();
        }
        // Close out the old gesture.
        match self.kind {
            GestureKind::OneFinger => {
                // Drop any deferred cursor motion. On a transition to
                // Idle this is the chip's last with-finger frame's
                // motion (often a centroid-shift artifact); on a
                // transition to TwoFinger* it's stale single-finger
                // motion that's no longer meaningful.
                let dropped = self.pending_motion.take();
                // A pending two-finger tap that doesn't get consumed by
                // an Idle transition (e.g. the residual finger gets
                // joined by a third — back to a 2F gesture) must be
                // discarded; the 2F lift sequence is over.
                let pending_2f = self.pending_two_finger_tap.take();
                if matches!(new_kind, GestureKind::Idle) {
                    if let Some(p) = pending_2f {
                        // Residual 1F is the tail of an asynchronous 2F
                        // lift. Combine the 2F window/motion with the
                        // residual's to decide whether the right-click
                        // still qualifies; either way, suppress the
                        // residual's own left-click path.
                        let total_dur = now - p.started_at;
                        let combined_max_move = p.max_move_sq.max(self.max_move_sq).sqrt();
                        if total_dur < TAP_MAX_DURATION && combined_max_move < TAP_MAX_MOVE_MM {
                            log::debug!(
                                "2f tap (split lift): click Right (total_dur={}ms combined_max_move={:.2}mm)",
                                total_dur.as_millis(),
                                combined_max_move,
                            );
                            self.out.click(MouseButton::Right);
                        } else {
                            log::debug!(
                                "2f tap (split lift): no click (total_dur={}ms combined_max_move={:.2}mm)",
                                total_dur.as_millis(),
                                combined_max_move,
                            );
                        }
                    } else {
                        let dur = now - self.started_at;
                        let max_move = self.max_move_sq.sqrt();
                        if dur < TAP_MAX_DURATION && max_move < TAP_MAX_MOVE_MM {
                            log::debug!(
                                "1f tap: click Left (dur={}ms max_move={:.2}mm{})",
                                dur.as_millis(),
                                max_move,
                                if dropped.is_some() { ", dropped lift-frame motion" } else { "" },
                            );
                            self.out.click(MouseButton::Left);
                        } else {
                            log::debug!(
                                "1f lift, no tap: dur={}ms max_move={:.2}mm (limits dur<{}ms move<{:.2}mm)",
                                dur.as_millis(),
                                max_move,
                                TAP_MAX_DURATION.as_millis(),
                                TAP_MAX_MOVE_MM,
                            );
                        }
                    }
                }
            }
            GestureKind::TwoFingerPan => {
                let (vx, vy) = self
                    .two_baseline
                    .map(|b| b.scroll_velocity)
                    .unwrap_or((0.0, 0.0));
                let speed = (vx * vx + vy * vy).sqrt();
                log::debug!(
                    "scroll: ended (v=({:+.0},{:+.0})mm/s speed={:.0}mm/s)",
                    vx, vy, speed,
                );
                self.out.scroll(0.0, 0.0, Phase::Ended);
                // Seed inertia from the lift velocity. `Output` decides
                // whether the seed is fast enough to coast on; gesture-side
                // we always offer it.
                self.out.scroll_inertia(vx, vy);
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
                let dur = now - self.started_at;
                let max_move = self.max_move_sq.sqrt();
                let tap_eligible = dur < TAP_MAX_DURATION && max_move < TAP_MAX_MOVE_MM;
                if matches!(new_kind, GestureKind::Idle) {
                    if tap_eligible {
                        log::debug!(
                            "2f tap: click Right (dur={}ms max_move={:.2}mm)",
                            dur.as_millis(),
                            max_move,
                        );
                        self.out.click(MouseButton::Right);
                    } else {
                        log::debug!(
                            "2f lift, no tap: dur={}ms max_move={:.2}mm",
                            dur.as_millis(),
                            max_move,
                        );
                    }
                } else if matches!(new_kind, GestureKind::OneFinger) && tap_eligible {
                    // One finger lifted while the 2F was still tap-eligible.
                    // Stash the 2F window/motion so the next OneFinger → Idle
                    // can fire the right-click; until then, the residual 1F
                    // is part of this gesture, not a fresh 1F tap.
                    log::debug!(
                        "2f → 1f partial lift (dur={}ms max_move={:.2}mm); pending right-click",
                        dur.as_millis(),
                        max_move,
                    );
                    self.pending_two_finger_tap = Some(PendingTwoFingerTap {
                        started_at: self.started_at,
                        max_move_sq: self.max_move_sq,
                    });
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
                    scroll_velocity: (0.0, 0.0),
                    last_scroll_time: None,
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

    fn dispatch(&mut self, active: &[Contact], now: Instant) {
        match self.kind {
            GestureKind::Idle | GestureKind::SwipeLatched => {}
            GestureKind::OneFinger => self.dispatch_one(active, now),
            GestureKind::TwoFingerUnclassified
            | GestureKind::TwoFingerPan
            | GestureKind::TwoFingerPinch
            | GestureKind::TwoFingerRotate => self.dispatch_two(active, now),
            GestureKind::ThreeFingerLive | GestureKind::FourFingerLive => self.dispatch_swipe(active),
        }
    }

    fn dispatch_one(&mut self, active: &[Contact], now: Instant) {
        let c = active[0];
        let Some(tr) = self.contacts.get(&c.id) else {
            return;
        };

        // Hold cursor motion until this touch is committed to "not a tap".
        // Per-frame finger jitter inside the tap budget would otherwise
        // drag the cursor away from where the user expected the click to
        // land. The touch becomes cursor-eligible the moment its
        // cumulative drift exceeds TAP_MAX_MOVE_MM or its duration exceeds
        // TAP_MAX_DURATION — both checks live here (not just at lift)
        // because a held-then-dragged finger should also start moving the
        // cursor once the tap window closes. Pre-commit frames clear
        // `pending_motion` so no stale buffered delta leaks out the
        // moment we cross the threshold.
        let max_move = tr.max_move_sq.sqrt();
        let dur = now - self.started_at;
        let could_still_tap = max_move < TAP_MAX_MOVE_MM && dur < TAP_MAX_DURATION;
        if could_still_tap {
            self.pending_motion = None;
            return;
        }

        let dx = tr.x - tr.prev_x;
        let dy = tr.y - tr.prev_y;
        // Emit the previous frame's deferred motion (if any), then
        // stash this frame's. On lift the `transition` arm clears
        // `pending_motion` without emitting it — that's what drops
        // the centroid-shift jump that capacitive trackpads commonly
        // report on the last with-finger frame.
        if let Some((bdx, bdy)) = self.pending_motion.take() {
            if bdx.abs() > MOTION_DEAD_ZONE_MM || bdy.abs() > MOTION_DEAD_ZONE_MM {
                log::debug!(
                    "cursor: emit deferred d=({:+.3},{:+.3})mm (cur frame at=({:.2},{:.2})mm)",
                    bdx, bdy, c.x, c.y,
                );
                self.out.move_cursor_by(bdx, bdy);
            }
        }
        self.pending_motion = Some((dx, dy));
    }

    fn dispatch_two(&mut self, active: &[Contact], now: Instant) {
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

        // Lock mode if not yet locked. Same could-still-tap gate as
        // dispatch_one: PAN_LOCK_MM (0.4) sits below TAP_MAX_MOVE_MM
        // (1.0), so without this check a 2F tap with synchronized
        // sub-mm centroid drift would lock pan mid-tap and start
        // emitting scroll events — and the right-click would never
        // fire on lift, since the kind would no longer be
        // TwoFingerUnclassified. `self.max_move_sq` tracks the worst
        // per-contact drift across the gesture, so it correctly gates
        // on either finger crossing the tap budget.
        if matches!(self.kind, GestureKind::TwoFingerUnclassified) {
            let max_move = self.max_move_sq.sqrt();
            let dur = now - self.started_at;
            let could_still_tap = max_move < TAP_MAX_MOVE_MM && dur < TAP_MAX_DURATION;
            if could_still_tap {
                base.last_centroid = centroid;
                base.last_angle = ang;
                self.two_baseline = Some(base);
                return;
            }
            let pan = ((centroid.0 - base.initial_centroid.0).powi(2)
                + (centroid.1 - base.initial_centroid.1).powi(2))
            .sqrt()
                / PAN_LOCK_MM;
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
                if ddx.abs() > MOTION_DEAD_ZONE_MM || ddy.abs() > MOTION_DEAD_ZONE_MM {
                    // EMA-track centroid velocity for the inertia seed.
                    // Skip the very first sample (no prior time → no
                    // meaningful dt); the next emit picks up the EMA.
                    if let Some(prev_t) = base.last_scroll_time {
                        let dt = (now - prev_t).as_secs_f64().max(1e-3);
                        let inst_vx = ddx / dt;
                        let inst_vy = ddy / dt;
                        base.scroll_velocity.0 = SCROLL_VELOCITY_ALPHA * inst_vx
                            + (1.0 - SCROLL_VELOCITY_ALPHA) * base.scroll_velocity.0;
                        base.scroll_velocity.1 = SCROLL_VELOCITY_ALPHA * inst_vy
                            + (1.0 - SCROLL_VELOCITY_ALPHA) * base.scroll_velocity.1;
                    }
                    base.last_scroll_time = Some(now);
                    log::debug!(
                        "scroll: d=({:+.3},{:+.3})mm v=({:+.0},{:+.0})mm/s",
                        ddx, ddy, base.scroll_velocity.0, base.scroll_velocity.1,
                    );
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
            if dx >= SWIPE_TRIGGER_MM {
                Some(SwipeDirection::Right)
            } else if dx <= -SWIPE_TRIGGER_MM {
                Some(SwipeDirection::Left)
            } else {
                None
            }
        } else if dy >= SWIPE_TRIGGER_MM {
            Some(SwipeDirection::Down)
        } else if dy <= -SWIPE_TRIGGER_MM {
            Some(SwipeDirection::Up)
        } else {
            None
        };

        if let Some(direction) = dir {
            log::debug!(
                "swipe: {:?} (n_fingers={} centroid_d=({:+.2},{:+.2})mm)",
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
        fn scroll_inertia(&self, vx: f64, vy: f64) {
            self.log
                .borrow_mut()
                .push(format!("scroll_inertia {vx:.4} {vy:.4}"));
        }
        fn cancel_inertia(&self) {
            self.log.borrow_mut().push("cancel_inertia".to_string());
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

    /// Tests pre-date the chip-px → mm migration: their coordinates are
    /// expressed as [0,1] fractions of a notional pad. The helper scales
    /// them onto a square 50 × 50 mm "test pad" so the engine sees the
    /// physical units it now expects. 50 mm is roughly the X dimension
    /// of the SoflePLUS2 (49 mm) and gives sensible mm budgets for the
    /// `0.001`-level deltas in tests like `pre_scroll_two_finger_settling`
    /// (~0.05 mm) and `lift_suppresses_prior_frame_centroid_shift_jump`
    /// (~0.25 mm normal motion vs. 2.5 mm lift jump).
    const TEST_PAD_MM: f64 = 50.0;

    fn frame(contacts: &[(u8, f64, f64)]) -> Frame {
        Frame {
            contacts: contacts
                .iter()
                .map(|&(id, nx, ny)| Contact {
                    id,
                    x: nx * TEST_PAD_MM,
                    y: ny * TEST_PAD_MM,
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
    // expectations, expressed via the same `frame()` helper (so the [0,1]
    // values get scaled onto the 50 mm test pad). Some are aspirational
    // — they describe behaviour the chip-side processor has but this
    // engine still lacks. Those are marked `#[ignore]` with a comment
    // naming the gap.
    //
    // Threshold parity: rmk's `TAP_DIST = 40` chip units on a 3936-wide,
    // 65 mm pad ≈ 0.66 mm — close to this engine's
    // `TAP_MAX_MOVE_MM = 1.0`. Slight conservatism here, since macOS
    // users expect taps to be forgiving of minor finger drift.

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

    /// Single-finger touch with motion exceeding TAP_MAX_MOVE_MM — does not
    /// tap on lift, only emits cursor motion.
    #[test]
    fn motion_laden_touch_does_not_fire_tap() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Move ~2.5 mm along x (0.05 fraction of the 50 mm test pad)
        // — well past TAP_MAX_MOVE_MM = 1.0.
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

    /// Diagonal short touch where every contact stays within TAP_MAX_MOVE_MM
    /// of its landing point still fires a tap. Mirrors rmk's
    /// `diagonal_short_touch_within_radius_fires_tap` — captures real-device
    /// pattern where a finger wobbles diagonally during a brisk tap.
    #[test]
    fn diagonal_short_touch_within_radius_fires_tap() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Series of small diagonal hops; final deviation from start
        // ≈ √(0.007² + 0.006²) × 50 mm ≈ 0.46 mm, well under
        // TAP_MAX_MOVE_MM = 1.0.
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
    /// TAP_MAX_MOVE_MM so the tap branch on TwoFingerUnclassified→Idle
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
        // Motion past TAP_MAX_MOVE_MM before the hold window — no latch.
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
                "motion past TAP_MAX_MOVE_MM must not latch a hold ({log:?})",
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
        // Open with motion well past TAP_MAX_MOVE_MM so the could-still-tap
        // gate releases on frame 2 — otherwise no `move` lines emit and
        // the assertion has nothing to check. Then steady 2.5 mm/frame of
        // tracking motion before a 7.5 mm final-with-finger jump (the
        // artifact this test exists to suppress).
        s.on_frame_at(frame(&[(1, 0.500, 0.500)]), t0);
        s.on_frame_at(frame(&[(1, 0.550, 0.500)]), at(t0, 13));
        s.on_frame_at(frame(&[(1, 0.600, 0.500)]), at(t0, 26));
        s.on_frame_at(frame(&[(1, 0.650, 0.500)]), at(t0, 39));
        // Final with-finger frame: 7.5 mm jump.
        s.on_frame_at(frame(&[(1, 0.800, 0.500)]), at(t0, 52));
        // Lift.
        s.on_frame_at(frame(&[]), at(t0, 65));

        let log = r.pop();
        let moves: Vec<&String> = log.iter().filter(|l| l.starts_with("move ")).collect();
        assert!(!moves.is_empty(), "test must emit some move lines to be meaningful: {log:?}");
        // Tracking deltas are 2.5 mm; the lift-frame jump is 7.5 mm. A 5 mm
        // ceiling separates the two — anything above is the artifact leaking.
        for line in &moves {
            if let Some(rest) = line.strip_prefix("move ") {
                let dx: f64 = rest
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                assert!(
                    dx.abs() <= 5.0,
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

    /// Captures the user-reported regression: small finger drift during a
    /// brisk tap (well inside both TAP_MAX_MOVE_MM = 1.0 and
    /// TAP_MAX_DURATION = 220 ms) must not push the cursor before the
    /// click lands. Pre-fix, per-frame deltas above MOTION_DEAD_ZONE_MM
    /// (0.04 mm) leaked through `dispatch_one` even when the touch was
    /// destined to resolve as a tap, so the click registered at a
    /// shifted location. The could-still-tap gate in `dispatch_one`
    /// holds cursor motion until the touch is committed to "not a tap".
    #[test]
    fn small_drift_during_tap_does_not_move_cursor() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Recreates the captured trace: ~0.13 mm total drift over 4
        // frames, lift in 70 ms — clearly a tap, but per-frame Δy hovers
        // at the dead-zone boundary. Note the helper is fed mm
        // directly here (not [0,1] fractions) so the drift figures
        // match the bug report 1:1.
        let frame_at_mm = |x: f64, y: f64| Frame {
            contacts: vec![Contact {
                id: 0,
                x,
                y,
                tip: true,
                confidence: true,
            }],
            scan_time_100us: 0,
            button: false,
        };
        s.on_frame_at(frame_at_mm(35.70, 39.04), t0);
        s.on_frame_at(frame_at_mm(35.67, 39.02), at(t0, 17));
        s.on_frame_at(frame_at_mm(35.65, 38.97), at(t0, 31));
        s.on_frame_at(frame_at_mm(35.63, 38.93), at(t0, 47));
        s.on_frame_at(Frame { contacts: vec![], scan_time_100us: 0, button: false }, at(t0, 70));

        let log = r.pop();
        assert!(
            !log.iter().any(|l| l.starts_with("move ")),
            "tap-eligible drift must not move cursor ({log:?})",
        );
        assert!(
            log.iter().any(|l| l.contains("click Left")),
            "tap should still fire ({log:?})",
        );
    }

    /// Scroll-end always seeds inertia with the EMA-smoothed velocity at
    /// lift; the `Output` decides whether the seed is fast enough to coast.
    /// Gesture-side responsibility: emit the call exactly once per
    /// scroll session, after the matching `scroll(.., Ended)`.
    #[test]
    fn scroll_lift_seeds_inertia() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Two-finger pan moving 5 mm/16ms ≈ 312 mm/s — well above any
        // sane seed threshold the Output side might apply.
        s.on_frame_at(frame(&[(1, 0.4, 0.5), (2, 0.6, 0.5)]), t0);
        s.on_frame_at(frame(&[(1, 0.4, 0.55), (2, 0.6, 0.55)]), at(t0, 16));
        s.on_frame_at(frame(&[(1, 0.4, 0.6), (2, 0.6, 0.6)]), at(t0, 32));
        s.on_frame_at(frame(&[(1, 0.4, 0.65), (2, 0.6, 0.65)]), at(t0, 48));
        s.on_frame_at(frame(&[]), at(t0, 64));
        let log = r.pop();
        let inertia: Vec<&String> = log
            .iter()
            .filter(|l| l.starts_with("scroll_inertia"))
            .collect();
        assert_eq!(inertia.len(), 1, "expected one inertia seed ({log:?})");
        // After 3 motion frames at +2.5 mm/16ms each, the EMA should be
        // tracking somewhere near +156 mm/s on Y. Don't pin the exact
        // value — EMA dynamics depend on how many samples land before
        // lift — but we should at least see a non-trivial Y velocity
        // and a near-zero X.
        let line = inertia[0];
        let parts: Vec<&str> = line.split_whitespace().collect();
        let vx: f64 = parts[1].parse().unwrap();
        let vy: f64 = parts[2].parse().unwrap();
        assert!(vy.abs() > 50.0, "expected Y velocity > 50 mm/s, got {vy} ({line})");
        assert!(vx.abs() < 50.0, "expected near-zero X velocity, got {vx} ({line})");
    }

    /// First contact after a fully-released gesture must cancel any
    /// in-flight inertia coast — otherwise a tap on the pad would
    /// "blend into" a fling instead of stopping it.
    #[test]
    fn new_touch_cancels_inertia() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Idle → 1F triggers cancel_inertia.
        s.on_frame_at(frame(&[(1, 0.5, 0.5)]), t0);
        let log = r.pop();
        assert!(
            log.iter().any(|l| l == "cancel_inertia"),
            "expected cancel_inertia on first touch ({log:?})",
        );
    }

    /// 2F analogue of `small_drift_during_tap_does_not_move_cursor`. A
    /// brief two-finger tap with synchronized sub-mm centroid drift sits
    /// above PAN_LOCK_MM (0.4 mm) but below TAP_MAX_MOVE_MM (1.0 mm), so
    /// pre-fix the lock branch would commit to TwoFingerPan and start
    /// emitting scroll events — and the lift would no longer fire the
    /// right-click (transition arm only checks for it from
    /// TwoFingerUnclassified). The could-still-tap gate in
    /// `dispatch_two` keeps the kind unclassified until the tap window
    /// closes.
    #[test]
    fn small_drift_during_two_finger_tap_does_not_lock_or_scroll() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        // Both fingers drift ~0.5 mm in the same direction over four
        // frames — centroid pan ~0.5 mm (above PAN_LOCK_MM = 0.4) but
        // each finger's max_move ~0.5 mm (under TAP_MAX_MOVE_MM = 1.0).
        let frame_at_mm = |a: (f64, f64), b: (f64, f64)| Frame {
            contacts: vec![
                Contact { id: 0, x: a.0, y: a.1, tip: true, confidence: true },
                Contact { id: 1, x: b.0, y: b.1, tip: true, confidence: true },
            ],
            scan_time_100us: 0,
            button: false,
        };
        s.on_frame_at(frame_at_mm((20.0, 30.0), (35.0, 30.0)), t0);
        s.on_frame_at(frame_at_mm((20.15, 30.15), (35.15, 30.15)), at(t0, 17));
        s.on_frame_at(frame_at_mm((20.30, 30.30), (35.30, 30.30)), at(t0, 34));
        s.on_frame_at(frame_at_mm((20.45, 30.45), (35.45, 30.45)), at(t0, 51));
        s.on_frame_at(Frame { contacts: vec![], scan_time_100us: 0, button: false }, at(t0, 75));

        let log = r.pop();
        assert!(
            !log.iter().any(|l| l.starts_with("scroll")),
            "tap-eligible 2F drift must not lock pan ({log:?})",
        );
        assert!(
            !log.iter().any(|l| l.starts_with("pinch") || l.starts_with("rotate")),
            "tap-eligible 2F drift must not lock pinch/rotate ({log:?})",
        );
        assert!(
            log.iter().any(|l| l.contains("click Right")),
            "right-click should still fire on lift ({log:?})",
        );
    }

    /// 2F tap where the two fingers don't lift in the same frame —
    /// captured from a real device trace where one finger went tip=false
    /// at t=65 ms and the other at t=77 ms (12 ms gap, well within human
    /// release tolerance). Pre-fix the engine treated the residual 12 ms
    /// of 1F as a fresh single-finger tap and fired Left; the fix
    /// recognizes the residual as the tail of the 2F lift sequence and
    /// fires Right (or, if the residual sits past the tap window,
    /// nothing).
    #[test]
    fn two_finger_tap_with_split_lift_fires_right_not_left() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        let one = |id, x, y, tip| Contact { id, x, y, tip, confidence: true };
        let two = |a: Contact, b: Contact| Frame {
            contacts: vec![a, b],
            scan_time_100us: 0,
            button: false,
        };
        let single = |c: Contact| Frame {
            contacts: vec![c],
            scan_time_100us: 0,
            button: false,
        };
        // t=0: id=0 lands.
        s.on_frame_at(single(one(0, 15.53, 35.84, true)), t0);
        // t=19: id=1 lands → 2F.
        s.on_frame_at(
            two(one(0, 15.53, 35.84, true), one(1, 31.80, 29.50, true)),
            at(t0, 19),
        );
        // t=50: still 2F.
        s.on_frame_at(
            two(one(0, 15.50, 35.84, true), one(1, 31.80, 29.50, true)),
            at(t0, 50),
        );
        // t=65: id=0 goes tip=false (still appears in report). The
        // engine sees 1 active contact → transitions to OneFinger and
        // stashes the pending right-click.
        s.on_frame_at(
            two(one(0, 15.50, 35.84, false), one(1, 31.80, 29.50, true)),
            at(t0, 65),
        );
        // t=77: id=1 also lifts. OneFinger → Idle consumes the pending
        // right-click.
        s.on_frame_at(single(one(1, 31.80, 29.50, false)), at(t0, 77));

        let log = r.pop();
        assert!(
            log.iter().any(|l| l.contains("click Right")),
            "split-lift 2F tap should fire Right ({log:?})",
        );
        assert!(
            !log.iter().any(|l| l.contains("click Left")),
            "split-lift 2F tap must not also fire Left ({log:?})",
        );
    }

    /// If the residual 1F finger sits past the original 2F tap window,
    /// the right-click is no longer eligible — and crucially, the
    /// residual must not fall through to fire its own left-click, since
    /// it's still part of the 2F lift sequence (the user didn't intend
    /// a 1F tap).
    #[test]
    fn two_finger_tap_with_long_residual_fires_nothing() {
        let r = Recorder::default();
        let mut s = State::new(&r);
        let t0 = Instant::now();
        let one = |id, x, y, tip| Contact { id, x, y, tip, confidence: true };
        let two = |a: Contact, b: Contact| Frame {
            contacts: vec![a, b],
            scan_time_100us: 0,
            button: false,
        };
        let single = |c: Contact| Frame {
            contacts: vec![c],
            scan_time_100us: 0,
            button: false,
        };
        s.on_frame_at(single(one(0, 20.0, 30.0, true)), t0);
        s.on_frame_at(
            two(one(0, 20.0, 30.0, true), one(1, 35.0, 30.0, true)),
            at(t0, 20),
        );
        // First finger lifts at t=80 (still 2F-tap-eligible).
        s.on_frame_at(
            two(one(0, 20.0, 30.0, false), one(1, 35.0, 30.0, true)),
            at(t0, 80),
        );
        // Residual 1F holds stationary until t=400 — past the 220 ms
        // total window measured from the 2F start.
        s.on_frame_at(single(one(1, 35.0, 30.0, false)), at(t0, 400));

        let log = r.pop();
        assert!(
            !log.iter().any(|l| l.starts_with("click")),
            "long residual must fire neither Right nor Left ({log:?})",
        );
    }
}
