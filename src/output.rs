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
use core_graphics::display::CGDisplay;
use core_graphics::geometry::{CGPoint, CGRect};
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

/// Power-curve acceleration for scroll, modeled on `smooth_scroll.swift`'s
/// `acceleratePixels`. Real trackpads (and Mac Mouse Fix / LinearMouse)
/// expose this kind of curve because uniform `pixels = mm × accel` feels
/// too linear: slow scrolls overshoot if accel is high enough for flicks
/// to feel responsive, and flicks feel sluggish if accel is low enough
/// for slow scrolls to stay precise. The exponent boosts faster motion
/// disproportionately, giving Apple-style "high initial velocity, clear
/// deceleration" feel without sacrificing slow-scroll precision.
///
/// Curve: `pixels_per_sec = sign(v) × LINEAR × |v|^EXPONENT`, calibrated
/// so the curve passes through the linear value `scroll_accel × v` at
/// `v == REF_MM_PER_SEC` (i.e. crosses the linear feel at "typical"
/// scroll speed). Below the reference, curve is sub-linear → slow
/// motion is slower than linear. Above it, super-linear → fast motion
/// gets amplified.
const SCROLL_CURVE_EXPONENT: f64 = 1.3;
/// Reference velocity (mm/s). At this velocity, the curve's pixel rate
/// equals `scroll_accel × velocity`; ~1 mm per chip frame on a 60 Hz
/// pad, which feels "typical" to the user during deliberate panning.
const SCROLL_CURVE_REF_MM_PER_SEC: f64 = 60.0;

/// Bounds of the display containing `point`, falling back to the main
/// display if the point isn't on any (e.g. just past a screen edge —
/// which is exactly the case we're trying to clamp against). Used by
/// `move_cursor_by` to keep posted event locations on-screen.
fn display_bounds_for(point: CGPoint) -> CGRect {
    if let Ok((ids, _)) = CGDisplay::displays_with_point(point, 1) {
        if let Some(&id) = ids.first() {
            return CGDisplay::new(id).bounds();
        }
    }
    CGDisplay::main().bounds()
}

/// Apply the scroll-acceleration curve to a velocity, returning pixels
/// per second. Caller multiplies by per-tick `dt` for the per-tick
/// pixel delta.
fn accelerate_scroll(v_mm_per_sec: f64, scroll_accel: f64) -> f64 {
    let mag = v_mm_per_sec.abs();
    if mag == 0.0 {
        return 0.0;
    }
    // LINEAR = scroll_accel × REF^(1 - EXPONENT). At v == REF this gives
    // pixels_per_sec = scroll_accel × REF (matches linear feel).
    let linear = scroll_accel * SCROLL_CURVE_REF_MM_PER_SEC.powf(1.0 - SCROLL_CURVE_EXPONENT);
    v_mm_per_sec.signum() * linear * mag.powf(SCROLL_CURVE_EXPONENT)
}

// ---------- Public CGEvent constants (mirrored from CGEventTypes.h) ----------

const kCGEventLeftMouseDown: u32 = 1;
const kCGEventLeftMouseUp: u32 = 2;
const kCGEventRightMouseDown: u32 = 3;
const kCGEventRightMouseUp: u32 = 4;
const kCGEventMouseMoved: u32 = 5;
const kCGEventLeftMouseDragged: u32 = 6;

const kCGMouseButtonLeft: u32 = 0;
const kCGMouseButtonRight: u32 = 1;

const kCGScrollEventUnitPixel: u32 = 0;

/// Event tap location for scroll events. The HID tap (0) sits below the
/// gesture-engine and AppleMultitouchHIDService — events injected there
/// can be filtered or merged into a multitouch device's own gesture
/// state, which on this firmware (matched by AppleMultitouchHIDService
/// on its `(0xFF60, 0x07)` HID usage) means our scroll posts get
/// silently absorbed. The session tap (1) sits one level up, post-HID
/// and pre-annotation: the path real trackpads' events take. Matches
/// `smooth_scroll.swift`'s `.cgSessionEventTap` choice.
const kCGSessionEventTap: u32 = 1;
/// Default for non-scroll events (mouse moves, clicks, gestures); these
/// have been working fine on the HID tap and the session-tap risk isn't
/// worth taking.
const kCGHIDEventTap: u32 = 0;

/// `kCGEventSourceStateCombinedSessionState` from `CGEventSource.h`.
/// A source created with this state behaves like a real input device
/// (per Apple docs: "represents the combined state of all event sources
/// in the user session"), so apps that gate trackpad-only behaviors —
/// notably Chrome's rubber-band bounce in WebKit/Blink — accept our
/// events as a "fling" worth animating. A null source (what
/// `CGEventCreateScrollWheelEvent2` accepts) reads as synthetic.
const kCGEventSourceStateCombinedSessionState: i32 = 0;

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

/// Translate a `Phase` to the integer value `kCGScrollWheelEventScrollPhase`
/// expects. That field uses `CGScrollPhase`, **not** `NSEventPhase`:
/// began=1, changed=2, ended=4, cancelled=8. Posting NSEventPhase values
/// here means apps see e.g. our "Changed" (NSEvent=4) as CGScrollPhase
/// `Ended`, and Chrome / terminal-style scroll consumers immediately
/// terminate the gesture instead of tracking motion. AppKit translates
/// CGScrollPhase → NSEventPhase when it builds an NSEvent for delivery,
/// so writing the CG values is what matches real trackpads on the wire.
fn cg_scroll_phase(phase: Phase) -> i64 {
    match phase {
        Phase::Began => 1,
        Phase::Changed => 2,
        Phase::Ended => 4,
        Phase::Cancelled => 0, // sentinel: caller used Cancelled to mean "no scroll phase"
    }
}

/// Translate a `Phase` to the integer value
/// `kCGScrollWheelEventMomentumPhase` expects. That field uses
/// `CGMomentumScrollPhase` (sequential, not bit-flags): none=0, begin=1,
/// continue=2, end=3. Inertia in `Momentum::tick` posts a Began once
/// then Changed-Changed-…; the latter must map to `continue`, and the
/// final Ended (or our momentum-cancel post) maps to `end`.
fn cg_momentum_phase(phase: Phase) -> i64 {
    match phase {
        Phase::Began => 1,
        Phase::Changed => 2,
        Phase::Ended => 3,
        Phase::Cancelled => 0, // sentinel: caller used Cancelled to mean "no momentum phase"
    }
}

/// Translate a `Phase` to the integer value the private gesture-event
/// phase field (`FIELD_GESTURE_PHASE`, 132) expects. Despite NSEvent's
/// public `phase` property being NSEventPhase (1/4/8/16 bit flags), the
/// underlying CGEvent field on a private gesture event holds an
/// `IOHIDEventPhaseBits` value: 1=Began, 2=Changed, 4=Ended, 8=Cancelled
/// (sequential-ish, not bit flags). Confirmed via calftrail/Touch's
/// `tl_CGEventCreateFromGesture` (which uses IOHIDEventPhaseBits) and
/// Hammerspoon's `newGesture` (same). Setting NSEventPhase here produces
/// events that NSMagnificationGestureRecognizer-using apps (Photos,
/// Apple Maps) silently drop, even though the simpler
/// NSResponder.magnify(with:) path may still react.
fn iohid_gesture_phase(phase: Phase) -> u32 {
    match phase {
        Phase::Began => 1,
        Phase::Changed => 2,
        Phase::Ended => 4,
        Phase::Cancelled => 8,
    }
}

/// Gesture subtype values. From calftrail/Touch's `TLInfoSubtype` enum
/// (used by Hammerspoon's `tl_CGEventCreateFromGesture` and the older
/// MultitouchSupport sources). The synthesizer writes this to CGEvent
/// field 0x6E ("gestureSubtype") on the `NSEventTypeGesture` (29)
/// wrapper. Used for magnify/rotate; 3F/4F swipes go through the
/// DockControl path below — calftrail's `kTLInfoSubtypeSwipe` (0x10) is
/// a legacy NSEventTypeSwipe shape that modern macOS Mission Control /
/// Spaces no longer consume.
const GESTURE_SUBTYPE_ROTATE: u32 = 0x05;
const GESTURE_SUBTYPE_MAGNIFY: u32 = 0x08;

// ---------- DockControl swipe synthesis ----------
//
// macOS routes 3F/4F swipes through `kCGSEventDockControl` events
// (event-type-field 55 = 30) carrying `kIOHIDEventTypeDockSwipe`
// (field 110 = 23). Both axes go through the same envelope:
// motion field 123 = 1 (horizontal Spaces / full-screen apps) or
// 2 (vertical Mission Control / App Exposé). Direction is encoded
// in the *sign* of the cumulative-progress (124) and end-velocity
// (129/130) fields — not in the flag-bits field, despite iss.c's
// `right ? 1 : 0` shortcut. Empirically verified by tapping a real
// MBP trackpad: every gesture (left/right/up/down) emits hid=23 with
// matching motion + signed progress/velocity.
//
// Each DockControl event must be followed by a "companion"
// `kCGSEventGesture` event (field 55 = 29). Without the companion the
// Dock's state machine drops the dock event as orphaned.
//
// To get the rubber-band animation, the Dock needs a *stream* of
// Changed events with progress walking smoothly from ~0 toward the
// final value over real wall-clock time (~400 ms, ~20 events on the
// captured MBP). A degenerate Begin → End pair commits but skips the
// animation — that's iss.c's "instant swipe" trick. `Emitter::swipe`
// is therefore live-driven by the gesture engine: each chip frame's
// finger motion produces one Changed event, and lift produces Ended.
// That mirrors what the real driver does and lets the user reverse
// or abort mid-gesture exactly like a real trackpad.
const FIELD_CGS_EVENT_TYPE: u32 = 55;
const FIELD_GESTURE_HID_TYPE: u32 = 110;
const FIELD_GESTURE_SCROLL_Y: u32 = 119;
const FIELD_GESTURE_SWIPE_MOTION: u32 = 123;
const FIELD_GESTURE_SWIPE_PROGRESS: u32 = 124;
/// Per-event X-component of the swipe motion delta. For horizontal
/// swipes the MBP capture shows this matches the per-event change in
/// `FIELD_GESTURE_SWIPE_PROGRESS` (124); for vertical swipes it's
/// near-zero noise.
const FIELD_GESTURE_DELTA_X: u32 = 125;
/// Per-event Y-component of the swipe motion delta, **negated**. For
/// vertical swipes the MBP capture shows this is `-Δ124`; for
/// horizontal swipes it's near-zero noise. The Dock uses the
/// relative magnitudes of 125 and 126 to route the swipe to the
/// horizontal Spaces handler vs. vertical Mission Control / Exposé —
/// the easily-set motion field (123/165) appears to be advisory only,
/// since the OS clamped our 123=2 / 165=2 writes to 1 regardless of
/// event source. Sending the per-axis delta is what actually drives
/// the routing decision.
const FIELD_GESTURE_DELTA_Y_NEG: u32 = 126;
const FIELD_GESTURE_SWIPE_VELOCITY_X: u32 = 129;
const FIELD_GESTURE_SWIPE_VELOCITY_Y: u32 = 130;
const FIELD_GESTURE_PHASE: u32 = 132;
/// Mirror of [`FIELD_GESTURE_PHASE`]: every MBP-trackpad dock-swipe
/// event sets 132 and 134 to the same phase value. iss.c's "instant
/// commit" path sets only 132 and works for *horizontal* commits, so
/// it skipped 134 — but 134 is load-bearing for the Dock's natural
/// rubber-band animation and (empirically) for routing vertical
/// motion to Mission Control / App Exposé instead of falling through
/// to the horizontal handler. Without 134, motion=2 events were
/// being treated as horizontal swipes with the same direction sign.
const FIELD_GESTURE_PHASE_DUP: u32 = 134;
const FIELD_SCROLL_GESTURE_FLAG_BITS: u32 = 135;
/// Always 1 in every MBP-trackpad dock-swipe event. Purpose unclear;
/// included on the assumption that it's another Dock-side validity
/// check that may help routing.
const FIELD_GESTURE_CONST_136: u32 = 136;
/// Always 3 in every MBP-trackpad dock-swipe event. Same rationale
/// as field 136.
const FIELD_GESTURE_CONST_138: u32 = 138;
/// "Required, reason unknown" per the iss.c reverse-engineering
/// comment — set to FLT_TRUE_MIN (smallest positive subnormal f32) on
/// every dock swipe event or the Dock silently ignores it.
const FIELD_GESTURE_ZOOM_DELTA_X: u32 = 139;
/// Mirror of [`FIELD_GESTURE_SWIPE_MOTION`] (123). Every MBP-trackpad
/// dock-swipe event sets 123 and 165 to the same motion-axis value.
/// This is the most likely missing field for vertical-swipe routing —
/// the Dock appears to check 165 (not 123) when deciding whether to
/// dispatch the gesture to the horizontal Spaces handler vs. the
/// vertical Mission Control / Exposé handler.
const FIELD_GESTURE_SWIPE_MOTION_DUP: u32 = 165;

const CGS_EVENT_TYPE_GESTURE: i64 = 29;
const CGS_EVENT_TYPE_DOCK_CONTROL: i64 = 30;
const IOHID_EVENT_TYPE_DOCK_SWIPE: i64 = 23;
const SWIPE_MOTION_HORIZONTAL: i64 = 1;
const SWIPE_MOTION_VERTICAL: i64 = 2;

const SWIPE_PHASE_BEGAN: i64 = 1;
const SWIPE_PHASE_CHANGED: i64 = 2;
const SWIPE_PHASE_ENDED: i64 = 4;

// ---------- Vertical-swipe Dock-notification fallback ----------
//
// macOS clamps our synthetic dock-control `motion=2` to `1` regardless
// of event source, included fields, or per-axis delta components —
// every vertical swipe we synthesize gets routed to the horizontal
// Spaces handler. Synthetic Ctrl+Up keystrokes also don't fire
// Mission Control (the WindowServer hot-key dispatcher gates on
// keystate signals we can't fake from userspace).
//
// Hammerspoon's `hs.spaces.toggleMissionControl` (extensions/spaces/
// libspaces.m:241) sidesteps both paths and calls a private
// `CoreDockSendNotification(CFSTR("com.apple.expose.awake"), 0)`
// directly — the Dock owns Mission Control / App Exposé and listens
// for these notification names. The symbol is exported from the
// dyld shared cache (no on-disk framework binary on modern macOS),
// so we resolve it at runtime via `dlsym(RTLD_DEFAULT, ...)` rather
// than committing to a specific `-framework` link that might break
// across macOS versions.

/// Notification name that toggles Mission Control. From
/// hammerspoon/extensions/spaces/spaces.lua:258.
const DOCK_NOTIF_MISSION_CONTROL: &str = "com.apple.expose.awake";
/// Notification name that toggles App Exposé. From
/// hammerspoon/extensions/spaces/spaces.lua:272.
const DOCK_NOTIF_APP_EXPOSE: &str = "com.apple.expose.front.awake";

/// Cumulative-progress threshold above which a vertical swipe commits
/// the Mission Control / Exposé toggle. With
/// [`SWIPE_PROGRESS_REF_MM`] = 50mm, 0.2 ≈ 10mm of finger travel —
/// large enough to clearly distinguish from a 3F tap or accidental
/// drift past the axis-lock threshold (3mm), small enough that a
/// natural quick flick (10–15mm) reliably fires. The horizontal /
/// dock-control path uses MBP's commit threshold of ~0.5 because
/// that's what the Dock itself enforces; vertical is up to us since
/// we own the trigger via `CoreDockSendNotification`.
const SWIPE_VERTICAL_COMMIT_PROGRESS: f64 = 0.2;

/// Soft cap on the End-event velocity (mm/s). The Dock interprets
/// large lift velocities as a deliberate flick and commits the
/// transition with little or no animation; capturing the MBP trackpad
/// shows its driver lands in the 3–7 mm/s range even after a brisk
/// swipe (the EMA tracks deceleration into the lift). Our raw
/// gesture-engine EMA can land much higher when the user lifts while
/// still moving, which would feel "abrupt" relative to the MBP
/// baseline. Saturating at this value keeps fast swipes from flicking
/// past the natural feel without changing slow ones at all. Tunable.
const SWIPE_END_VELOCITY_MAX: f64 = 8.0;


/// Magic CGEventFlags value calftrail's gesture synthesizer sets on the
/// envelope event before serialization (`CGEventSetFlags(e, 256)`).
/// `0x100` is `NX_NONCOALSESCEDMASK` in IOHIDSystem private headers —
/// signals "do not collapse this with adjacent events of the same
/// type". Without it, AppKit's gesture pipeline can drop our synthesized
/// gesture events as duplicates of the surrounding (empty) HID stream.
const GESTURE_EVENT_FLAGS: u64 = 0x100;

// ---------- FFI ----------

type CGEventRef = *mut c_void;
type CGEventSourceRef = *mut c_void;

#[repr(C)]
struct CFRange {
    location: i64,
    length: i64,
}

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
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetTimestamp(event: CGEventRef, ts: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventSetDoubleValueField(event: CGEventRef, field: u32, value: f64);
    fn CGEventCreateData(allocator: *const c_void, event: CGEventRef) -> *mut c_void;
    fn CGEventCreateFromData(allocator: *const c_void, data: *const c_void) -> CGEventRef;
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventSourceCreate(state: i32) -> CGEventSourceRef;
    fn CFRelease(cf: *const c_void);
    fn CFDataCreateMutableCopy(
        allocator: *const c_void,
        capacity: i64,
        data: *const c_void,
    ) -> *mut c_void;
    fn CFDataAppendBytes(data: *mut c_void, bytes: *const u8, length: i64);
    fn CFDataDeleteBytes(data: *mut c_void, range: CFRange);
    fn CFDataGetLength(data: *const c_void) -> i64;
    fn clock_gettime_nsec_np(clock_id: u32) -> u64;
}

/// `CLOCK_UPTIME_RAW` from `<time.h>` — monotonic nanoseconds since boot
/// (does not advance during sleep). Same time base IOHID uses for its
/// event timestamps, and what `tl_uptime()` in calftrail returns via
/// the deprecated `AbsoluteToNanoseconds(UpTime())` path.
const CLOCK_UPTIME_RAW: u32 = 8;

struct Event(CGEventRef);

impl Event {
    fn new() -> Option<Self> {
        Self::with_source(std::ptr::null_mut())
    }

    /// Create an event with an explicit `CGEventSource`. Some CGEvent
    /// fields are silently normalized when the event has a NULL source
    /// (e.g. dock-control swipe motion=2 was being clamped to 1 on
    /// vertical swipes — the OS treats null-sourced gesture events as
    /// untrusted and forces them onto the horizontal-Spaces handler).
    /// Pass the persistent `kCGEventSourceStateCombinedSessionState`
    /// source from `Emitter` to keep the value the caller wrote.
    fn with_source(source: CGEventSourceRef) -> Option<Self> {
        let raw = unsafe { CGEventCreate(source) };
        if raw.is_null() {
            None
        } else {
            Some(Event(raw))
        }
    }

    fn from_raw(raw: CGEventRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Event(raw))
        }
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
    fn post_to(&self, tap: u32) {
        unsafe { CGEventPost(tap, self.0) };
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0 as *const c_void) };
    }
}

// ---------- Calftrail-style gesture synthesizer ----------
//
// `CGEventCreate(NULL); SetType(30); Set(field, value); Post()` produces an
// event AppKit's gesture-recognizer pipeline silently ignores: the
// `NSMagnificationGestureRecognizer` consumed by Photos / Apple Maps wants
// the event to carry an embedded IOHID payload (a digitizer-collection
// event + vendor token + per-touch digitizer events), not just the
// CGEvent fields. We can't get a properly-formed event from a public
// API, so we build one by:
//   1. asking CGEvent for the serialized form of an `NSEventTypeGesture`
//      (type 29) wrapper,
//   2. lopping off CGEvent's empty trailing field array,
//   3. appending a hand-rolled `IOHIDSystemQueueElement` containing a
//      digitizer-hand parent event and a vendor token,
//   4. appending the gesture's CGEvent fields (subtype, IOHID phase,
//      magnification/rotation/swipe value, plus the magic-zero fields
//      AppKit's parser walks),
//   5. and reconstituting via `CGEventCreateFromData`.
// Original C is calftrail/Touch's `tl_CGEventCreateFromGesture`
// (TouchSynthesis/TouchEvents.c); Hammerspoon's `newGesture` and
// jitouch use the same recipe. The IOHID structs are public from
// IOKit headers (IOHIDEventTypes.h / IOHIDEventData.h) but the
// CGEvent serialization layout — the trailing 24-byte trim, the
// {0x10,0x6D} marker, the BE field-encoding scheme, the magic
// zero fields at 0x6F/0x70/0x85/0x8B/0x8C — is reverse-engineered
// and undocumented.

/// `NSEventTypeGesture` — base wrapper for all calftrail-style private
/// gesture events. Subtype goes in field 0x6E.
const kCGEventGesture: u32 = 29;

const kIOHIDEventTypeVendorDefined: u32 = 1;
const kIOHIDEventTypeDigitizer: u32 = 11;
const kIOHIDEventOptionIsCollection: u32 = 0x02;
const kIOHIDDigitizerTransducerTypeHand: u32 = 0x23;
const kIOHIDDigitizerOrientationTypeQuality: u32 = 2;

/// Mirrors `_IOHIDDigitizerEventData` from `IOHIDEventData.h`. Layout
/// must match the C struct exactly: every field's offset and the total
/// `size_of::<DigitizerEventData>()` are read directly by the OS when
/// the appended bytes are demuxed back into events.
#[repr(C)]
struct DigitizerEventData {
    size: u32,
    ty: u32,
    timestamp: u64,
    options: u32,
    position_x: i32, // IOFixed (Q16.16)
    position_y: i32,
    position_z: i32,
    transducer_index: u32,
    transducer_type: u32,
    identity: u32,
    event_mask: u32,
    child_event_mask: u32,
    button_mask: u32,
    tip_pressure: i32,
    barrel_pressure: i32,
    twist: i32,
    orientation_type: u32,
    orientation_quality: i32,
    orientation_density: i32,
    orientation_irregularity: i32,
    orientation_major_radius: i32,
    orientation_minor_radius: i32,
}

/// Mirrors `_IOHIDVendorDefinedEventData`. The flexible `data[0]`
/// trailing array isn't part of `size_of`; payload bytes get appended
/// after this struct in the serialized stream.
#[repr(C)]
struct VendorDefinedEventData {
    size: u32,
    ty: u32,
    timestamp: u64,
    options: u32,
    usage_page: u16,
    usage: u16,
    version: u32,
    length: u32,
}

/// Mirrors `_IOHIDSystemQueueElement`. Trailing flexible
/// `events[]` is not part of `size_of`; child events get appended
/// after this struct.
#[repr(C)]
struct SystemQueueElement {
    timestamp: u64,
    device_id: u64,
    options: u32,
    event_count: u32,
}

/// Per-subtype payload value for the synthesized gesture event. Each
/// variant maps to a specific CGEvent field id (0x71/0x72). The
/// 0-value case is implicit for Began-/Ended-phase magnify and rotate
/// events.
enum GesturePayload {
    Magnification(f32),
    Rotation(f32),
}

/// Append a single CGEvent-serialized field entry: 2 bytes big-endian
/// `count`, 1 byte `type` (0x40 = uint32), 1 byte `field` id, then
/// `count` × 4 bytes of big-endian uint32 payload.
fn append_field_u32(data: *mut c_void, field: u8, value: u32) {
    let count: u16 = 1u16.to_be();
    let type_byte: u8 = 0x40;
    let value_be: u32 = value.to_be();
    unsafe {
        CFDataAppendBytes(data, &count as *const u16 as *const u8, 2);
        CFDataAppendBytes(data, &type_byte, 1);
        CFDataAppendBytes(data, &field, 1);
        CFDataAppendBytes(data, &value_be as *const u32 as *const u8, 4);
    }
}

/// As [`append_field_u32`] but with type 0xC0 (Float32 payload).
fn append_field_f32(data: *mut c_void, field: u8, value: f32) {
    let count: u16 = 1u16.to_be();
    let type_byte: u8 = 0xC0;
    let value_be: u32 = value.to_bits().to_be();
    unsafe {
        CFDataAppendBytes(data, &count as *const u16 as *const u8, 2);
        CFDataAppendBytes(data, &type_byte, 1);
        CFDataAppendBytes(data, &field, 1);
        CFDataAppendBytes(data, &value_be as *const u32 as *const u8, 4);
    }
}

/// Build a complete private gesture CGEvent ready to post. `subtype` is
/// one of `GESTURE_SUBTYPE_*` (8 = magnify, 5 = rotate, 16 = swipe).
/// `phase` is an `IOHIDEventPhaseBits` value (1=Began, 2=Changed,
/// 4=Ended). `payload` carries the subtype-specific value.
///
/// Implements `tl_CGEventCreateFromGesture` from calftrail/Touch with
/// no embedded child touches (Hammerspoon's `newGesture` does the same;
/// the parent digitizer-hand collection alone is enough for AppKit's
/// gesture-recognizer pipeline to bind to the event).
fn synthesize_gesture_event(subtype: u32, phase: u32, payload: GesturePayload) -> Option<Event> {
    let timestamp = unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) };

    // 1. Base event: type=29 (NSEventTypeGesture) wrapper, magic 256
    //    flags, IOHID-aligned timestamp.
    let proto = unsafe { CGEventCreate(std::ptr::null_mut()) };
    if proto.is_null() {
        return None;
    }
    unsafe {
        CGEventSetType(proto, kCGEventGesture);
        CGEventSetFlags(proto, GESTURE_EVENT_FLAGS);
        CGEventSetTimestamp(proto, timestamp);
    }

    // 2. Serialize. CGEvent's serialized form ends with a 24-byte empty
    //    field-array placeholder we'll overwrite with our own payload.
    let base_data = unsafe { CGEventCreateData(std::ptr::null(), proto) };
    unsafe { CFRelease(proto as *const c_void) };
    if base_data.is_null() {
        return None;
    }
    let gesture_data = unsafe { CFDataCreateMutableCopy(std::ptr::null(), 0, base_data) };
    unsafe { CFRelease(base_data as *const c_void) };
    if gesture_data.is_null() {
        return None;
    }
    let len = unsafe { CFDataGetLength(gesture_data) };
    if len >= 24 {
        unsafe {
            CFDataDeleteBytes(
                gesture_data,
                CFRange {
                    location: len - 24,
                    length: 24,
                },
            )
        };
    }

    // 3. Append the IOHID payload header: a 16-bit big-endian total size
    //    plus the {0x10, 0x6D} marker that flags the rest as the
    //    serialized-events blob.
    let parent_size = std::mem::size_of::<DigitizerEventData>() as u32;
    let queue_size = std::mem::size_of::<SystemQueueElement>() as u32;
    let vendor_struct_size = std::mem::size_of::<VendorDefinedEventData>() as u32;
    let vendor_payload_size: u32 = 40;
    let vendor_total = vendor_struct_size + vendor_payload_size;
    let total_size: u16 = (queue_size + vendor_total + parent_size) as u16;
    unsafe {
        let total_be = total_size.to_be();
        CFDataAppendBytes(gesture_data, &total_be as *const u16 as *const u8, 2);
        let marker: [u8; 2] = [0x10, 0x6D];
        CFDataAppendBytes(gesture_data, marker.as_ptr(), 2);
    }

    // 4. Queue-element header (host-endian — these are raw IOHID
    //    structs, not CGEvent-serialized fields).
    let queue = SystemQueueElement {
        timestamp,
        device_id: 0,
        options: kIOHIDEventOptionIsCollection,
        event_count: 2, // parent digitizer + vendor token
    };
    unsafe {
        CFDataAppendBytes(
            gesture_data,
            &queue as *const _ as *const u8,
            queue_size as i64,
        )
    };

    // 5. Parent digitizer event — a "hand" collection with empty quality
    //    orientation. No real touches embedded; AppKit treats this as
    //    "synthetic 2F gesture from a multitouch device" via the hand
    //    transducer type and binds the recognizer accordingly.
    let parent = DigitizerEventData {
        size: parent_size,
        ty: kIOHIDEventTypeDigitizer,
        timestamp,
        options: kIOHIDEventOptionIsCollection,
        position_x: 0,
        position_y: 0,
        position_z: 0,
        transducer_index: 0,
        transducer_type: kIOHIDDigitizerTransducerTypeHand,
        identity: 0,
        event_mask: 0,
        child_event_mask: 0,
        button_mask: 0,
        tip_pressure: 0,
        barrel_pressure: 0,
        twist: 0,
        orientation_type: kIOHIDDigitizerOrientationTypeQuality,
        orientation_quality: 0,
        orientation_density: 0,
        orientation_irregularity: 0,
        orientation_major_radius: 0,
        orientation_minor_radius: 0,
    };
    unsafe {
        CFDataAppendBytes(
            gesture_data,
            &parent as *const _ as *const u8,
            parent_size as i64,
        )
    };

    // 6. Vendor token. usagePage 0xFF00 / usage 0x1777 is the magic
    //    pair calftrail discovered AppleMultitouchHIDService stamps on
    //    real-trackpad gesture events; the 40-byte payload is mostly
    //    zeros but the first 8 bytes hold a deviceID (0 here = "no
    //    specific device").
    let vendor_header = VendorDefinedEventData {
        size: vendor_total,
        ty: kIOHIDEventTypeVendorDefined,
        timestamp,
        options: 0,
        usage_page: 0xFF00,
        usage: 0x1777,
        version: 1,
        length: vendor_payload_size,
    };
    unsafe {
        CFDataAppendBytes(
            gesture_data,
            &vendor_header as *const _ as *const u8,
            vendor_struct_size as i64,
        );
        let payload = [0u8; 40];
        CFDataAppendBytes(gesture_data, payload.as_ptr(), vendor_payload_size as i64);
    }

    // 7. CGEvent fields (each 8-byte big-endian header + payload). The
    //    0x6F/0x70/0x85 zero fields aren't optional — the AppKit field
    //    walker expects them in this exact order. 0x8B/0x8C are
    //    likewise required-zero floats at the tail.
    append_field_u32(gesture_data, 0x6E, subtype); // gestureSubtype
    append_field_u32(gesture_data, 0x6F, 0);
    append_field_u32(gesture_data, 0x70, 0);
    append_field_u32(gesture_data, 0x84, phase); // gesturePhase
    append_field_u32(gesture_data, 0x85, 0);
    match payload {
        GesturePayload::Magnification(m) => append_field_f32(gesture_data, 0x71, m),
        GesturePayload::Rotation(r) => append_field_f32(gesture_data, 0x72, r),
    }
    append_field_f32(gesture_data, 0x8B, 0.0);
    append_field_f32(gesture_data, 0x8C, 0.0);

    // 8. Reconstitute the CGEvent.
    let synth = unsafe { CGEventCreateFromData(std::ptr::null(), gesture_data) };
    unsafe { CFRelease(gesture_data as *const c_void) };
    Event::from_raw(synth)
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

/// Axis along which a multi-finger swipe is accumulating progress.
/// Selected by the gesture engine on first significant motion (whichever
/// of horizontal/vertical dominates) and held for the rest of the
/// gesture so a wandering centroid doesn't flip the swipe sideways
/// mid-flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SwipeAxis {
    Horizontal,
    Vertical,
}

/// Trait surface a gesture engine sees. Implemented by [`Emitter`]
/// against real CGEvents; in tests, swap in a recording fake. All
/// motion arguments are in physical millimeters of finger travel.
pub trait Output {
    fn move_cursor_by(&self, dx_mm: f64, dy_mm: f64);
    fn click(&self, button: MouseButton);
    /// Latch the integrated touchpad button. Driven by the firmware's
    /// PTP report bit (bit 0 = left), which in turn mirrors keymap-driven
    /// `MouseBtn1` presses. While held, [`Self::move_cursor_by`] should
    /// post `LeftMouseDragged` so apps see a real drag rather than a
    /// move-while-button-pressed mismatch. Implementations are expected
    /// to dedupe (called once per frame regardless of change) and only
    /// post the corresponding mouse-down/up CGEvents on actual edges.
    fn set_left_button_held(&self, held: bool);
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
    /// Returns `true` if a coast was actually live and got cancelled —
    /// callers use this to flag the new touch as "born during coast"
    /// (rmk-style) and suppress any tap derived from it: the user
    /// reached in to stop the fling, not to click.
    fn cancel_inertia(&self) -> bool;
    fn pinch(&self, delta: f64, phase: Phase);
    fn rotate(&self, delta_degrees: f64, phase: Phase);
    /// Drive a multi-finger swipe live. `signed_progress` is the
    /// cumulative finger displacement along `axis`, normalized so that
    /// ±1.0 ≈ "user has clearly committed to the swipe". Sign carries
    /// direction (positive = right / down). `velocity_mm_per_sec` is
    /// only meaningful at `Phase::Ended` (drives the Dock's
    /// commit-vs-rubber-band decision); pass 0 for other phases.
    /// Phase brackets the gesture exactly like `scroll`: Began on the
    /// first emit after axis lock, Changed each subsequent frame,
    /// Ended on lift / finger-count drop. Implementations may track
    /// state across calls; `Cancelled` clears any in-flight stream.
    fn swipe(&self, axis: SwipeAxis, signed_progress: f64, velocity_mm_per_sec: f64, phase: Phase);
}

pub struct Emitter {
    cfg: Config,
    /// Persistent CGEventSource. Created with
    /// `kCGEventSourceStateCombinedSessionState` so apps see our scroll
    /// events as coming from a real input device — Chrome / WebKit
    /// gates rubber-band bounce on this. Held for the lifetime of the
    /// emitter; released on Drop.
    event_source: CGEventSourceRef,
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
    /// Wall-clock time of the most recent `scroll()` call. Used to derive
    /// per-frame `dt` so the acceleration curve can run on velocity
    /// (mm/s) rather than raw per-frame mm — keeps feel consistent
    /// across pad frame rates.
    scroll_last_time: Cell<Option<Instant>>,
    /// Inertia state plus its CFRunLoopTimer. Boxed for a stable address
    /// — the timer's C context holds a raw pointer back here. Allocated
    /// once at `new()`; the timer ref inside is None except while a
    /// coast is in flight.
    momentum: Box<Momentum>,
    /// Axis of the in-flight swipe, if any. Held across `swipe` calls
    /// so Ended posts to the same axis the stream was opened on, even
    /// if the gesture engine's motion estimate jitters between
    /// horizontal and vertical near the diagonal. None when no swipe
    /// is active.
    swipe_axis: Cell<Option<SwipeAxis>>,
    /// Previous `signed_progress` value posted on this swipe stream,
    /// for computing the per-event delta needed by the X/Y motion-
    /// component fields (125/126) the Dock reads to route horizontal
    /// vs. vertical. Reset to 0 on each Began.
    swipe_last_progress: Cell<f64>,
    /// True between `LeftMouseDown` and `LeftMouseUp` posts. While set,
    /// `move_cursor_by` emits `LeftMouseDragged` instead of
    /// `MouseMoved` so apps see a real drag stream. Driven by
    /// [`Output::set_left_button_held`], which the gesture engine
    /// forwards from the firmware's PTP integrated-button bit.
    left_button_held: Cell<bool>,
}

/// Inertia state. All cells are accessed only from the main run loop
/// thread (CFRunLoopTimer fires there), so `Cell` is sufficient.
struct Momentum {
    cfg: Config,
    /// Same persistent CGEventSource the Emitter holds. Aliased here
    /// (not retained separately) because the timer callback needs to
    /// post events but doesn't have the Emitter handy. Lifetime is the
    /// Emitter's — Drop invalidates the timer before releasing the
    /// source so the callback can't dangle.
    event_source: CGEventSourceRef,
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
        let event_source = unsafe { CGEventSourceCreate(kCGEventSourceStateCombinedSessionState) };
        if event_source.is_null() {
            log::warn!(
                "CGEventSourceCreate(combinedSessionState) returned NULL; \
                 scroll bounce-back may not engage in WebKit / Chrome"
            );
        }
        Self {
            cfg,
            event_source,
            last_click: Cell::new(None),
            click_count: Cell::new(0),
            scroll_carry_x_px: Cell::new(0.0),
            scroll_carry_y_px: Cell::new(0.0),
            scroll_last_time: Cell::new(None),
            momentum: Box::new(Momentum {
                cfg,
                event_source,
                vel_x_mm_per_sec: Cell::new(0.0),
                vel_y_mm_per_sec: Cell::new(0.0),
                last_tick: Cell::new(None),
                carry_x_px: Cell::new(0.0),
                carry_y_px: Cell::new(0.0),
                timer_ref: Cell::new(std::ptr::null_mut()),
                began_posted: Cell::new(false),
            }),
            swipe_axis: Cell::new(None),
            swipe_last_progress: Cell::new(0.0),
            left_button_held: Cell::new(false),
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
        let from = self.cursor();
        let mut p = from;
        p.x += dx;
        p.y += dy;
        // If the proposed point lands off every display, clamp it to the
        // bounds of the source display so the *event location* sits exactly
        // on the edge. The auto-hidden full-screen menu bar reveals only
        // while the cursor sits at the menu-bar display's top edge (y ==
        // origin.y); a real input device hits that edge naturally because
        // the OS won't let it leave the screen, but a CGEvent posted at
        // y < origin.y just visually clamps the cursor without firing the
        // reveal. Same for Dock auto-reveal at the bottom edge. Delta fields
        // below stay at the user's requested value so apps see "pushing past
        // the edge" intent. When the proposed point lands on *any* display
        // (e.g. crossing to an adjacent monitor), post it as-is — clamping
        // to the source display's bounds would pin the cursor at the source
        // display's edge and prevent multi-monitor traversal entirely.
        // NB: `displays_with_point` returns a Vec preallocated to at least
        // length 1 even when no display matches — the real match count is
        // in the second tuple field. Don't use `ids.is_empty()` here.
        let on_a_display = CGDisplay::displays_with_point(p, 1)
            .map(|(_, count)| count > 0)
            .unwrap_or(false);
        if !on_a_display {
            let bounds = display_bounds_for(from);
            p.x =
                p.x.clamp(bounds.origin.x, bounds.origin.x + bounds.size.width - 1.0);
            p.y =
                p.y.clamp(bounds.origin.y, bounds.origin.y + bounds.size.height - 1.0);
        }
        let event_type = if self.left_button_held.get() {
            kCGEventLeftMouseDragged
        } else {
            kCGEventMouseMoved
        };
        let Some(e) = Event::from_raw(unsafe {
            CGEventCreateMouseEvent(std::ptr::null_mut(), event_type, p, kCGMouseButtonLeft)
        }) else {
            return;
        };
        e.set_int(kCGMouseEventDeltaX as u32, dx as i64);
        e.set_int(kCGMouseEventDeltaY as u32, dy as i64);
        log::trace!(
            "post: {} d=({:+.1},{:+.1})px to=({:.0},{:.0})",
            if self.left_button_held.get() { "leftMouseDragged" } else { "mouseMoved" },
            dx,
            dy,
            p.x,
            p.y
        );
        e.post();
    }

    pub fn set_left_button_held(&self, held: bool) {
        if self.left_button_held.get() == held {
            return;
        }
        self.left_button_held.set(held);
        let p = self.cursor();
        let event_type = if held {
            kCGEventLeftMouseDown
        } else {
            kCGEventLeftMouseUp
        };
        log::debug!(
            "post: leftMouse{} at=({:.0},{:.0})",
            if held { "Down" } else { "Up" },
            p.x,
            p.y,
        );
        if let Some(e) = Event::from_raw(unsafe {
            CGEventCreateMouseEvent(std::ptr::null_mut(), event_type, p, kCGMouseButtonLeft)
        }) {
            // Click-state 1 matches the firmware-button semantic: a single
            // press, not a synthetic double/triple. Without setting this,
            // some apps treat the down/up pair as count=0 and ignore it.
            e.set_int(kCGMouseEventClickState, 1);
            e.post();
        }
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
            MouseButton::Left => (
                kCGEventLeftMouseDown,
                kCGEventLeftMouseUp,
                kCGMouseButtonLeft,
            ),
            MouseButton::Right => (
                kCGEventRightMouseDown,
                kCGEventRightMouseUp,
                kCGMouseButtonRight,
            ),
        };
        log::debug!(
            "post: click {:?} count={} at=({:.0},{:.0})",
            button,
            count,
            p.x,
            p.y,
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
    /// Per-frame mm is converted to mm/s via wall-clock dt so the
    /// acceleration curve runs on a frame-rate-independent velocity.
    pub fn scroll(&self, dx_mm: f64, dy_mm: f64, phase: Phase) {
        let sign = if self.cfg.natural_scroll { 1.0 } else { -1.0 };
        let now = Instant::now();
        // Reset per-stroke state on Began. Carry would otherwise leak a
        // fraction-of-a-pixel from the previous stroke; `scroll_last_time`
        // would inflate dt across the gap between strokes and corrupt the
        // first Changed event's velocity.
        if matches!(phase, Phase::Began) {
            self.scroll_carry_x_px.set(0.0);
            self.scroll_carry_y_px.set(0.0);
            self.scroll_last_time.set(None);
        }
        let prev_time = self.scroll_last_time.replace(Some(now));
        let dt = match prev_time {
            Some(t) => (now - t).as_secs_f64().clamp(0.001, 0.1),
            None => 1.0 / 60.0,
        };
        let vx = dx_mm / dt;
        let vy = dy_mm / dt;
        let dx_px = sign * accelerate_scroll(vx, self.cfg.scroll_accel) * dt;
        let dy_px = sign * accelerate_scroll(vy, self.cfg.scroll_accel) * dt;
        let total_x = self.scroll_carry_x_px.get() + dx_px;
        let total_y = self.scroll_carry_y_px.get() + dy_px;
        let int_x = total_x.trunc() as i32;
        let int_y = total_y.trunc() as i32;
        self.scroll_carry_x_px.set(total_x - int_x as f64);
        self.scroll_carry_y_px.set(total_y - int_y as f64);
        if matches!(phase, Phase::Changed) && int_x == 0 && int_y == 0 {
            return;
        }
        post_scroll_event(
            self.event_source,
            int_x,
            int_y,
            dx_px,
            dy_px,
            phase,
            /* momentum */ Phase::Cancelled,
        );
    }

    /// Seed inertia from the just-ended pan. Cancels any in-flight coast
    /// and starts a new one driven by a CFRunLoopTimer.
    pub fn scroll_inertia(&self, vx_mm_per_sec: f64, vy_mm_per_sec: f64) {
        let sign = if self.cfg.natural_scroll { 1.0 } else { -1.0 };
        // Apply direction sign here so the Momentum struct doesn't have
        // to know about natural_scroll — it just integrates a velocity.
        self.momentum
            .start(sign * vx_mm_per_sec, sign * vy_mm_per_sec);
    }

    /// Cancel any in-flight inertia. Returns `true` if a coast was
    /// active (so the caller knows the touch was cancelling a fling).
    pub fn cancel_inertia(&self) -> bool {
        self.momentum.cancel()
    }

    /// Emit a pinch (magnify) gesture. `delta` is the *change* in scale
    /// since the last event (e.g. 0.05 = 5% bigger). Phase brackets are
    /// required for apps to track the gesture.
    pub fn pinch(&self, delta: f64, phase: Phase) {
        if !self.cfg.private_gestures {
            log::trace!("post: pinch suppressed (private_gestures=false)");
            return;
        }
        if let Some(e) = synthesize_gesture_event(
            GESTURE_SUBTYPE_MAGNIFY,
            iohid_gesture_phase(phase),
            GesturePayload::Magnification(delta as f32),
        ) {
            log::trace!("post: pinch {:?} delta={:+.4}", phase, delta);
            e.post_to(kCGSessionEventTap);
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
        if let Some(e) = synthesize_gesture_event(
            GESTURE_SUBTYPE_ROTATE,
            iohid_gesture_phase(phase),
            GesturePayload::Rotation(delta_degrees as f32),
        ) {
            log::trace!("post: rotate {:?} delta={:+.2}deg", phase, delta_degrees);
            e.post_to(kCGSessionEventTap);
        }
    }

    /// Drive a 3F/4F swipe live, mirroring the per-frame motion the
    /// gesture engine sees. macOS routes these through the Dock as
    /// `kCGSEventDockControl` events (field 55 = 30) carrying
    /// `kIOHIDEventTypeDockSwipe` (110 = 23). Both horizontal Spaces
    /// and vertical-axis swipes use the same envelope; only the
    /// motion-axis field (123) and the sign of progress differ.
    ///
    /// The Dock plays its rubber-band animation in response to the
    /// Changed event stream at wall-clock pacing — a degenerate
    /// Begin→End pair commits without animating. The user can also
    /// reverse direction or release short to abort, just like a real
    /// trackpad, because progress is driven by their actual finger
    /// position.
    ///
    /// Sign convention (matches the captured MBP-trackpad shape):
    /// - Horizontal: positive = swipe-left finger motion, switches to
    ///   the space on the right (macOS natural-scrolling default).
    /// - Vertical: positive = swipe-up finger motion. (Whether this
    ///   triggers Mission Control vs. App Exposé is up to the Dock and
    ///   not yet fully reverse-engineered for synthetic events; the
    ///   captured shape is identical for both.)
    pub fn swipe(&self, axis: SwipeAxis, signed_progress: f64, velocity_mm_per_sec: f64, phase: Phase) {
        if !self.cfg.private_gestures {
            log::trace!(
                "post: swipe {:?} {:?} suppressed (private_gestures=false)",
                axis, phase,
            );
            return;
        }
        // Vertical swipes route through the keyboard-shortcut fallback,
        // not dock-control: the OS clamps motion=2 → 1 on synthetic
        // events, so a dock-control vertical swipe is indistinguishable
        // from a horizontal one and would inadvertently switch spaces.
        // Fire the discrete shortcut only on commit; intermediate
        // phases produce no output (no live rubber-band, but Mission
        // Control / Exposé are inherently discrete actions).
        if matches!(axis, SwipeAxis::Vertical) {
            if matches!(phase, Phase::Ended) && signed_progress.abs() >= SWIPE_VERTICAL_COMMIT_PROGRESS {
                let (notif, label) = if signed_progress < 0.0 {
                    (DOCK_NOTIF_MISSION_CONTROL, "Mission Control")
                } else {
                    (DOCK_NOTIF_APP_EXPOSE, "App Exposé")
                };
                log::debug!(
                    "post: vertical swipe → {} via CoreDockSendNotification (progress={:+.3})",
                    label, signed_progress,
                );
                send_dock_notification(notif);
            } else {
                log::trace!(
                    "post: vertical swipe {:?} progress={:+.3} (no-op until Ended past ±{})",
                    phase, signed_progress, SWIPE_VERTICAL_COMMIT_PROGRESS,
                );
            }
            return;
        }
        let motion = match axis {
            SwipeAxis::Horizontal => SWIPE_MOTION_HORIZONTAL,
            SwipeAxis::Vertical => SWIPE_MOTION_VERTICAL,
        };
        let dock_phase = match phase {
            Phase::Began => SWIPE_PHASE_BEGAN,
            Phase::Changed => SWIPE_PHASE_CHANGED,
            Phase::Ended | Phase::Cancelled => SWIPE_PHASE_ENDED,
        };
        // Only Began establishes the active axis; Changed/Ended on a
        // foreign axis would split the event stream across two
        // gestures, which the Dock interprets as cancellation. Began
        // also resets the per-stream `last_progress` so the first
        // Began event's delta-x/y fields aren't inherited from the
        // previous gesture.
        match phase {
            Phase::Began => {
                self.swipe_axis.set(Some(axis));
                self.swipe_last_progress.set(0.0);
            }
            Phase::Ended | Phase::Cancelled => self.swipe_axis.set(None),
            Phase::Changed => {}
        }
        // Per-event delta along the locked axis. The Dock reads this
        // (split into X- and Y-components in fields 125/126) to route
        // the swipe — it appears to ignore the easily-set motion
        // field (123/165), since the OS clamped our writes to 1
        // regardless of source. With 125/126 zero (our prior shape),
        // every swipe was getting routed as horizontal-with-no-motion
        // and falling through to the Spaces handler.
        let delta = signed_progress - self.swipe_last_progress.get();
        self.swipe_last_progress.set(signed_progress);
        let (delta_x, delta_y_neg) = match axis {
            // Horizontal: per-event X delta carries the motion;
            // Y-component is zero. Sign of delta matches sign of
            // progress for forward motion, flips for reversal.
            SwipeAxis::Horizontal => (delta, 0.0),
            // Vertical: per-event Y delta (with the MBP convention
            // of inverted Y in field 126: positive when finger moves
            // up, i.e. opposite the screen-Y / cumulative-progress
            // sign). X-component is zero.
            SwipeAxis::Vertical => (0.0, -delta),
        };
        let flag = if signed_progress >= 0.0 { 1 } else { 0 };
        let velocity = matches!(phase, Phase::Ended).then(|| {
            // Saturating cap: preserve sign, clamp magnitude. See
            // SWIPE_END_VELOCITY_MAX docs for why — keeps fast-lift
            // commits in the MBP-natural-feel range.
            velocity_mm_per_sec.clamp(-SWIPE_END_VELOCITY_MAX, SWIPE_END_VELOCITY_MAX)
        });
        log::trace!(
            "post: swipe axis={:?} motion={} flag={} progress={:+.3} delta_x={:+.4} delta_y_neg={:+.4} v={:+.1} (capped {:+.1}) phase={:?}",
            axis,
            motion,
            flag,
            signed_progress,
            delta_x,
            delta_y_neg,
            velocity_mm_per_sec,
            velocity.unwrap_or(0.0),
            phase,
        );
        post_dock_swipe_pair(
            self.event_source,
            motion,
            flag,
            dock_phase,
            velocity,
            signed_progress,
            delta_x,
            delta_y_neg,
        );
    }
}

/// Function pointer signature for `CoreDockSendNotification`. Symbol
/// is exported from the dyld shared cache (no on-disk framework
/// binary). Resolved on first use via `dlsym(RTLD_DEFAULT, …)` and
/// cached. Returns 0 on success, nonzero CGError otherwise.
type CoreDockSendNotificationFn =
    unsafe extern "C" fn(notification: *const c_void, unknown: i32) -> i32;

/// Send a notification name to the Dock via the private
/// `CoreDockSendNotification` function — same path Hammerspoon's
/// `hs.spaces.toggleMissionControl` and `toggleAppExpose` use. Logs
/// and no-ops if the symbol can't be resolved or the call fails (the
/// alternative — keyboard or dock-control synthesis — is what we
/// fell back from, and there's no fourth path to try).
fn send_dock_notification(name: &str) {
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    use std::sync::OnceLock;

    static FN_PTR: OnceLock<Option<CoreDockSendNotificationFn>> = OnceLock::new();
    let f = FN_PTR.get_or_init(|| {
        // RTLD_DEFAULT only sees symbols from libraries already mapped
        // into the process. We don't link anything that drags this in,
        // so dlopen it explicitly first. Confirmed via
        // `dyld_info -all_dyld_cache -exports`: the symbol lives in
        // ApplicationServices.framework/Frameworks/HIServices.framework,
        // not SkyLight as the framework name might suggest. The
        // framework is in the dyld shared cache (no on-disk binary on
        // modern macOS); dlopen knows to look there for canonical
        // /System paths.
        let framework = c"/System/Library/Frameworks/ApplicationServices.framework/Frameworks/HIServices.framework/HIServices";
        let handle = unsafe { libc::dlopen(framework.as_ptr(), libc::RTLD_LAZY) };
        if handle.is_null() {
            let err = unsafe {
                let p = libc::dlerror();
                if p.is_null() {
                    "(no error)".to_string()
                } else {
                    std::ffi::CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            };
            log::warn!(
                "dlopen SkyLight.framework failed ({err}); \
                 vertical swipe → Mission Control / Exposé will not fire"
            );
            return None;
        }
        let symbol = c"CoreDockSendNotification";
        let raw = unsafe { libc::dlsym(handle, symbol.as_ptr()) };
        if raw.is_null() {
            log::warn!(
                "CoreDockSendNotification not found in HIServices.framework; \
                 vertical swipe → Mission Control / Exposé will not fire"
            );
            None
        } else {
            // SAFETY: the symbol exists and matches the documented
            // signature (verified via Hammerspoon's
            // extensions/spaces/private.h).
            Some(unsafe { std::mem::transmute::<*mut c_void, CoreDockSendNotificationFn>(raw) })
        }
    });
    let Some(f) = f else { return };
    let cf_name = CFString::new(name);
    let rc = unsafe { f(cf_name.as_concrete_TypeRef() as *const c_void, 0) };
    if rc != 0 {
        log::warn!("CoreDockSendNotification({name:?}, 0) returned {rc}");
    }
}

/// Build and post a single (DockControl + companion gesture) pair on
/// the session event tap. `source` is the persistent
/// `kCGEventSourceStateCombinedSessionState` source from the Emitter —
/// passing NULL here causes the OS to clamp the motion field (123)
/// from 2 to 1 on vertical swipes, routing them to the horizontal
/// Spaces handler. Verified empirically: identical event shape posted
/// with NULL source vs. session source differs only in the stored
/// motion value.
///
/// The companion is a bare `CGEventCreate(source); set field 55 = 29`
/// — no other fields. Without it, AppKit's gesture pipeline drops the
/// dock event as orphaned. `velocity` and `progress` are both signed
/// (sign carries direction). The real driver writes the velocity to
/// *both* fields 129 and 130 regardless of motion axis; matching that
/// here.
fn post_dock_swipe_pair(
    source: CGEventSourceRef,
    motion: i64,
    flag: i64,
    phase: i64,
    velocity: Option<f64>,
    progress: f64,
    delta_x: f64,
    delta_y_neg: f64,
) {
    let Some(dock) = Event::with_source(source) else { return };
    dock.set_int(FIELD_CGS_EVENT_TYPE, CGS_EVENT_TYPE_DOCK_CONTROL);
    dock.set_int(FIELD_GESTURE_HID_TYPE, IOHID_EVENT_TYPE_DOCK_SWIPE);
    dock.set_int(FIELD_GESTURE_PHASE, phase);
    dock.set_int(FIELD_GESTURE_PHASE_DUP, phase);
    dock.set_int(FIELD_SCROLL_GESTURE_FLAG_BITS, flag);
    dock.set_int(FIELD_GESTURE_SWIPE_MOTION, motion);
    dock.set_int(FIELD_GESTURE_SWIPE_MOTION_DUP, motion);
    dock.set_int(FIELD_GESTURE_CONST_136, 1);
    dock.set_int(FIELD_GESTURE_CONST_138, 3);
    dock.set_dbl(FIELD_GESTURE_SCROLL_Y, 0.0);
    // FLT_TRUE_MIN = smallest positive subnormal f32 ≈ 1.4e-45. The
    // Dock checks this field is non-zero; using the smallest possible
    // value avoids implying any actual zoom motion.
    dock.set_dbl(FIELD_GESTURE_ZOOM_DELTA_X, f64::from(f32::from_bits(1)));
    dock.set_dbl(FIELD_GESTURE_SWIPE_PROGRESS, progress);
    dock.set_dbl(FIELD_GESTURE_DELTA_X, delta_x);
    dock.set_dbl(FIELD_GESTURE_DELTA_Y_NEG, delta_y_neg);
    if let Some(v) = velocity {
        dock.set_dbl(FIELD_GESTURE_SWIPE_VELOCITY_X, v);
        dock.set_dbl(FIELD_GESTURE_SWIPE_VELOCITY_Y, v);
    }
    let Some(companion) = Event::with_source(source) else { return };
    companion.set_int(FIELD_CGS_EVENT_TYPE, CGS_EVENT_TYPE_GESTURE);
    dock.post_to(kCGSessionEventTap);
    companion.post_to(kCGSessionEventTap);
}

/// Post a single phased scroll event. Exactly one of `scroll_phase` and
/// `momentum_phase` should carry the active phase; the other goes on
/// the wire as `PHASE_NONE` (encoded by passing `Phase::Cancelled` for
/// the unused field). The integer pixel values drive line-equivalent
/// and point-delta fields; the float values drive the high-precision
/// `FixedPtDelta` field, so smooth-scroll-aware apps see sub-pixel
/// motion that integer truncation would otherwise drop.
///
/// Posted to `kCGSessionEventTap` (not the HID tap) so
/// AppleMultitouchHIDService doesn't merge the event into our PTP
/// device's gesture state, and using the persistent
/// combinedSessionState `source` so apps like Chrome accept this as a
/// real-trackpad fling worthy of rubber-band bounce.
fn post_scroll_event(
    source: CGEventSourceRef,
    int_x_px: i32,
    int_y_px: i32,
    float_x_px: f64,
    float_y_px: f64,
    scroll_phase: Phase,
    momentum_phase: Phase,
) {
    let Some(e) = Event::from_raw(unsafe {
        CGEventCreateScrollWheelEvent2(source, kCGScrollEventUnitPixel, 2, int_y_px, int_x_px, 0)
    }) else {
        return;
    };
    // `Phase::Cancelled` is the sentinel for "this field is unused on this
    // event"; cg_*_phase encodes that as 0 (none).
    let scroll_mask = cg_scroll_phase(scroll_phase);
    let momentum_mask = cg_momentum_phase(momentum_phase);
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
        scroll_phase,
        momentum_phase,
        int_x_px,
        int_y_px,
        float_x_px,
        float_y_px,
    );
    e.post_to(kCGSessionEventTap);
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
                speed,
                MOMENTUM_SEED_MM_PER_SEC,
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
            vx_mm_per_sec,
            vy_mm_per_sec,
        );
    }

    /// Stop coasting (if active), post a momentum-Ended bracket so apps
    /// can finalize their scroll animation, and release the timer.
    /// Returns `true` if a coast was actually active.
    fn cancel(&self) -> bool {
        let t = self.timer_ref.replace(std::ptr::null_mut());
        if t.is_null() {
            return false;
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
            post_scroll_event(
                self.event_source,
                0,
                0,
                0.0,
                0.0,
                Phase::Cancelled,
                Phase::Ended,
            );
        }
        log::debug!("scroll: inertia cancelled");
        true
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

        // Integrate to per-tick pixel displacement, applying the same
        // power curve as the active-scroll path so the user feels a
        // continuous deceleration from flick → coast (rather than a
        // step at lift from "amplified" to "linear").
        let dx_px = accelerate_scroll(vx, self.cfg.scroll_accel) * dt;
        let dy_px = accelerate_scroll(vy, self.cfg.scroll_accel) * dt;
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
        post_scroll_event(
            self.event_source,
            int_x,
            int_y,
            dx_px,
            dy_px,
            Phase::Cancelled,
            phase,
        );
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
        // `cancel` may post a final MomentumPhase::Ended via the event
        // source, so the source release has to come after.
        self.momentum.cancel();
        // Bracket any in-flight swipe with an Ended on the same axis
        // so the Dock doesn't leave the gesture half-open across our
        // shutdown. Sign of 0 progress is positive ⇒ flag = 1, fine
        // for cleanup since the Dock cancels at zero progress anyway.
        if let Some(axis) = self.swipe_axis.take() {
            let motion = match axis {
                SwipeAxis::Horizontal => SWIPE_MOTION_HORIZONTAL,
                SwipeAxis::Vertical => SWIPE_MOTION_VERTICAL,
            };
            post_dock_swipe_pair(self.event_source, motion, 1, SWIPE_PHASE_ENDED, Some(0.0), 0.0, 0.0, 0.0);
        }
        if !self.event_source.is_null() {
            unsafe { CFRelease(self.event_source as *const c_void) };
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
    fn set_left_button_held(&self, held: bool) {
        Emitter::set_left_button_held(self, held);
    }
    fn scroll(&self, dx_mm: f64, dy_mm: f64, phase: Phase) {
        Emitter::scroll(self, dx_mm, dy_mm, phase);
    }
    fn scroll_inertia(&self, vx_mm_per_sec: f64, vy_mm_per_sec: f64) {
        Emitter::scroll_inertia(self, vx_mm_per_sec, vy_mm_per_sec);
    }
    fn cancel_inertia(&self) -> bool {
        Emitter::cancel_inertia(self)
    }
    fn pinch(&self, delta: f64, phase: Phase) {
        Emitter::pinch(self, delta, phase);
    }
    fn rotate(&self, delta_degrees: f64, phase: Phase) {
        Emitter::rotate(self, delta_degrees, phase);
    }
    fn swipe(&self, axis: SwipeAxis, signed_progress: f64, velocity_mm_per_sec: f64, phase: Phase) {
        Emitter::swipe(self, axis, signed_progress, velocity_mm_per_sec, phase);
    }
}
