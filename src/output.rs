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

use core_graphics::geometry::CGPoint;
use std::ffi::c_void;

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

const kCGMouseEventDeltaX: u32 = 35;
const kCGMouseEventDeltaY: u32 = 36;

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
    /// Cursor pixels per logical normalized unit (input coords are [0,1]).
    pub accel: f64,
    /// Scroll pixels per logical normalized unit.
    pub scroll_accel: f64,
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
/// against real CGEvents; in tests, swap in a recording fake.
pub trait Output {
    fn move_cursor_by(&self, dx_units: f64, dy_units: f64);
    fn click(&self, button: MouseButton);
    fn scroll(&self, dx_units: f64, dy_units: f64, phase: Phase);
    fn pinch(&self, delta: f64, phase: Phase);
    fn rotate(&self, delta_degrees: f64, phase: Phase);
    fn swipe(&self, direction: SwipeDirection);
}

#[derive(Clone, Copy, Debug)]
pub struct Emitter {
    cfg: Config,
}

impl Emitter {
    pub fn new(cfg: Config) -> Self {
        Self { cfg }
    }

    pub fn cursor(&self) -> CGPoint {
        // CGEventCreate(NULL) on a fresh event yields the current cursor
        // location via CGEventGetLocation.
        let Some(e) = Event::new() else {
            return CGPoint::new(0.0, 0.0);
        };
        unsafe { CGEventGetLocation(e.0) }
    }

    pub fn move_cursor_by(&self, dx_units: f64, dy_units: f64) {
        let dx = dx_units * self.cfg.accel;
        let dy = dy_units * self.cfg.accel;
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
        e.post();
    }

    pub fn click(&self, button: MouseButton) {
        let p = self.cursor();
        let (down, up, raw_button) = match button {
            MouseButton::Left => (kCGEventLeftMouseDown, kCGEventLeftMouseUp, kCGMouseButtonLeft),
            MouseButton::Right => (kCGEventRightMouseDown, kCGEventRightMouseUp, kCGMouseButtonRight),
        };
        if let Some(e) = Event::from_raw(unsafe {
            CGEventCreateMouseEvent(std::ptr::null_mut(), down, p, raw_button)
        }) {
            e.post();
        }
        if let Some(e) = Event::from_raw(unsafe {
            CGEventCreateMouseEvent(std::ptr::null_mut(), up, p, raw_button)
        }) {
            e.post();
        }
    }

    /// Phased smooth-pixel scroll. `phase` brackets the gesture so apps
    /// (Safari, Maps, etc.) can do rubber-banding and track the gesture
    /// as a continuous interaction rather than discrete wheel ticks.
    pub fn scroll(&self, dx_units: f64, dy_units: f64, phase: Phase) {
        let dx = -dx_units * self.cfg.scroll_accel;
        let dy = -dy_units * self.cfg.scroll_accel;
        let Some(e) = Event::from_raw(unsafe {
            CGEventCreateScrollWheelEvent2(
                std::ptr::null_mut(),
                kCGScrollEventUnitPixel,
                2,
                dy as i32,
                dx as i32,
                0,
            )
        }) else {
            return;
        };
        e.set_int(kCGScrollWheelEventScrollPhase as u32, phase.mask());
        e.set_int(kCGScrollWheelEventMomentumPhase as u32, PHASE_NONE);
        e.post();
    }

    /// Emit a pinch (magnify) gesture. `delta` is the *change* in scale
    /// since the last event (e.g. 0.05 = 5% bigger). Phase brackets are
    /// required for apps to track the gesture.
    pub fn pinch(&self, delta: f64, phase: Phase) {
        if !self.cfg.private_gestures {
            return;
        }
        if matches!(phase, Phase::Began) {
            self.gesture_bracket(true);
        }
        if let Some(e) = Event::new() {
            e.set_type(kCGEventGestureMagnify);
            e.set_int(FIELD_GESTURE_PHASE, phase.mask());
            e.set_dbl(FIELD_GESTURE_VALUE, delta);
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
            return;
        }
        if matches!(phase, Phase::Began) {
            self.gesture_bracket(true);
        }
        if let Some(e) = Event::new() {
            e.set_type(kCGEventGestureRotate);
            e.set_int(FIELD_GESTURE_PHASE, phase.mask());
            e.set_dbl(FIELD_GESTURE_VALUE, delta_degrees);
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
            return;
        }
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

#[derive(Clone, Copy, Debug)]
pub enum MouseButton {
    Left,
    Right,
}

impl Output for Emitter {
    fn move_cursor_by(&self, dx_units: f64, dy_units: f64) {
        Emitter::move_cursor_by(self, dx_units, dy_units);
    }
    fn click(&self, button: MouseButton) {
        Emitter::click(self, button);
    }
    fn scroll(&self, dx_units: f64, dy_units: f64, phase: Phase) {
        Emitter::scroll(self, dx_units, dy_units, phase);
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
