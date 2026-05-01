//! macOS event synthesis. Public CGEvent APIs handle cursor, click, and
//! phased smooth scroll. The private path (gated by [`Config::private_gestures`])
//! injects pinch, rotate, and swipe via undocumented CGEvent type/field
//! IDs that BetterTouchTool, Karabiner-Elements, and similar tools have
//! used for years — stable across recent macOS versions but not in any
//! public Apple header.
//!
//! All field IDs and event-type constants used here are reverse-engineered
//! from NSEvent type values; see the comments next to each declaration.

#![allow(non_upper_case_globals)]

use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopDefaultMode};
use core_foundation_sys::runloop::{
    CFRunLoopAddTimer, CFRunLoopTimerContext, CFRunLoopTimerCreate, CFRunLoopTimerInvalidate,
    CFRunLoopTimerRef,
};
use core_graphics::geometry::CGPoint;
use std::cell::Cell;
use std::ffi::c_void;
use std::time::{Duration, Instant};

/// Hardcoded macOS double-click interval. Configurable in System
/// Settings (Mouse → Double-Click Speed) but rarely changed; querying
/// `CGEventSourceGetDoubleClickInterval` on every click is overkill,
/// and the default works for almost everyone. Promote to a CLI flag
/// if a user needs to tune it.
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);
/// Maximum cursor displacement (pixels) between successive clicks for
/// them to count as a multi-click sequence. macOS doesn't expose this
/// as a public API; 5 px matches stock NSEvent behaviour. Without this
/// guard, a click → drift → click sequence would still register as
/// double-click when it should reset to 1.
const DOUBLE_CLICK_DISTANCE_PX: f64 = 5.0;

/// Inertia / coast tunables.
///
/// Modeled on rmk's `TrackpadProcessor`: an EMA-smoothed velocity sampled
/// during active scroll seeds an exponential decay after the user lifts.
/// The values aren't a direct port — rmk works in chip units per chip
/// cycle, and we work in mm/s with a wall-clock timer — but the wall-clock
/// half-life and stop time roughly match.
///
/// `MOMENTUM_TICK_HZ` drives the CFRunLoopTimer that posts momentum-phase
/// scroll events while coasting. 60 Hz is the natural cadence for
/// momentum-aware UIs and keeps each event small enough to feel smooth.
const MOMENTUM_TICK_HZ: f64 = 60.0;
const MOMENTUM_TICK_INTERVAL: f64 = 1.0 / MOMENTUM_TICK_HZ;
/// Per-second velocity decay multiplier during coast. 0.05 means velocity
/// drops to 5% of its lift value over one second — wall-clock half-life
/// of ~230 ms, full-stop within ~1 s for a typical flick. Tweak by ear.
const MOMENTUM_DECAY_PER_SEC: f64 = 0.05;
/// Speed below which momentum stops emitting (mm/s). Anything smaller
/// would round to under a pixel per tick at the default scroll_accel
/// and just look like jitter trailing off.
const MOMENTUM_STOP_MM_PER_SEC: f64 = 5.0;
/// Speed required at scroll-end to seed inertia (mm/s). Avoids "ghost"
/// coasts from a slow drag that the user wasn't trying to fling.
const MOMENTUM_SEED_MM_PER_SEC: f64 = 25.0;

// ---------- Public CGEvent constants (mirrored from CGEventTypes.h) ----------

const kCGEventLeftMouseDown: u32 = 1;
const kCGEventLeftMouseUp: u32 = 2;
const kCGEventRightMouseDown: u32 = 3;
const kCGEventRightMouseUp: u32 = 4;
const kCGEventMouseMoved: u32 = 5;

const kCGMouseButtonLeft: u32 = 0;
const kCGMouseButtonRight: u32 = 1;

const kCGScrollEventUnitPixel: u32 = 0;

const kCGHIDEventTap: u32 = 0;

/// `kCGMouseEventClickState` — the click count (1, 2, 3, …) macOS
/// uses to decide whether to deliver a double/triple-click. Synthetic
/// CGEvents don't get this auto-computed; callers must set it.
const kCGMouseEventClickState: u32 = 1;
const kCGMouseEventDeltaX: u32 = 35;
const kCGMouseEventDeltaY: u32 = 36;

// Public scroll-event fields (CGEventTypes.h).
const kCGScrollWheelEventPointDeltaAxis1: u32 = 96;
const kCGScrollWheelEventPointDeltaAxis2: u32 = 97;
const kCGScrollWheelEventFixedPtDeltaAxis1: u32 = 93;
const kCGScrollWheelEventFixedPtDeltaAxis2: u32 = 94;
const kCGScrollWheelEventIsContinuous: u32 = 88;
// Public scroll-phase fields (in CGEventTypes.h since macOS 10.7).
const kCGScrollWheelEventScrollPhase: u32 = 99;
const kCGScrollWheelEventMomentumPhase: u32 = 123;

// NSEventPhase values (macOS public, but only exposed via NSEvent — same
// integer constants apply to CGEvent's scroll-phase and the private
// gesture-phase field).
const PHASE_NONE: i64 = 0;
const PHASE_BEGAN: i64 = 1;
const PHASE_CHANGED: i64 = 4;
const PHASE_ENDED: i64 = 8;
const PHASE_CANCELLED: i64 = 16;

// ---------- Private gesture event types (NSEvent → CGEvent mapping) ----------
//
// These integer values match NSEventType. CGEvent accepts them when set
// via CGEventSetType on a CGEventCreate(NULL) event, even though the
// CGEventType enum doesn't expose them publicly. Used by BetterTouchTool,
// Karabiner-Elements, MTMR, and others; stable on macOS 10.5+.
const kCGEventGestureRotate: u32 = 18;
const kCGEventGestureBegin: u32 = 19;
const kCGEventGestureEnd: u32 = 20;
const kCGEventGestureMagnify: u32 = 30;
const kCGEventGestureSwipe: u32 = 31;

// Private CGEventField IDs (gesture event payload).
const FIELD_GESTURE_SUBTYPE: u32 = 110;
const FIELD_GESTURE_VALUE: u32 = 113;
const FIELD_GESTURE_SWIPE_MASK: u32 = 115;
const FIELD_GESTURE_PHASE: u32 = 132;

// ---------- FFI ----------

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

unsafe extern "C" {
    fn CGEventCreate(source: CGEventSourceRef) -> CGEventRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        ty: u32,
        cursor: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateScrollWheelEvent2(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventGetLocation(event: CGEventRef) -> CGPoint;
    fn CGEventSetType(event: CGEventRef, ty: u32);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: CGEventRef, field: u32, value: f64);
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CFRelease(cf: *const c_void);
}

struct Event(CGEventRef);

impl Event {
    fn new() -> Option<Self> {
        let raw = unsafe { CGEventCreate(std::ptr::null_mut()) };
        if raw.is_null() {
            None
        } else {
            Some(Event(raw))
        }
    }

    fn from_raw(raw: CGEventRef) -> Option<Self> {
        if raw.is_null() { None } else { Some(Event(raw)) }
    }

    fn set_type(&self, ty: u32) {
        unsafe { CGEventSetType(self.0, ty) };
    }
    fn set_int(&self, field: u32, value: i64) {
        unsafe { CGEventSetIntegerValueField(self.0, field, value) };
    }
    fn set_dbl(&self, field: u32, value: f64) {
        unsafe { CGEventSetDoubleValueField(self.0, field, value) };
    }
    fn post(&self) {
        unsafe { CGEventPost(kCGHIDEventTap, self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as *const c_void) };
    }
}

// ---------- Public API ----------

#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Screen pixels emitted per millimeter of finger motion. Pad-density
    /// independent, so a given value gives the same physical sensitivity
    /// across pads of any logical resolution or aspect ratio.
    pub accel: f64,
    /// Screen pixels emitted per millimeter of finger motion in scroll mode.
    pub scroll_accel: f64,
    /// Natural scrolling: finger-down on the pad scrolls content down on
    /// the screen (the macOS default since 10.7). False for the legacy
    /// "wheel" convention where finger-down moves the scrollbar down /
    /// the content up.
    pub natural_scroll: bool,
    /// Allow private gesture-event injection. If false, pinch/rotate/swipe
    /// are no-ops (or fall back to keyboard shortcuts where sensible).
    pub private_gestures: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Phase {
    Began,
    Changed,
    Ended,
    Cancelled,
}

impl Phase {
    fn mask(self) -> i64 {
        match self {
            Phase::Began => PHASE_BEGAN,
            Phase::Changed => PHASE_CHANGED,
            Phase::Ended => PHASE_ENDED,
            Phase::Cancelled => PHASE_CANCELLED,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum SwipeDirection {
    Left,
    Right,
    Up,
    Down,
}

/// Trait surface a gesture engine sees. Implemented by [`Emitter`]
/// against real CGEvents; in tests, swap in a recording fake. All
/// motion arguments are in physical millimeters of finger travel.
pub trait Output {
    fn move_cursor_by(&self, dx_mm: f64, dy_mm: f64);
    fn click(&self, button: MouseButton);
    fn scroll(&self, dx_mm: f64, dy_mm: f64, phase: Phase);
    /// Seed scroll inertia from a just-ended pan. `vx_mm_per_sec` and
    /// `vy_mm_per_sec` are the EMA-smoothed centroid velocity at lift.
    /// Implementation drives a self-paced momentum-phase scroll stream
    /// that decays to zero. Called once per scroll session, after the
    /// final `scroll(.., Phase::Ended)`.
    fn scroll_inertia(&self, vx_mm_per_sec: f64, vy_mm_per_sec: f64);
    /// Cancel an in-flight inertia coast (typically because a new touch
    /// landed). Implementations should bracket the cancellation with a
    /// `MomentumPhase::Ended` event so apps stop their scroll animations.
    fn cancel_inertia(&self);
    fn pinch(&self, delta: f64, phase: Phase);
    fn rotate(&self, delta_degrees: f64, phase: Phase);
    fn swipe(&self, direction: SwipeDirection);
}

pub struct Emitter {
    cfg: Config,
    /// Most recent click that produced a CGEvent post: button + time +
    /// cursor location. The next click checks this to decide its
    /// `kCGMouseEventClickState` value — same button, within
    /// [`DOUBLE_CLICK_INTERVAL`], within [`DOUBLE_CLICK_DISTANCE_PX`] →
    /// increment; otherwise reset to 1. macOS doesn't auto-compute the
    /// click count for synthetic events, so triple-click etc. depend
    /// entirely on this field.
    last_click: Cell<Option<(MouseButton, Instant, CGPoint)>>,
    click_count: Cell<i64>,
    /// Sub-pixel carry for active scroll, by axis. Each `scroll()` call's
    /// f64 pixel value gets accumulated; the integer part drives the
    /// CGEvent's PointDelta / wheel fields, and the fractional part rolls
    /// over to the next event. Without this, integer truncation drops up
    /// to one pixel per event, which on a 60+ Hz event stream is a
    /// noticeable drift on slow scrolls.
    scroll_carry_x_px: Cell<f64>,
    scroll_carry_y_px: Cell<f64>,
    /// Inertia state plus its CFRunLoopTimer. Boxed for a stable address
    /// — the timer's C context holds a raw pointer back here. Allocated
    /// once at `new()`; the timer ref inside is None except while a
    /// coast is in flight.
    momentum: Box<Momentum>,
}

/// Inertia state. All cells are accessed only from the main run loop
/// thread (CFRunLoopTimer fires there), so `Cell` is sufficient.
struct Momentum {
    cfg: Config,
    /// Most recent EMA velocity passed in via `scroll_inertia`, decayed
    /// each tick. Zero between coasts.
    vel_x_mm_per_sec: Cell<f64>,
    vel_y_mm_per_sec: Cell<f64>,
    /// Wall-clock time of the previous tick. Used to derive the per-tick
    /// integration step `dt`; tolerates jitter in timer scheduling.
    last_tick: Cell<Option<Instant>>,
    /// Fractional-pixel carry for momentum-phase events (separate from
    /// the active-scroll carry so a fresh coast doesn't inherit drift
    /// from the lift).
    carry_x_px: Cell<f64>,
    carry_y_px: Cell<f64>,
    /// Live timer ref while coasting. Null otherwise. Stored so
    /// `cancel()` can invalidate it.
    timer_ref: Cell<CFRunLoopTimerRef>,
    /// True after `MomentumPhase::Began` has been posted for the current
    /// coast; gates the corresponding `Ended` on cancel/stop.
    began_posted: Cell<bool>,
}

impl Emitter {
    pub fn new(cfg: Config) -> Self {
        Self {
            cfg,
            last_click: Cell::new(None),
            click_count: Cell::new(0),
            scroll_carry_x_px: Cell::new(0.0),
            scroll_carry_y_px: Cell::new(0.0),
            momentum: Box::new(Momentum {
                cfg,
                vel_x_mm_per_sec: Cell::new(0.0),
                vel_y_mm_per_sec: Cell::new(0.0),
                last_tick: Cell::new(None),
                carry_x_px: Cell::new(0.0),
                carry_y_px: Cell::new(0.0),
                timer_ref: Cell::new(std::ptr::null_mut()),
                began_posted: Cell::new(false),
            }),
        }
    }

    pub fn cursor(&self) -> CGPoint {
        // CGEventCreate(NULL) on a fresh event yields the current cursor
        // location via CGEventGetLocation.
        let Some(e) = Event::new() else {
            return CGPoint::new(0.0, 0.0);
        };
        unsafe { CGEventGetLocation(e.0) }
    }

    pub fn move_cursor_by(&self, dx_mm: f64, dy_mm: f64) {
        let dx = dx_mm * self.cfg.accel;
        let dy = dy_mm * self.cfg.accel;
        let mut p = self.cursor();
        p.x += dx;
        p.y += dy;
        let Some(e) = Event::from_raw(unsafe {
            CGEventCreateMouseEvent(std::ptr::null_mut(), kCGEventMouseMoved, p, kCGMouseButtonLeft)
        }) else {
            return;
        };
        e.set_int(kCGMouseEventDeltaX as u32, dx as i64);
        e.set_int(kCGMouseEventDeltaY as u32, dy as i64);
        log::trace!("post: mouseMoved d=({:+.1},{:+.1})px to=({:.0},{:.0})", dx, dy, p.x, p.y);
        e.post();
    }

    pub fn click(&self, button: MouseButton) {
        let p = self.cursor();
        let now = Instant::now();
        // Decide the click count for this event. Same button, within
        // the double-click time and distance windows → increment;
        // otherwise reset to 1. Both the down and up events of this
        // click carry the same count, matching what macOS does for
        // natural input.
        let count = match self.last_click.get() {
            Some((b, t, last_p))
                if b == button
                    && now.saturating_duration_since(t) < DOUBLE_CLICK_INTERVAL
                    && ((p.x - last_p.x).powi(2) + (p.y - last_p.y).powi(2)).sqrt()
                        < DOUBLE_CLICK_DISTANCE_PX =>
            {
                self.click_count.get() + 1
            }
            _ => 1,
        };
        self.click_count.set(count);
        self.last_click.set(Some((button, now, p)));

        let (down, up, raw_button) = match button {
            MouseButton::Left => (kCGEventLeftMouseDown, kCGEventLeftMouseUp, kCGMouseButtonLeft),
            MouseButton::Right => (kCGEventRightMouseDown, kCGEventRightMouseUp, kCGMouseButtonRight),
        };
        log::debug!(
            "post: click {:?} count={} at=({:.0},{:.0})",
            button, count, p.x, p.y,
        );
        if let Some(e) = Event::from_raw(unsafe {
            CGEventCreateMouseEvent(std::ptr::null_mut(), down, p, raw_button)
        }) {
            e.set_int(kCGMouseEventClickState, count);
            e.post();
        }
        if let Some(e) = Event::from_raw(unsafe {
            CGEventCreateMouseEvent(std::ptr::null_mut(), up, p, raw_button)
        }) {
            e.set_int(kCGMouseEventClickState, count);
            e.post();
        }
    }

    /// Phased smooth-pixel scroll. `phase` brackets the gesture so apps
    /// (Safari, Maps, etc.) can do rubber-banding and track the gesture
    /// as a continuous interaction rather than discrete wheel ticks.
    pub fn scroll(&self, dx_mm: f64, dy_mm: f64, phase: Phase) {
        let sign = if self.cfg.natural_scroll { 1.0 } else { -1.0 };
        let dx_px = sign * dx_mm * self.cfg.scroll_accel;
        let dy_px = sign * dy_mm * self.cfg.scroll_accel;
        // Reset sub-pixel carry on Began so a stale fraction from the
        // previous gesture doesn't surface on the first event.
        if matches!(phase, Phase::Began) {
            self.scroll_carry_x_px.set(0.0);
            self.scroll_carry_y_px.set(0.0);
        }
        let total_x = self.scroll_carry_x_px.get() + dx_px;
        let total_y = self.scroll_carry_y_px.get() + dy_px;
        let int_x = total_x.trunc() as i32;
        let int_y = total_y.trunc() as i32;
        self.scroll_carry_x_px.set(total_x - int_x as f64);
        self.scroll_carry_y_px.set(total_y - int_y as f64);
        post_scroll_event(int_x, int_y, dx_px, dy_px, phase, /* momentum */ Phase::Cancelled);
    }

    /// Seed inertia from the just-ended pan. Cancels any in-flight coast
    /// and starts a new one driven by a CFRunLoopTimer.
    pub fn scroll_inertia(&self, vx_mm_per_sec: f64, vy_mm_per_sec: f64) {
        let sign = if self.cfg.natural_scroll { 1.0 } else { -1.0 };
        // Apply direction sign here so the Momentum struct doesn't have
        // to know about natural_scroll — it just integrates a velocity.
        self.momentum.start(sign * vx_mm_per_sec, sign * vy_mm_per_sec);
    }

    /// Cancel any in-flight inertia. No-op if not coasting.
    pub fn cancel_inertia(&self) {
        self.momentum.cancel();
    }

    /// Emit a pinch (magnify) gesture. `delta` is the *change* in scale
    /// since the last event (e.g. 0.05 = 5% bigger). Phase brackets are
    /// required for apps to track the gesture.
    pub fn pinch(&self, delta: f64, phase: Phase) {
        if !self.cfg.private_gestures {
            log::trace!("post: pinch suppressed (private_gestures=false)");
            return;
        }
        if matches!(phase, Phase::Began) {
            self.gesture_bracket(true);
        }
        if let Some(e) = Event::new() {
            e.set_type(kCGEventGestureMagnify);
            e.set_int(FIELD_GESTURE_PHASE, phase.mask());
            e.set_dbl(FIELD_GESTURE_VALUE, delta);
            log::trace!("post: pinch {:?} delta={:+.4}", phase, delta);
            e.post();
        }
        if matches!(phase, Phase::Ended | Phase::Cancelled) {
            self.gesture_bracket(false);
        }
    }

    /// Emit a rotate gesture. `delta_degrees` is the *change* in rotation
    /// since the last event, positive = counterclockwise (matching
    /// NSEvent.rotation semantics).
    pub fn rotate(&self, delta_degrees: f64, phase: Phase) {
        if !self.cfg.private_gestures {
            log::trace!("post: rotate suppressed (private_gestures=false)");
            return;
        }
        if matches!(phase, Phase::Began) {
            self.gesture_bracket(true);
        }
        if let Some(e) = Event::new() {
            e.set_type(kCGEventGestureRotate);
            e.set_int(FIELD_GESTURE_PHASE, phase.mask());
            e.set_dbl(FIELD_GESTURE_VALUE, delta_degrees);
            log::trace!("post: rotate {:?} delta={:+.2}deg", phase, delta_degrees);
            e.post();
        }
        if matches!(phase, Phase::Ended | Phase::Cancelled) {
            self.gesture_bracket(false);
        }
    }

    /// Emit a 3-finger swipe in `direction`. macOS treats 3F swipe as a
    /// discrete navigation event (Safari back/forward, etc.).
    pub fn swipe(&self, direction: SwipeDirection) {
        if !self.cfg.private_gestures {
            log::debug!("post: swipe {:?} suppressed (private_gestures=false)", direction);
            return;
        }
        log::debug!("post: swipe {:?}", direction);
        let (dx, dy): (f64, f64) = match direction {
            SwipeDirection::Left => (-1.0, 0.0),
            SwipeDirection::Right => (1.0, 0.0),
            SwipeDirection::Up => (0.0, 1.0),
            SwipeDirection::Down => (0.0, -1.0),
        };
        // BeginGesture
        self.gesture_bracket(true);
        // The swipe event itself: type=31, X delta in value, Y delta in
        // swipe-mask field. (This matches the NSEvent.deltaX/deltaY split
        // for swipe events.)
        if let Some(e) = Event::new() {
            e.set_type(kCGEventGestureSwipe);
            e.set_dbl(FIELD_GESTURE_VALUE, dx);
            e.set_dbl(FIELD_GESTURE_SWIPE_MASK as u32, dy);
            e.post();
        }
        self.gesture_bracket(false);
    }

}

/// Post a single phased scroll event. Exactly one of `scroll_phase` and
/// `momentum_phase` should be `Some`; the other goes on the wire as
/// `PHASE_NONE`. The integer pixel values drive line-equivalent and
/// point-delta fields; the float values drive the high-precision
/// `FixedPtDelta` field, so smooth-scroll-aware apps see sub-pixel
/// motion that integer truncation would otherwise drop.
fn post_scroll_event(
    int_x_px: i32,
    int_y_px: i32,
    float_x_px: f64,
    float_y_px: f64,
    scroll_phase: Phase,
    momentum_phase: Phase,
) {
    let Some(e) = Event::from_raw(unsafe {
        CGEventCreateScrollWheelEvent2(
            std::ptr::null_mut(),
            kCGScrollEventUnitPixel,
            2,
            int_y_px,
            int_x_px,
            0,
        )
    }) else {
        return;
    };
    let scroll_mask = match scroll_phase {
        Phase::Cancelled => PHASE_NONE,
        p => p.mask(),
    };
    let momentum_mask = match momentum_phase {
        Phase::Cancelled => PHASE_NONE,
        p => p.mask(),
    };
    e.set_int(kCGScrollWheelEventScrollPhase, scroll_mask);
    e.set_int(kCGScrollWheelEventMomentumPhase, momentum_mask);
    e.set_int(kCGScrollWheelEventIsContinuous, 1);
    // High-precision deltas. Q16.16 fixed-point (1.0 == 0x10000), capped
    // at i32 range. Apps that look at `scrollingDeltaY` (NSEvent) read
    // the FixedPt value rather than the integer, so this keeps fractional
    // pixels from disappearing on slow scrolls.
    let fp_y = (float_y_px * 65536.0).clamp(i32::MIN as f64, i32::MAX as f64) as i64;
    let fp_x = (float_x_px * 65536.0).clamp(i32::MIN as f64, i32::MAX as f64) as i64;
    e.set_int(kCGScrollWheelEventFixedPtDeltaAxis1, fp_y);
    e.set_int(kCGScrollWheelEventFixedPtDeltaAxis2, fp_x);
    e.set_int(kCGScrollWheelEventPointDeltaAxis1, int_y_px as i64);
    e.set_int(kCGScrollWheelEventPointDeltaAxis2, int_x_px as i64);
    log::trace!(
        "post: scroll s={:?} m={:?} px=({:+},{:+}) precise=({:+.2},{:+.2})",
        scroll_phase, momentum_phase, int_x_px, int_y_px, float_x_px, float_y_px,
    );
    e.post();
}

impl Momentum {
    /// Begin coasting at the given velocity. Cancels any in-flight coast
    /// first so a quick re-flick replaces the seed cleanly.
    fn start(&self, vx_mm_per_sec: f64, vy_mm_per_sec: f64) {
        self.cancel();
        let speed = (vx_mm_per_sec * vx_mm_per_sec + vy_mm_per_sec * vy_mm_per_sec).sqrt();
        if speed < MOMENTUM_SEED_MM_PER_SEC {
            log::debug!(
                "scroll: inertia skipped (speed={:.0}mm/s below seed threshold {:.0})",
                speed, MOMENTUM_SEED_MM_PER_SEC,
            );
            return;
        }
        self.vel_x_mm_per_sec.set(vx_mm_per_sec);
        self.vel_y_mm_per_sec.set(vy_mm_per_sec);
        self.last_tick.set(None);
        self.carry_x_px.set(0.0);
        self.carry_y_px.set(0.0);
        self.began_posted.set(false);
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: self as *const Momentum as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        let now_abs = unsafe { CFAbsoluteTimeGetCurrent() };
        let timer = unsafe {
            CFRunLoopTimerCreate(
                std::ptr::null_mut(),
                now_abs + MOMENTUM_TICK_INTERVAL,
                MOMENTUM_TICK_INTERVAL,
                0,
                0,
                momentum_tick,
                &mut ctx,
            )
        };
        if timer.is_null() {
            log::warn!("scroll: CFRunLoopTimerCreate returned NULL; inertia disabled");
            return;
        }
        unsafe {
            CFRunLoopAddTimer(
                CFRunLoop::get_current().as_concrete_TypeRef() as *mut _,
                timer,
                kCFRunLoopDefaultMode,
            );
        }
        self.timer_ref.set(timer);
        log::debug!(
            "scroll: inertia started v=({:+.0},{:+.0})mm/s",
            vx_mm_per_sec, vy_mm_per_sec,
        );
    }

    /// Stop coasting (if active), post a momentum-Ended bracket so apps
    /// can finalize their scroll animation, and release the timer.
    fn cancel(&self) {
        let t = self.timer_ref.replace(std::ptr::null_mut());
        if t.is_null() {
            return;
        }
        unsafe {
            CFRunLoopTimerInvalidate(t);
            CFRelease(t as *const c_void);
        }
        self.vel_x_mm_per_sec.set(0.0);
        self.vel_y_mm_per_sec.set(0.0);
        self.last_tick.set(None);
        self.carry_x_px.set(0.0);
        self.carry_y_px.set(0.0);
        if self.began_posted.replace(false) {
            post_scroll_event(0, 0, 0.0, 0.0, Phase::Cancelled, Phase::Ended);
        }
        log::debug!("scroll: inertia cancelled");
    }

    /// One timer tick: integrate velocity over the elapsed interval,
    /// post a momentum-phase event if the integer-pixel quantum is
    /// non-zero, decay the velocity, and stop if we're below the
    /// stop threshold.
    fn tick(&self) {
        let now = Instant::now();
        let dt = match self.last_tick.replace(Some(now)) {
            Some(prev) => (now - prev).as_secs_f64().clamp(0.001, 0.1),
            None => MOMENTUM_TICK_INTERVAL,
        };

        // Decay velocity exponentially toward zero. `MOMENTUM_DECAY_PER_SEC`
        // is the multiplier per second; scale to dt with `^dt`.
        let factor = MOMENTUM_DECAY_PER_SEC.powf(dt);
        let vx = self.vel_x_mm_per_sec.get() * factor;
        let vy = self.vel_y_mm_per_sec.get() * factor;
        self.vel_x_mm_per_sec.set(vx);
        self.vel_y_mm_per_sec.set(vy);

        let speed = (vx * vx + vy * vy).sqrt();
        if speed < MOMENTUM_STOP_MM_PER_SEC {
            self.cancel();
            return;
        }

        // Integrate to per-tick pixel displacement.
        let dx_px = vx * dt * self.cfg.scroll_accel;
        let dy_px = vy * dt * self.cfg.scroll_accel;
        let total_x = self.carry_x_px.get() + dx_px;
        let total_y = self.carry_y_px.get() + dy_px;
        let int_x = total_x.trunc() as i32;
        let int_y = total_y.trunc() as i32;
        self.carry_x_px.set(total_x - int_x as f64);
        self.carry_y_px.set(total_y - int_y as f64);

        let phase = if self.began_posted.replace(true) {
            Phase::Changed
        } else {
            Phase::Began
        };
        post_scroll_event(int_x, int_y, dx_px, dy_px, Phase::Cancelled, phase);
    }
}

extern "C" fn momentum_tick(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    // Safety: `info` was set to `&Momentum` in `Momentum::start`, the
    // Momentum lives in a Box owned by the Emitter, and the Emitter's
    // Drop invalidates the timer before the Box is dropped. So the
    // pointer is live for every callback.
    let m = unsafe { &*(info as *const Momentum) };
    m.tick();
}

impl Drop for Emitter {
    fn drop(&mut self) {
        // Invalidate the timer before the Momentum box is dropped so
        // an in-flight callback can't dereference a freed pointer.
        self.momentum.cancel();
    }
}

impl Emitter {
    fn gesture_bracket(&self, begin: bool) {
        if let Some(e) = Event::new() {
            e.set_type(if begin {
                kCGEventGestureBegin
            } else {
                kCGEventGestureEnd
            });
            e.set_int(FIELD_GESTURE_SUBTYPE, 0);
            e.post();
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
}

impl Output for Emitter {
    fn move_cursor_by(&self, dx_mm: f64, dy_mm: f64) {
        Emitter::move_cursor_by(self, dx_mm, dy_mm);
    }
    fn click(&self, button: MouseButton) {
        Emitter::click(self, button);
    }
    fn scroll(&self, dx_mm: f64, dy_mm: f64, phase: Phase) {
        Emitter::scroll(self, dx_mm, dy_mm, phase);
    }
    fn scroll_inertia(&self, vx_mm_per_sec: f64, vy_mm_per_sec: f64) {
        Emitter::scroll_inertia(self, vx_mm_per_sec, vy_mm_per_sec);
    }
    fn cancel_inertia(&self) {
        Emitter::cancel_inertia(self);
    }
    fn pinch(&self, delta: f64, phase: Phase) {
        Emitter::pinch(self, delta, phase);
    }
    fn rotate(&self, delta_degrees: f64, phase: Phase) {
        Emitter::rotate(self, delta_degrees, phase);
    }
    fn swipe(&self, direction: SwipeDirection) {
        Emitter::swipe(self, direction);
    }
}
