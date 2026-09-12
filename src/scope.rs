//! Gesture scope — a live view of what the engine is reasoning about.
//!
//! The recognizer's whole job is a judgement call made from numbers
//! nobody can see: contacts a few tenths of a millimetre apart, a
//! common/differential decomposition, three normalized scores racing
//! each other to 1.0. Until now that was observable only as a log line
//! after the fact:
//!
//! ```text
//! 2F lock=pinch+rotate scores[pinch=1.39 rot=0.16 pan=4.95 disq:margin] …
//! ```
//!
//! This window draws the same numbers while the fingers are still on
//! the pad: where the contacts are, where they landed, the path they
//! took, and the state of the 2F lock decision with every gate it turns
//! on. It renders a [`gesture::Snapshot`], which the engine hands to an
//! optional [`gesture::Observer`] — so the engine stays pure and does
//! not know this file exists.
//!
//! The same window renders a replay (`replay FILE --scope`), which is
//! the point of building it on the snapshot rather than on live HID: a
//! capture from a trackpad nobody here owns can be watched frame by
//! frame, and the transport below the canvas ([`Transport`]) lets you
//! stop on the frame where the lock went the wrong way.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};
use core_foundation_sys::runloop::{CFRunLoopTimerInvalidate, CFRunLoopTimerRef};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBackingStoreType, NSBezierPath, NSButton, NSColor, NSEvent,
    NSFont, NSFontAttributeName, NSForegroundColorAttributeName, NSSlider, NSStringDrawing,
    NSTextAlignment, NSTextField, NSView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

use crate::app_kit;
use crate::gesture::{
    ContactTrack, GestureKind, MultiMetrics, Observer, Snapshot, TwoFingerMetrics,
};
use crate::output::SwipeAxis;

/// Width of the drawn canvas. The window is this plus the tuning
/// column, when there is one.
const CANVAS_W: f64 = 780.0;
const WINDOW_H: f64 = 660.0;
/// Width of the live-tuning column down the right-hand side. Present
/// only when `main` has registered somewhere to apply the values —
/// a replay has an engine to tune, but tuning a recording's cursor
/// speed tells you nothing you can feel.
const TUNE_W: f64 = 244.0;
/// Quiet period after the last slider movement before the config file
/// is written. The engine already has the value — this only decides
/// how often the file is rewritten during a drag.
const FLUSH_DELAY_SECS: f64 = 0.25;
/// How often the tuning column re-reads the config file, so a change
/// made in the settings window or by hand shows up here too.
const CONFIG_POLL_SECS: f64 = 1.0;
/// Height reserved at the bottom for replay transport controls. Zero
/// in the daemon, where there is no timeline to scrub.
const TRANSPORT_H: f64 = 44.0;
/// Redraw cadence. The pad reports at 125 Hz and the engine hands over
/// a snapshot for every frame; drawing each one would spend more time
/// in AppKit than in the gesture engine for no visible gain, so frames
/// accumulate into the state below and the view is repainted on a
/// timer.
const REDRAW_HZ: f64 = 60.0;
/// Longest track kept per contact, in points. At 125 Hz this is about
/// five seconds, which is longer than any gesture worth studying.
const TRACK_POINTS_MAX: usize = 640;
/// Minimum movement (mm) before a new point is appended to a track.
/// Keeps a resting finger from filling the buffer with its own noise.
const TRACK_MIN_STEP_MM: f64 = 0.05;
/// Score at which a bar is full width. The lock threshold is 1.0, so
/// the threshold tick sits at the halfway mark and there is as much
/// room above it as below.
const BAR_FULL_SCORE: f64 = 2.0;
/// Radius a contact is drawn at, and the larger radius used while the
/// integrated button is held.
const CONTACT_RADIUS: f64 = 8.0;
const CONTACT_RADIUS_PRESSED: f64 = 11.5;
/// Pad size assumed when no device has reported one (a capture taken
/// before the descriptor was parsed, mostly). Drawn with a warning so
/// nobody reads millimetres off it.
const FALLBACK_PAD_MM: (f64, f64) = (100.0, 60.0);

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
    static SCOPE: RefCell<ScopeState> = RefCell::new(ScopeState::default());
    static TRANSPORT: RefCell<Option<Box<dyn Transport>>> = const { RefCell::new(None) };
    /// Mirrors `WINDOW.is_some() && visible`. Read once per frame by
    /// the observer, which runs in the HID callback.
    static OPEN: Cell<bool> = const { Cell::new(false) };
    /// Registered by `main`, which owns the engine. Same shape as
    /// [`crate::pause::set_settle_hook`]: the scope knows what the
    /// numbers mean, `main` knows how to reach the engine, and neither
    /// needs the other's knowledge.
    static LIVE_APPLY: RefCell<Option<LiveApply>> = const { RefCell::new(None) };
    /// Last known values of the tunables, so the canvas can show
    /// what the curves do even when there is no column (a replay reads
    /// them from the config file like everything else).
    static TUNING: Cell<Tuning> = Cell::new(Tuning::fallback());
}

// -------------------------------------------------------------- tuning

/// Where a slider drag lands. `main` supplies one; see [`set_live_apply`].
type LiveApply = Box<dyn Fn(Tuning)>;

/// The settings the scope can change while you gesture.
///
/// Deliberately only these: they are *feel*, not recognition. A
/// slider that moved a lock threshold would let you fix the gesture in
/// front of you while silently breaking the one you tried yesterday,
/// and with a single test device there would be no way to notice. See
/// `docs/known-gaps.md`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    pub cursor_sensitivity: f64,
    pub cursor_accel_exponent: f64,
    pub cursor_accel_ref: f64,
    pub scroll_sensitivity: f64,
    pub scroll_accel_exponent: f64,
    pub scroll_accel_ref: f64,
}

impl Tuning {
    /// Used only before the config file has been read — a canvas that
    /// drew no curve at all would be worse than one drawn from the
    /// documented defaults. Taken from `Config::default()` rather than
    /// retyped, so it cannot drift from them.
    fn fallback() -> Self {
        Self::from_config(&crate::config::Config::default())
    }

    /// The scroll numbers in the shape the emitter's curve wants.
    fn scroll_curve(&self) -> crate::output::ScrollAccel {
        crate::output::ScrollAccel {
            px_per_mm_at_ref: self.scroll_sensitivity,
            exponent: self.scroll_accel_exponent,
            ref_mm_per_sec: self.scroll_accel_ref,
        }
    }

    fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            cursor_sensitivity: cfg.cursor.sensitivity,
            cursor_accel_exponent: cfg.cursor.accel_exponent,
            cursor_accel_ref: cfg.cursor.accel_ref,
            scroll_sensitivity: cfg.scroll.sensitivity,
            scroll_accel_exponent: cfg.scroll.accel_exponent,
            scroll_accel_ref: cfg.scroll.accel_ref,
        }
    }
}

/// Register where a slider drag should land. Called by `main` with a
/// closure that reaches the running engine; without it the scope shows
/// no tuning column at all.
pub fn set_live_apply(f: impl Fn(Tuning) + 'static) {
    LIVE_APPLY.with(|h| *h.borrow_mut() = Some(Box::new(f)));
}

fn current_tuning() -> Tuning {
    TUNING.with(Cell::get)
}

/// Read the tunables off disk. The config file stays the shape of
/// truth even while the engine is running ahead of it.
fn tuning_from_file() -> Tuning {
    crate::settings::config_path()
        .and_then(|p| crate::config::load(Some(&p)).ok())
        .map(|(cfg, _)| Tuning::from_config(&cfg))
        .unwrap_or_else(Tuning::fallback)
}

// ---------------------------------------------------------------- feed

/// The [`Observer`] handed to the gesture engine. Stateless: it only
/// forwards into this module's window state, and reports that it wants
/// nothing at all while the window is closed — so a daemon whose scope
/// has never been opened pays one `Cell` read per frame.
pub struct Feed;

impl Observer for Feed {
    fn wants_frames(&self) -> bool {
        OPEN.with(Cell::get)
    }

    fn frame(&self, snapshot: &Snapshot) {
        SCOPE.with(|s| s.borrow_mut().ingest(snapshot));
    }
}

/// Whether the scope window is currently showing.
pub fn is_open() -> bool {
    OPEN.with(Cell::get)
}

// ----------------------------------------------------------- transport

/// Playback control for a scope that is rendering something other than
/// a live device. Registered by `replay`; absent in the daemon, which
/// shows no transport row at all.
pub trait Transport {
    fn toggle_play(&self);
    /// Move `delta` frames, clamped to the stream.
    fn step(&self, delta: i64);
    /// Seek to a count of processed frames, from 0 through the total.
    fn seek(&self, frame: usize);
    /// `(processed frames, total frames, playing)`; the total is reachable.
    fn position(&self) -> (usize, usize, bool);
}

/// Install playback controls. Call before [`show`]; the transport row
/// is built with the window.
pub fn set_transport(transport: Box<dyn Transport>) {
    TRANSPORT.with(|t| *t.borrow_mut() = Some(transport));
}

fn with_transport<R>(f: impl FnOnce(&dyn Transport) -> R) -> Option<R> {
    TRANSPORT.with(|t| t.borrow().as_ref().map(|t| f(t.as_ref())))
}

// --------------------------------------------------------- scope state

/// One contact's path across the pad. Rendering state, deliberately
/// kept here rather than in the engine: the engine has no use for a
/// history it never reads, and `Tracked` staying two points wide is
/// part of what keeps it cheap.
struct Track {
    id: u8,
    points: Vec<(f64, f64)>,
}

/// The 2F decision, frozen at the frame it was made. Scores keep
/// growing after the lock, so the interesting values are the ones that
/// crossed — not whatever they read by the time you look up.
struct LockRecord {
    kind: GestureKind,
    metrics: TwoFingerMetrics,
    /// How far into the gesture the lock happened.
    after: Duration,
}

struct ScopeState {
    latest: Option<Snapshot>,
    tracks: Vec<Track>,
    /// Set when every finger has lifted. The tracks stay on screen so
    /// the gesture that just happened can still be read; the next touch
    /// clears them.
    ghost: bool,
    lock: Option<LockRecord>,
    prev_kind: GestureKind,
    dirty: bool,
    /// Frames seen since the window opened, purely so the header can
    /// show that data really is arriving.
    frames: u64,
}

impl Default for ScopeState {
    fn default() -> Self {
        Self {
            latest: None,
            tracks: Vec::new(),
            ghost: false,
            lock: None,
            prev_kind: GestureKind::Idle,
            dirty: false,
            frames: 0,
        }
    }
}

impl ScopeState {
    /// Forget everything. Used when the window opens, and on a replay
    /// seek that rewinds past the current position.
    fn reset(&mut self) {
        *self = Self::default();
        self.dirty = true;
    }

    fn ingest(&mut self, s: &Snapshot) {
        self.frames += 1;

        if s.contacts.is_empty() {
            if !self.tracks.is_empty() {
                self.ghost = true;
            }
        } else {
            if self.ghost {
                self.tracks.clear();
                self.lock = None;
                self.ghost = false;
            }
            for c in &s.contacts {
                self.append(c);
            }
        }

        // A lock is the transition into one of the two locked 2F kinds.
        // Catching it on the transition rather than on the score means
        // a lock restored after a partial lift shows up too.
        let locked_now = matches!(
            s.kind,
            GestureKind::TwoFingerPan | GestureKind::TwoFingerPinchAndRotate
        );
        let was_locked = matches!(
            self.prev_kind,
            GestureKind::TwoFingerPan | GestureKind::TwoFingerPinchAndRotate
        );
        if let (true, Some(metrics)) = (locked_now && !was_locked, s.two_finger) {
            self.lock = Some(LockRecord {
                kind: s.kind,
                metrics,
                after: s.since_start,
            });
        }
        self.prev_kind = s.kind;

        self.latest = Some(s.clone());
        self.dirty = true;
    }

    fn append(&mut self, c: &ContactTrack) {
        let point = (c.x_mm, c.y_mm);
        if let Some(track) = self.tracks.iter_mut().find(|t| t.id == c.id) {
            let last = track.points.last().copied().unwrap_or(point);
            let moved = ((point.0 - last.0).powi(2) + (point.1 - last.1).powi(2)).sqrt();
            if moved >= TRACK_MIN_STEP_MM || track.points.is_empty() {
                track.points.push(point);
                if track.points.len() > TRACK_POINTS_MAX {
                    track.points.remove(0);
                }
            }
        } else {
            self.tracks.push(Track {
                id: c.id,
                points: vec![point],
            });
        }
    }
}

/// Clear the accumulated view state. Public so a replay seek can put
/// the scope back to a clean slate before re-feeding frames.
pub fn reset() {
    SCOPE.with(|s| s.borrow_mut().reset());
}

// -------------------------------------------------------------- window

define_class!(
    #[unsafe(super(NSView))]
    #[thread_kind = MainThreadOnly]
    #[name = "TrackpadCompanionScopeView"]
    struct ScopeView;

    impl ScopeView {
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            // PTP reports the origin at the top-left with Y growing
            // downward. Flipping the view makes millimetres map onto
            // points with nothing but a scale factor — no sign flip to
            // get wrong, and the drawing reads like the spec.
            true
        }

        #[unsafe(method(acceptsFirstResponder))]
        fn accepts_first_responder(&self) -> bool {
            true
        }

        #[unsafe(method(drawRect:))]
        fn draw_rect(&self, _dirty: NSRect) {
            draw(self.bounds());
        }

        #[unsafe(method(keyDown:))]
        fn key_down(&self, event: &NSEvent) {
            // Studying a misclassification means stopping on the frame
            // it happened and stepping around it, which is unusable
            // with a mouse. Space plays, arrows step, shift steps by
            // ten.
            let chars = event.charactersIgnoringModifiers();
            let shift = event
                .modifierFlags()
                .contains(objc2_app_kit::NSEventModifierFlags::Shift);
            let stride = if shift { 10 } else { 1 };
            let key = chars.map(|c| c.to_string()).unwrap_or_default();
            let mut handled = true;
            match key.chars().next() {
                Some(' ') => {
                    with_transport(|t| t.toggle_play());
                }
                // NSLeftArrowFunctionKey / NSRightArrowFunctionKey.
                Some('\u{F702}') => {
                    with_transport(|t| t.step(-stride));
                }
                Some('\u{F703}') => {
                    with_transport(|t| t.step(stride));
                }
                // Escape. A diagnostic window you opened to glance at
                // should close without aiming at anything.
                Some('\u{1b}') => close(),
                _ => handled = false,
            }
            if !handled {
                let _: () = unsafe { msg_send![super(self), keyDown: event] };
            }
        }
    }
);

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TrackpadCompanionScopeActions"]
    struct Actions;

    impl Actions {
        #[unsafe(method(togglePlay:))]
        fn toggle_play(&self, _s: Option<&AnyObject>) {
            with_transport(|t| t.toggle_play());
        }

        #[unsafe(method(stepBack:))]
        fn step_back(&self, _s: Option<&AnyObject>) {
            with_transport(|t| t.step(-1));
        }

        #[unsafe(method(stepForward:))]
        fn step_forward(&self, _s: Option<&AnyObject>) {
            with_transport(|t| t.step(1));
        }

        #[unsafe(method(tuneSensitivity:))]
        fn tune_sensitivity(&self, _s: Option<&AnyObject>) {
            slider_moved();
        }

        #[unsafe(method(tuneExponent:))]
        fn tune_exponent(&self, _s: Option<&AnyObject>) {
            slider_moved();
        }

        #[unsafe(method(tuneAccelRef:))]
        fn tune_accel_ref(&self, _s: Option<&AnyObject>) {
            slider_moved();
        }

        #[unsafe(method(tuneScroll:))]
        fn tune_scroll(&self, _s: Option<&AnyObject>) {
            slider_moved();
        }

        #[unsafe(method(tuneScrollExponent:))]
        fn tune_scroll_exponent(&self, _s: Option<&AnyObject>) {
            slider_moved();
        }

        #[unsafe(method(tuneScrollAccelRef:))]
        fn tune_scroll_accel_ref(&self, _s: Option<&AnyObject>) {
            slider_moved();
        }

        #[unsafe(method(closeScope:))]
        fn close_scope(&self, _s: Option<&AnyObject>) {
            close();
        }

        #[unsafe(method(scrub:))]
        fn scrub(&self, _s: Option<&AnyObject>) {
            WINDOW.with(|cell| {
                if let Some(ui) = cell.borrow().as_ref().and_then(|w| w.transport.as_ref()) {
                    let frame = ui.slider.doubleValue().round().max(0.0) as usize;
                    with_transport(|t| t.seek(frame));
                }
            });
        }
    }
);

/// Close the window the same way the title bar's own button does, so
/// the teardown path — flushing a pending write, dropping the redraw
/// timer, releasing the activation policy — is the one already proven
/// by the red button rather than a second copy of it.
fn close() {
    // Take a reference out of the borrow before calling into AppKit:
    // performClose: is a message send, and holding a `RefCell` borrow
    // across one is how a re-entrant callback turns into a panic.
    let window = WINDOW.with(|cell| cell.borrow().as_ref().map(|w| w.window.clone()));
    if let Some(window) = window {
        window.performClose(None);
    }
}

/// Every tuning slider lands here; which one moved doesn't matter,
/// since all are written together.
fn slider_moved() {
    WINDOW.with(|cell| {
        if let Some(w) = cell.borrow_mut().as_mut() {
            w.slider_moved();
        }
    });
}

impl Actions {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let _ = mtm;
        unsafe { msg_send![Self::alloc(), init] }
    }
}

struct TransportUi {
    play: Retained<NSButton>,
    slider: Retained<NSSlider>,
    label: Retained<NSTextField>,
}

/// The live-tuning column. One slider per tunable, plus the section
/// headers, which brighten for whichever group the gesture in your
/// hand is actually exercising — you cannot feel cursor acceleration
/// with two fingers down, and the column should say so.
struct TuneUi {
    cursor_header: Retained<NSTextField>,
    scroll_header: Retained<NSTextField>,
    sensitivity: Retained<NSSlider>,
    sensitivity_value: Retained<NSTextField>,
    exponent: Retained<NSSlider>,
    exponent_value: Retained<NSTextField>,
    accel_ref: Retained<NSSlider>,
    accel_ref_value: Retained<NSTextField>,
    scroll: Retained<NSSlider>,
    scroll_value: Retained<NSTextField>,
    scroll_exponent: Retained<NSSlider>,
    scroll_exponent_value: Retained<NSTextField>,
    scroll_accel_ref: Retained<NSSlider>,
    scroll_accel_ref_value: Retained<NSTextField>,
}

impl TuneUi {
    /// What the sliders currently read, rounded the same way the
    /// settings window rounds them so the two write identical values.
    fn values(&self) -> Tuning {
        Tuning {
            cursor_sensitivity: crate::settings::round1(self.sensitivity.doubleValue()),
            cursor_accel_exponent: crate::settings::round2(self.exponent.doubleValue()),
            cursor_accel_ref: crate::settings::round1(self.accel_ref.doubleValue()),
            scroll_sensitivity: crate::settings::round1(self.scroll.doubleValue()),
            scroll_accel_exponent: crate::settings::round2(self.scroll_exponent.doubleValue()),
            scroll_accel_ref: crate::settings::round1(self.scroll_accel_ref.doubleValue()),
        }
    }

    fn show(&self, t: Tuning) {
        self.sensitivity.setDoubleValue(t.cursor_sensitivity);
        self.exponent.setDoubleValue(t.cursor_accel_exponent);
        self.accel_ref.setDoubleValue(t.cursor_accel_ref);
        self.scroll.setDoubleValue(t.scroll_sensitivity);
        self.scroll_exponent.setDoubleValue(t.scroll_accel_exponent);
        self.scroll_accel_ref.setDoubleValue(t.scroll_accel_ref);
        self.relabel(t);
    }

    fn relabel(&self, t: Tuning) {
        self.sensitivity_value
            .setStringValue(&NSString::from_str(&format!(
                "{:.1} px/mm",
                t.cursor_sensitivity
            )));
        self.exponent_value
            .setStringValue(&NSString::from_str(&format!(
                "{:.2}",
                t.cursor_accel_exponent
            )));
        self.accel_ref_value
            .setStringValue(&NSString::from_str(&format!(
                "{:.0} mm/s",
                t.cursor_accel_ref
            )));
        self.scroll_value
            .setStringValue(&NSString::from_str(&format!(
                "{:.1} px/mm",
                t.scroll_sensitivity
            )));
        self.scroll_exponent_value
            .setStringValue(&NSString::from_str(&format!(
                "{:.2}",
                t.scroll_accel_exponent
            )));
        self.scroll_accel_ref_value
            .setStringValue(&NSString::from_str(&format!(
                "{:.0} mm/s",
                t.scroll_accel_ref
            )));
    }
}

struct Window {
    window: Retained<NSWindow>,
    view: Retained<ScopeView>,
    timer: Option<CFRunLoopTimer>,
    transport: Option<TransportUi>,
    tune: Option<TuneUi>,
    _close: Option<Retained<NSButton>>,
    /// Pending debounced write of the tunables. The engine already
    /// has them; this is only about not rewriting the file sixty times
    /// a second during a drag.
    flush_timer: Option<CFRunLoopTimer>,
    /// Config mtime as of the last read, so an edit made in the
    /// settings window or by hand lands here too.
    seen_mtime: Option<std::time::SystemTime>,
    last_config_poll: f64,
    _actions: Retained<Actions>,
}

/// Open the scope, creating it on first use.
pub fn show(mtm: MainThreadMarker) {
    WINDOW.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Window::build(mtm));
        }
        if let Some(w) = slot.as_mut() {
            w.present(mtm);
        }
    });
    SCOPE.with(|s| s.borrow_mut().reset());
    OPEN.with(|o| o.set(true));
}

impl Window {
    fn build(mtm: MainThreadMarker) -> Self {
        app_kit::ensure_app(mtm);

        let has_transport = TRANSPORT.with(|t| t.borrow().is_some());
        let transport_h = if has_transport { TRANSPORT_H } else { 0.0 };
        let has_tuning = LIVE_APPLY.with(|h| h.borrow().is_some());
        let tune_w = if has_tuning { TUNE_W } else { 0.0 };
        let window_w = CANVAS_W + tune_w;

        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(window_w, WINDOW_H));
        let window: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect,
                NSWindowStyleMask::Titled
                    | NSWindowStyleMask::Closable
                    | NSWindowStyleMask::Resizable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("Gesture Scope"));
        // Same trap as the settings window: the default frees the
        // window on close, dangling the `Retained` held here.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setMinSize(NSSize::new(560.0 + tune_w, 520.0));
        app_kit::register_window(window.clone());

        let content = window
            .contentView()
            .expect("NSWindow auto-creates a contentView");

        let view_frame = NSRect::new(
            NSPoint::new(0.0, transport_h),
            NSSize::new(CANVAS_W, WINDOW_H - transport_h),
        );
        let view: Retained<ScopeView> =
            unsafe { msg_send![mtm.alloc::<ScopeView>(), initWithFrame: view_frame] };
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable
                | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        content.addSubview(&view);

        let actions = Actions::new(mtm);
        let transport = has_transport.then(|| {
            let play = push(mtm, "Play", &actions, sel!(togglePlay:), 12.0, 8.0, 74.0);
            let back = push(mtm, "◀", &actions, sel!(stepBack:), 92.0, 8.0, 44.0);
            let fwd = push(mtm, "▶", &actions, sel!(stepForward:), 140.0, 8.0, 44.0);
            let slider = unsafe {
                NSSlider::sliderWithValue_minValue_maxValue_target_action(
                    0.0,
                    0.0,
                    1.0,
                    Some(actions.as_ref() as &AnyObject),
                    Some(sel!(scrub:)),
                    mtm,
                )
            };
            slider.setFrame(NSRect::new(
                NSPoint::new(192.0, 8.0),
                NSSize::new(CANVAS_W - 192.0 - 292.0, 24.0),
            ));
            slider.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);
            let label = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            label.setFrame(NSRect::new(
                NSPoint::new(CANVAS_W - 284.0, 10.0),
                NSSize::new(160.0, 18.0),
            ));
            label.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(11.0, 0.0)));
            label.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            content.addSubview(&play);
            content.addSubview(&back);
            content.addSubview(&fwd);
            content.addSubview(&slider);
            content.addSubview(&label);
            TransportUi {
                play,
                slider,
                label,
            }
        });

        let tune = has_tuning.then(|| Self::build_tuning(mtm, &content, &actions));

        // Bottom of the tuning column when there is one, otherwise the
        // right-hand end of the transport row. Both are the window's
        // bottom-right, which is where a dismiss lives on this
        // platform; it just depends which row is down there.
        let close_btn = if has_tuning {
            Some(push(
                mtm,
                "Close",
                &actions,
                sel!(closeScope:),
                CANVAS_W + TUNE_W - 16.0 - 88.0,
                16.0,
                88.0,
            ))
        } else if has_transport {
            Some(push(
                mtm,
                "Close",
                &actions,
                sel!(closeScope:),
                CANVAS_W - 104.0,
                8.0,
                88.0,
            ))
        } else {
            // Neither row exists, so there is nowhere to put a button
            // that wouldn't sit on top of the canvas. The title bar's
            // own close button still works.
            None
        };
        if let Some(b) = close_btn.as_ref() {
            b.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            content.addSubview(b);
        }

        let mut me = Self {
            window,
            view,
            timer: None,
            transport,
            tune,
            _close: close_btn,
            flush_timer: None,
            seen_mtime: None,
            last_config_poll: 0.0,
            _actions: actions,
        };
        me.adopt_file(false);
        me
    }

    /// Lay the column out from the top down. AppKit's origin is
    /// bottom-left, so each row subtracts.
    fn build_tuning(
        mtm: MainThreadMarker,
        content: &Retained<NSView>,
        actions: &Retained<Actions>,
    ) -> TuneUi {
        let x = CANVAS_W + 16.0;
        let w = TUNE_W - 32.0;
        let mut y = WINDOW_H - 34.0;

        let title = label(mtm, "Live tuning", x, y, w, 18.0);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        content.addSubview(&title);
        y -= 34.0;
        let note = label(mtm, "Applies when you let go.", x, y, w, 32.0);
        note.setFont(Some(&NSFont::systemFontOfSize(10.0)));
        content.addSubview(&note);
        y -= 26.0;

        let row = |mtm: MainThreadMarker,
                   name: &str,
                   min: f64,
                   max: f64,
                   action: objc2::runtime::Sel,
                   y: &mut f64|
         -> (Retained<NSSlider>, Retained<NSTextField>) {
            *y -= 20.0;
            let l = label(mtm, name, x, *y, w - 88.0, 16.0);
            l.setFont(Some(&NSFont::systemFontOfSize(11.0)));
            content.addSubview(&l);
            let value = label(mtm, "", x + w - 88.0, *y, 88.0, 16.0);
            value.setFont(Some(&NSFont::monospacedSystemFontOfSize_weight(10.0, 0.0)));
            value.setAlignment(NSTextAlignment::Right);
            content.addSubview(&value);
            *y -= 24.0;
            let slider = unsafe {
                NSSlider::sliderWithValue_minValue_maxValue_target_action(
                    min,
                    min,
                    max,
                    Some(actions.as_ref() as &AnyObject),
                    Some(action),
                    mtm,
                )
            };
            slider.setFrame(NSRect::new(NSPoint::new(x, *y), NSSize::new(w, 20.0)));
            slider.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
            content.addSubview(&slider);
            (slider, value)
        };

        let cursor_header = label(mtm, "Cursor", x, y, w, 18.0);
        cursor_header.setFont(Some(&NSFont::boldSystemFontOfSize(11.0)));
        content.addSubview(&cursor_header);
        y -= 8.0;
        let (sensitivity, sensitivity_value) = row(
            mtm,
            "Speed",
            crate::settings::CURSOR_MIN,
            crate::settings::CURSOR_MAX,
            sel!(tuneSensitivity:),
            &mut y,
        );
        let (exponent, exponent_value) = row(
            mtm,
            "Acceleration",
            crate::settings::EXPONENT_MIN,
            crate::settings::EXPONENT_MAX,
            sel!(tuneExponent:),
            &mut y,
        );
        let (accel_ref, accel_ref_value) = row(
            mtm,
            "Accel reference",
            crate::settings::ACCEL_REF_MIN,
            crate::settings::ACCEL_REF_MAX,
            sel!(tuneAccelRef:),
            &mut y,
        );
        y -= 26.0;
        let scroll_header = label(mtm, "Scroll", x, y, w, 18.0);
        scroll_header.setFont(Some(&NSFont::boldSystemFontOfSize(11.0)));
        content.addSubview(&scroll_header);
        y -= 8.0;
        let (scroll, scroll_value) = row(
            mtm,
            "Speed",
            crate::settings::SCROLL_MIN,
            crate::settings::SCROLL_MAX,
            sel!(tuneScroll:),
            &mut y,
        );
        let (scroll_exponent, scroll_exponent_value) = row(
            mtm,
            "Acceleration",
            crate::settings::EXPONENT_MIN,
            crate::settings::EXPONENT_MAX,
            sel!(tuneScrollExponent:),
            &mut y,
        );
        let (scroll_accel_ref, scroll_accel_ref_value) = row(
            mtm,
            "Accel reference",
            crate::settings::ACCEL_REF_MIN,
            crate::settings::ACCEL_REF_MAX,
            sel!(tuneScrollAccelRef:),
            &mut y,
        );

        for f in [&cursor_header, &scroll_header, &title, &note] {
            f.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);
        }

        TuneUi {
            cursor_header,
            scroll_header,
            sensitivity,
            sensitivity_value,
            exponent,
            exponent_value,
            accel_ref,
            accel_ref_value,
            scroll,
            scroll_value,
            scroll_exponent,
            scroll_exponent_value,
            scroll_accel_ref,
            scroll_accel_ref_value,
        }
    }

    /// Take what a slider now reads, hand it to the engine, and queue
    /// the file write behind it.
    ///
    /// The engine-first order is the point: going through the file the
    /// way the settings window does costs a debounce plus a poll, which
    /// is over a second before your fingers feel anything. The file
    /// still ends up authoritative — when the watcher notices it, it
    /// re-applies the identical values.
    ///
    /// But *not* while the knob is still held. You drag these sliders
    /// with the trackpad they configure, so applying mid-drag changes
    /// the pointer doing the dragging: the knob stops tracking your
    /// finger, and at the low end of Speed it takes several times the
    /// travel to drag it back. Nothing is lost by waiting — you cannot
    /// perform the gesture you are trying to judge while your finger is
    /// on the knob anyway, so "live" was only ever worth anything from
    /// the moment you let go. Which is exactly when this fires.
    ///
    /// A keyboard change (arrow keys on a focused slider) has no button
    /// held, so it applies at once.
    fn slider_moved(&mut self) {
        let Some(ui) = self.tune.as_ref() else {
            return;
        };
        let values = ui.values();
        // The number under the slider tracks the drag regardless — you
        // should be able to see where you are heading.
        ui.relabel(values);
        if NSEvent::pressedMouseButtons() & 1 != 0 {
            return;
        }
        // Only now, with the engine actually taking the value, does the
        // canvas get to draw the curve — a readout showing a curve the
        // engine isn't running would be worse than none.
        TUNING.with(|t| t.set(values));
        LIVE_APPLY.with(|h| {
            if let Some(f) = h.borrow().as_ref() {
                f(values);
            }
        });
        self.schedule_flush();
    }

    fn schedule_flush(&mut self) {
        if let Some(timer) = self.flush_timer.take() {
            unsafe { CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef()) };
        }
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        let fire_at = unsafe { CFAbsoluteTimeGetCurrent() } + FLUSH_DELAY_SECS;
        let timer = CFRunLoopTimer::new(fire_at, 0.0, 0, 0, on_flush, &mut ctx);
        CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopCommonModes });
        self.flush_timer = Some(timer);
    }

    /// Write every value in one format-preserving edit, through the
    /// settings window's own helper.
    fn flush(&mut self) {
        self.flush_timer = None;
        let Some(ui) = self.tune.as_ref() else {
            return;
        };
        let v = ui.values();
        let written = crate::settings::edit(|c| {
            c.set_f64(&["cursor"], "sensitivity", v.cursor_sensitivity)?;
            c.set_f64(&["cursor"], "accel_exponent", v.cursor_accel_exponent)?;
            c.set_f64(&["cursor"], "accel_ref", v.cursor_accel_ref)?;
            c.set_f64(&["scroll"], "sensitivity", v.scroll_sensitivity)?;
            c.set_f64(&["scroll"], "accel_exponent", v.scroll_accel_exponent)?;
            c.set_f64(&["scroll"], "accel_ref", v.scroll_accel_ref)?;
            Ok(())
        });
        if written {
            self.seen_mtime = Self::config_mtime();
            return;
        }
        // The engine is running on a value the file refused. Applying
        // it first is what makes the slider feel instant; leaving it
        // applied after the write failed would be a divergence nobody
        // could see — the scope would show one thing, the file another,
        // and the next reload would silently undo whatever you tuned.
        // Put the slider and the engine back on what is actually on
        // disk instead.
        log::warn!("gesture scope: tuning write failed, reverting to the config file");
        self.adopt_file(true);
    }

    fn config_mtime() -> Option<std::time::SystemTime> {
        crate::settings::config_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
    }

    /// Adopt what is on disk. Also runs at build time, which is how the
    /// sliders start on the right values.
    ///
    /// `to_engine` pushes the values on to the engine as well. Normally
    /// false: the file watcher is already doing that, and doing it
    /// twice is only noise. True on the revert path, where the engine
    /// is the thing that needs correcting and waiting up to a second
    /// for the watcher would leave it wrong in the meantime.
    fn adopt_file(&mut self, to_engine: bool) {
        let values = tuning_from_file();
        TUNING.with(|t| t.set(values));
        if let Some(ui) = self.tune.as_ref() {
            ui.show(values);
        }
        if to_engine {
            LIVE_APPLY.with(|h| {
                if let Some(f) = h.borrow().as_ref() {
                    f(values);
                }
            });
        }
        self.seen_mtime = Self::config_mtime();
    }

    fn present(&mut self, mtm: MainThreadMarker) {
        self.adopt_file(false);
        app_kit::activate_for_window(mtm);
        self.window.center();
        self.window.makeKeyAndOrderFront(None);
        // Keyboard transport only works if the canvas is first
        // responder; the buttons would otherwise steal it on first
        // click and never give it back. Only where there is a transport
        // to drive, though — in the daemon the canvas has no keys of
        // its own worth claiming, and holding first responder would
        // stop a click from focusing a tuning slider, which is how you
        // nudge one with the arrow keys instead of dragging it.
        if self.transport.is_some() {
            self.window.makeFirstResponder(Some(&self.view));
        }

        if self.timer.is_none() {
            let interval = 1.0 / REDRAW_HZ;
            let mut ctx = CFRunLoopTimerContext {
                version: 0,
                info: std::ptr::null_mut(),
                retain: None,
                release: None,
                copyDescription: None,
            };
            let fire_at = unsafe { CFAbsoluteTimeGetCurrent() } + interval;
            let timer = CFRunLoopTimer::new(fire_at, interval, 0, 0, on_tick, &mut ctx);
            // Common modes, not default: a menu tracking loop would
            // otherwise freeze the scope exactly when someone opens the
            // menu to look at it.
            CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopCommonModes });
            self.timer = Some(timer);
        }
    }

    /// Repaint if a frame arrived, and keep the two control rows honest.
    fn tick(&mut self) {
        let dirty = SCOPE.with(|s| std::mem::replace(&mut s.borrow_mut().dirty, false));
        if dirty {
            self.view.setNeedsDisplay(true);
        }
        self.tick_tuning();
        self.tick_transport();
    }

    fn tick_tuning(&mut self) {
        if self.tune.is_none() {
            return;
        }
        // Never while a write of our own is pending — that is exactly
        // the mid-drag case where a refresh would yank the slider out
        // from under the hand holding it.
        let now = unsafe { CFAbsoluteTimeGetCurrent() };
        if self.flush_timer.is_none() && now - self.last_config_poll >= CONFIG_POLL_SECS {
            self.last_config_poll = now;
            if Self::config_mtime() != self.seen_mtime {
                self.adopt_file(false);
            }
        }

        // Brighten whichever group the gesture in flight is actually
        // exercising. Neither, most of the time — that is honest.
        let kind = SCOPE.with(|s| s.borrow().latest.as_ref().map(|l| l.kind));
        let cursor_live = matches!(kind, Some(GestureKind::OneFinger));
        let scroll_live = matches!(kind, Some(GestureKind::TwoFingerPan));
        if let Some(ui) = self.tune.as_ref() {
            let lit = NSColor::labelColor();
            let unlit = NSColor::tertiaryLabelColor();
            ui.cursor_header
                .setTextColor(Some(if cursor_live { &lit } else { &unlit }));
            ui.scroll_header
                .setTextColor(Some(if scroll_live { &lit } else { &unlit }));
        }
    }

    fn tick_transport(&mut self) {
        let Some(ui) = self.transport.as_ref() else {
            return;
        };
        let Some((frame, total, playing)) = with_transport(|t| t.position()) else {
            return;
        };
        ui.play
            .setTitle(&NSString::from_str(if playing { "Pause" } else { "Play" }));
        // Position counts processed frames: 0 is before the first, total is after the last.
        ui.slider.setMaxValue(total as f64);
        ui.slider.setDoubleValue(frame as f64);
        ui.label
            .setStringValue(&NSString::from_str(&format!("{frame} / {total}")));
    }

    fn teardown(&mut self, mtm: MainThreadMarker) {
        if let Some(timer) = self.timer.take() {
            unsafe { CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef()) };
        }
        // A pending write must still land — closing the window mid-drag
        // shouldn't lose the value the engine is already running on.
        if self.flush_timer.is_some() {
            self.flush();
        }
        OPEN.with(|o| o.set(false));
        app_kit::settle_activation(mtm);
    }
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Retained<NSTextField> {
    let f = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    f.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
    f
}

extern "C" fn on_flush(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    WINDOW.with(|cell| {
        if let Some(w) = cell.borrow_mut().as_mut() {
            w.flush();
        }
    });
}

extern "C" fn on_tick(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    WINDOW.with(|cell| {
        if let Some(w) = cell.borrow_mut().as_mut() {
            if w.window.isVisible() {
                w.tick();
            } else {
                w.teardown(mtm);
            }
        }
    });
}

fn push(
    mtm: MainThreadMarker,
    title: &str,
    target: &Retained<Actions>,
    action: objc2::runtime::Sel,
    x: f64,
    y: f64,
    w: f64,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target.as_ref() as &AnyObject),
            Some(action),
            mtm,
        )
    };
    b.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, 28.0)));
    b
}

// ------------------------------------------------------------ drawing
//
// Everything below is pure rendering of a `Snapshot`. It reads state;
// it never asks the engine anything, and there is nothing here the
// engine could be made to wait on.

/// Contact colours. Five, because that is the contact count the
/// descriptors in this project report; a sixth finger wraps around
/// rather than going invisible.
const CONTACT_COLORS: [(f64, f64, f64); 5] = [
    (0.38, 0.72, 1.00), // blue
    (1.00, 0.62, 0.28), // orange
    (0.46, 0.86, 0.52), // green
    (0.93, 0.48, 0.72), // pink
    (0.78, 0.70, 1.00), // violet
];

fn rgb(r: f64, g: f64, b: f64) -> Retained<NSColor> {
    NSColor::colorWithCalibratedRed_green_blue_alpha(r, g, b, 1.0)
}

fn rgba(r: f64, g: f64, b: f64, a: f64) -> Retained<NSColor> {
    NSColor::colorWithCalibratedRed_green_blue_alpha(r, g, b, a)
}

fn ink() -> Retained<NSColor> {
    rgb(0.86, 0.88, 0.93)
}

fn dim() -> Retained<NSColor> {
    rgb(0.52, 0.56, 0.64)
}

fn good() -> Retained<NSColor> {
    rgb(0.44, 0.85, 0.52)
}

fn warn() -> Retained<NSColor> {
    rgb(1.00, 0.72, 0.30)
}

fn bad() -> Retained<NSColor> {
    rgb(0.94, 0.44, 0.44)
}

fn attrs(font: &NSFont, color: &NSColor) -> Retained<NSDictionary<NSString, AnyObject>> {
    NSDictionary::from_slices(
        &[unsafe { NSFontAttributeName }, unsafe {
            NSForegroundColorAttributeName
        }],
        &[font as &AnyObject, color as &AnyObject],
    )
}

fn text(s: &str, x: f64, y: f64, font: &NSFont, color: &NSColor) -> f64 {
    let a = attrs(font, color);
    let ns = NSString::from_str(s);
    let size = unsafe { ns.sizeWithAttributes(Some(&a)) };
    unsafe { ns.drawAtPoint_withAttributes(NSPoint::new(x, y), Some(&a)) };
    size.width
}

fn fill(rect: NSRect, color: &NSColor) {
    color.setFill();
    NSBezierPath::fillRect(rect);
}

fn stroke(rect: NSRect, color: &NSColor, width: f64) {
    let path = NSBezierPath::bezierPathWithRect(rect);
    path.setLineWidth(width);
    color.setStroke();
    path.stroke();
}

fn line(from: NSPoint, to: NSPoint, color: &NSColor, width: f64) {
    let path = NSBezierPath::bezierPath();
    path.moveToPoint(from);
    path.lineToPoint(to);
    path.setLineWidth(width);
    color.setStroke();
    path.stroke();
}

fn disc(center: NSPoint, radius: f64, color: &NSColor) {
    let rect = NSRect::new(
        NSPoint::new(center.x - radius, center.y - radius),
        NSSize::new(radius * 2.0, radius * 2.0),
    );
    color.setFill();
    NSBezierPath::bezierPathWithOvalInRect(rect).fill();
}

fn ring(center: NSPoint, radius: f64, color: &NSColor, width: f64) {
    let rect = NSRect::new(
        NSPoint::new(center.x - radius, center.y - radius),
        NSSize::new(radius * 2.0, radius * 2.0),
    );
    let path = NSBezierPath::bezierPathWithOvalInRect(rect);
    path.setLineWidth(width);
    color.setStroke();
    path.stroke();
}

fn contact_color(index: usize) -> Retained<NSColor> {
    let (r, g, b) = CONTACT_COLORS[index % CONTACT_COLORS.len()];
    rgb(r, g, b)
}

fn kind_name(kind: GestureKind) -> &'static str {
    match kind {
        GestureKind::Idle => "idle",
        GestureKind::OneFinger => "1F cursor",
        GestureKind::TwoFingerUnclassified => "2F deciding",
        GestureKind::TwoFingerPan => "2F scroll",
        GestureKind::TwoFingerPinchAndRotate => "2F pinch+rotate",
        GestureKind::ThreeFingerLive => "3F swipe",
        GestureKind::FourFingerLive => "4F swipe",
        GestureKind::SwipeLatched => "swipe latched",
    }
}

fn draw(bounds: NSRect) {
    fill(bounds, &rgb(0.07, 0.08, 0.10));
    SCOPE.with(|s| {
        let state = s.borrow();
        draw_scope(bounds, &state);
    });
}

fn draw_scope(bounds: NSRect, state: &ScopeState) {
    let mono = NSFont::monospacedSystemFontOfSize_weight(11.0, 0.0);
    let mono_big = NSFont::monospacedSystemFontOfSize_weight(15.0, 0.0);
    let margin = 14.0;
    let header_h = 22.0;
    let width = bounds.size.width - margin * 2.0;

    let Some(snap) = state.latest.as_ref() else {
        text(
            "waiting for frames — touch the pad",
            margin,
            margin,
            &mono,
            &dim(),
        );
        return;
    };

    // Header: what device this is and that frames are arriving.
    let pad_note = match snap.pad {
        Some(p) => format!("pad {:.1} x {:.1} mm", p.width_mm, p.height_mm),
        None => "pad size unknown — drawing a nominal surface".to_string(),
    };
    let head = format!("{}   ·   {} frames", pad_note, state.frames);
    text(&head, margin, 6.0, &mono, &dim());

    let pad_area = NSRect::new(
        NSPoint::new(margin, header_h + 4.0),
        NSSize::new(width, (bounds.size.height - header_h - margin * 2.0) * 0.46),
    );
    draw_pad(pad_area, snap, state, &mono);

    let panel_y = pad_area.origin.y + pad_area.size.height + 12.0;
    let panel = NSRect::new(
        NSPoint::new(margin, panel_y),
        NSSize::new(width, bounds.size.height - panel_y - margin),
    );
    draw_panel(panel, snap, state, &mono, &mono_big);
}

/// The pad surface, the contacts on it, and where they came from.
fn draw_pad(area: NSRect, snap: &Snapshot, state: &ScopeState, mono: &NSFont) {
    let (pad_w, pad_h) = match snap.pad {
        Some(p) => (p.width_mm.max(1.0), p.height_mm.max(1.0)),
        None => FALLBACK_PAD_MM,
    };
    // Letterbox the pad into the available area, keeping its aspect —
    // a scope that stretched the surface would put every distance on
    // screen at a different scale depending on direction.
    let scale = (area.size.width / pad_w).min(area.size.height / pad_h);
    let draw_w = pad_w * scale;
    let draw_h = pad_h * scale;
    let x0 = area.origin.x + (area.size.width - draw_w) / 2.0;
    let y0 = area.origin.y + (area.size.height - draw_h) / 2.0;
    let surface = NSRect::new(NSPoint::new(x0, y0), NSSize::new(draw_w, draw_h));
    let at = |x_mm: f64, y_mm: f64| NSPoint::new(x0 + x_mm * scale, y0 + y_mm * scale);

    fill(surface, &rgb(0.11, 0.12, 0.15));
    stroke(surface, &rgb(0.26, 0.29, 0.35), 1.0);
    // Midlines, so a centroid drift near the middle of the pad is
    // readable without measuring.
    let grid = rgba(1.0, 1.0, 1.0, 0.06);
    line(
        NSPoint::new(x0 + draw_w / 2.0, y0),
        NSPoint::new(x0 + draw_w / 2.0, y0 + draw_h),
        &grid,
        1.0,
    );
    line(
        NSPoint::new(x0, y0 + draw_h / 2.0),
        NSPoint::new(x0 + draw_w, y0 + draw_h / 2.0),
        &grid,
        1.0,
    );

    // Tracks first, so contacts sit on top of their own history.
    let track_alpha = if state.ghost { 0.28 } else { 0.62 };
    for (i, track) in state.tracks.iter().enumerate() {
        if track.points.len() < 2 {
            continue;
        }
        let (r, g, b) = CONTACT_COLORS[i % CONTACT_COLORS.len()];
        let path = NSBezierPath::bezierPath();
        let first = track.points[0];
        path.moveToPoint(at(first.0, first.1));
        for p in &track.points[1..] {
            path.lineToPoint(at(p.0, p.1));
        }
        path.setLineWidth(1.6);
        rgba(r, g, b, track_alpha).setStroke();
        path.stroke();
    }

    // Landing points: the anchor every per-finger displacement in the
    // panel below is measured from.
    for (i, c) in snap.contacts.iter().enumerate() {
        let color = contact_color(i);
        let p = at(c.down_x_mm, c.down_y_mm);
        line(
            NSPoint::new(p.x - 4.0, p.y),
            NSPoint::new(p.x + 4.0, p.y),
            &color,
            1.0,
        );
        line(
            NSPoint::new(p.x, p.y - 4.0),
            NSPoint::new(p.x, p.y + 4.0),
            &color,
            1.0,
        );
    }

    // The lever arm, whose length is the pinch signal and whose angle
    // is the rotate signal.
    if snap.contacts.len() == 2 {
        let a = at(snap.contacts[0].x_mm, snap.contacts[0].y_mm);
        let b = at(snap.contacts[1].x_mm, snap.contacts[1].y_mm);
        line(a, b, &rgba(1.0, 1.0, 1.0, 0.22), 1.0);
        let centroid = NSPoint::new((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        ring(centroid, 3.5, &rgba(1.0, 1.0, 1.0, 0.45), 1.0);
    }

    // Centroid travel, which is what the swipe axis locks on.
    if let Some(m) = snap.multi {
        let cx: f64 =
            snap.contacts.iter().map(|c| c.x_mm).sum::<f64>() / snap.contacts.len().max(1) as f64;
        let cy: f64 =
            snap.contacts.iter().map(|c| c.y_mm).sum::<f64>() / snap.contacts.len().max(1) as f64;
        let from = at(cx - m.travel_mm.0, cy - m.travel_mm.1);
        let to = at(cx, cy);
        line(from, to, &warn(), 2.0);
        ring(from, 3.0, &warn(), 1.0);
    }

    for (i, c) in snap.contacts.iter().enumerate() {
        let color = contact_color(i);
        let p = at(c.x_mm, c.y_mm);
        // The integrated button swells every contact and punches a hole
        // through it. A word in the status line is easy to read past;
        // the thing under your fingers changing shape is not, and the
        // button is exactly the state you need to notice without
        // looking away from the pad.
        let radius = if snap.button {
            CONTACT_RADIUS_PRESSED
        } else {
            CONTACT_RADIUS
        };
        // An unconfident contact is the device saying "this might be a
        // palm" — the engine treats it as a contact regardless, so the
        // scope has to show the difference.
        if c.confidence {
            disc(p, radius, &color);
        } else {
            ring(p, radius, &color, 1.5);
        }
        if snap.button {
            disc(p, radius * 0.42, &rgb(0.0, 0.0, 0.0));
        }
        let label = format!("{}", c.id);
        let _ = text(&label, p.x + radius + 3.0, p.y - 7.0, mono, &color);
    }

    if snap.pad.is_none() {
        text(
            "nominal surface — no descriptor size",
            x0 + 4.0,
            y0 + draw_h - 16.0,
            mono,
            &rgba(1.0, 0.72, 0.30, 0.7),
        );
    }
}

/// The decision: what the engine thinks is happening, and the numbers
/// it thinks it from.
fn draw_panel(area: NSRect, snap: &Snapshot, state: &ScopeState, mono: &NSFont, mono_big: &NSFont) {
    let mut y = area.origin.y;
    let x = area.origin.x;

    // Status line.
    // A held button with two or more fingers short-circuits the whole
    // classification pipeline, so `kind` is left holding whatever it
    // was before the press — a stale "2F scroll" next to "gestures
    // suppressed" reads as a contradiction. Say what is actually
    // happening instead.
    let (kind_label, kind_color) = if snap.physical_drag {
        ("physical drag", warn())
    } else {
        (
            kind_name(snap.kind),
            match snap.kind {
                GestureKind::Idle => dim(),
                GestureKind::TwoFingerUnclassified => warn(),
                _ => ink(),
            },
        )
    };
    let used = text(kind_label, x, y, mono_big, &kind_color);
    let mut sx = x + used + 16.0;
    sx += text(
        &format!("{} contacts", snap.contacts.len()),
        sx,
        y + 3.0,
        mono,
        &dim(),
    ) + 14.0;
    sx += text(
        &format!("{:.0} ms", snap.since_start.as_secs_f64() * 1000.0),
        sx,
        y + 3.0,
        mono,
        &dim(),
    ) + 14.0;
    sx += text(
        &format!("moved {:.2} mm", snap.max_move_mm),
        sx,
        y + 3.0,
        mono,
        &dim(),
    ) + 14.0;
    if snap.tap_window_open && !snap.contacts.is_empty() {
        // The commonest answer to "why has nothing happened yet".
        sx += text("tap window open", sx, y + 3.0, mono, &warn()) + 14.0;
    }
    if snap.button {
        sx += text("BUTTON", sx, y + 3.0, mono, &good()) + 14.0;
    }
    if snap.physical_drag {
        text(
            "gestures suppressed while the button is held",
            sx,
            y + 3.0,
            mono,
            &warn(),
        );
    }
    y += 26.0;

    match (snap.two_finger, snap.multi) {
        (Some(m), _) => {
            y = draw_two_finger(area, y, &m, mono);
        }
        (None, Some(m)) => {
            y = draw_multi(area, y, &m, mono);
        }
        (None, None) => {
            y = draw_contacts(area, y, snap, mono);
        }
    }

    if let Some(lock) = state.lock.as_ref() {
        draw_lock(area, y + 6.0, lock, state.ghost, mono);
    }
}

/// What there is to say when no multi-finger decision is in flight:
/// the contacts themselves. For a 1F touch this is the whole story —
/// how far it has moved and how long it has been down are exactly what
/// decide tap versus cursor.
fn draw_contacts(area: NSRect, top: f64, snap: &Snapshot, mono: &NSFont) -> f64 {
    let x = area.origin.x;
    let mut y = top;
    if snap.contacts.is_empty() {
        text("no contacts", x, y + 4.0, mono, &dim());
        return y + 24.0;
    }
    // What the acceleration curve is doing, right now, to the speed
    // your hand is actually producing. `accel_exponent` and
    // `accel_ref` shape a curve you can otherwise only feel, and
    // "effective px/mm" is the same unit as the Speed slider — so you
    // can see how far the curve has carried you from where you set it.
    if let Some(v) = snap.cursor_speed_mm_per_sec.filter(|v| *v > 1.0) {
        let t = current_tuning();
        let px_per_sec = crate::gesture::accelerate_cursor(v, snap.cursor_accel);
        text(
            &format!(
                "cursor  {:5.0} mm/s → {:6.0} px/s    effective {:5.1} px/mm   (set {:.1} px/mm at {:.0} mm/s)",
                v,
                px_per_sec,
                px_per_sec / v,
                t.cursor_sensitivity,
                t.cursor_accel_ref,
            ),
            x,
            y,
            mono,
            &good(),
        );
        y += 20.0;
    }

    for c in &snap.contacts {
        let line = format!(
            "contact {:<3} at {:6.1}, {:6.1} mm   moved {:5.2} mm   down {:4.0} ms",
            c.id,
            c.x_mm,
            c.y_mm,
            c.max_move_mm,
            c.age.as_secs_f64() * 1000.0,
        );
        let w = text(&line, x, y, mono, &ink());
        if !c.confidence {
            // The device's own doubt about this contact — usually a
            // palm. Worth seeing, because the engine acts on it anyway.
            text("unconfident", x + w + 16.0, y, mono, &warn());
        }
        y += 18.0;
    }
    y + 4.0
}

/// One normalized score, as a bar against its 1.0 lock threshold.
fn draw_two_finger(area: NSRect, top: f64, m: &TwoFingerMetrics, mono: &NSFont) -> f64 {
    let x = area.origin.x;
    let mut y = top;
    let bar_x = x + 62.0;
    let bar_w = (area.size.width - 62.0 - 230.0).max(120.0);

    let rows: [(&str, f64, f64, &str); 3] = [
        ("pan", m.pan_raw, m.pan, m.pan_tag()),
        ("pinch", m.pinch_raw, m.pinch, m.pinch_tag()),
        ("rotate", m.rot_raw, m.rot, m.rot_tag()),
    ];
    for (name, raw, gated, tag) in rows {
        let track = NSRect::new(NSPoint::new(bar_x, y + 4.0), NSSize::new(bar_w, 12.0));
        fill(track, &rgb(0.14, 0.15, 0.19));

        let frac = |v: f64| (v / BAR_FULL_SCORE).clamp(0.0, 1.0);
        // The raw score as an outline, the score the lock actually
        // selects on as a solid fill. When a gate has zeroed a signal
        // the difference is the whole story, so it must be visible.
        if raw > 0.0 {
            fill(
                NSRect::new(
                    NSPoint::new(bar_x, y + 4.0),
                    NSSize::new(bar_w * frac(raw), 12.0),
                ),
                &rgba(1.0, 1.0, 1.0, 0.16),
            );
        }
        if gated > 0.0 {
            let color = if gated >= 1.0 {
                good()
            } else {
                rgb(0.36, 0.58, 0.92)
            };
            fill(
                NSRect::new(
                    NSPoint::new(bar_x, y + 4.0),
                    NSSize::new(bar_w * frac(gated), 12.0),
                ),
                &color,
            );
        }
        // The lock threshold.
        let tick = bar_x + bar_w * (1.0 / BAR_FULL_SCORE);
        line(
            NSPoint::new(tick, y + 1.0),
            NSPoint::new(tick, y + 19.0),
            &rgba(1.0, 1.0, 1.0, 0.55),
            1.0,
        );

        text(name, x, y + 3.0, mono, &ink());
        let value = format!("{raw:.2}");
        let w = text(&value, bar_x + bar_w + 10.0, y + 3.0, mono, &ink());
        if !tag.is_empty() {
            let color = if tag.starts_with(" disq") {
                bad()
            } else {
                warn()
            };
            text(tag.trim(), bar_x + bar_w + 14.0 + w, y + 3.0, mono, &color);
        }
        y += 22.0;
    }

    y += 4.0;
    // The decomposition the gates are read off. `common` beating
    // `differential` by the margin is what makes a gesture a pan at
    // all; alignment and balance decide whether both fingers actually
    // took part.
    let gate = |ok: bool| if ok { good() } else { bad() };
    let mut cx = x;
    cx += text(
        &format!("common {:.2} mm", m.common_mm),
        cx,
        y,
        mono,
        &ink(),
    ) + 16.0;
    cx += text(
        &format!("diff {:.2} mm", m.differential_mm),
        cx,
        y,
        mono,
        &ink(),
    ) + 16.0;
    cx += text(
        &format!("margin {}", if m.margin_ok { "ok" } else { "fail" }),
        cx,
        y,
        mono,
        &gate(m.margin_ok),
    ) + 16.0;
    cx += text(
        &format!("align {:+.3}", m.alignment),
        cx,
        y,
        mono,
        &gate(m.aligned),
    ) + 16.0;
    text(
        &format!("balance {:.2}", m.balance),
        cx,
        y,
        mono,
        &gate(m.balance_ok),
    );
    y += 18.0;

    let mut cx = x;
    cx += text(
        &format!(
            "travel {:.2} / {:.2} mm",
            m.travel_mm.0.min(m.travel_mm.1),
            m.travel_mm.0.max(m.travel_mm.1)
        ),
        cx,
        y,
        mono,
        &ink(),
    ) + 16.0;
    cx += text(
        &format!(
            "span {:.1} → {:.1} mm",
            m.initial_distance_mm, m.distance_mm
        ),
        cx,
        y,
        mono,
        &ink(),
    ) + 16.0;
    text(
        &format!("angle {:+.1}°", m.angle_delta_rad.to_degrees()),
        cx,
        y,
        mono,
        &ink(),
    );
    y += 18.0;

    if m.scroll_speed_mm_per_sec > 1.0 {
        // Through `output::accelerate_scroll` itself rather than a
        // copy of the formula — the same rule as everything else here.
        let t = current_tuning();
        let px_per_sec =
            crate::output::accelerate_scroll(m.scroll_speed_mm_per_sec, t.scroll_curve());
        text(
            &format!(
                "scroll  {:5.0} mm/s → {:6.0} px/s    effective {:5.1} px/mm   (set {:.1} px/mm at {:.0} mm/s)",
                m.scroll_speed_mm_per_sec,
                px_per_sec,
                px_per_sec / m.scroll_speed_mm_per_sec,
                t.scroll_sensitivity,
                t.scroll_accel_ref,
            ),
            x,
            y,
            mono,
            &good(),
        );
        y += 18.0;
    }

    let mut cx = x;
    if !m.pinch_rot_admissible {
        cx += text(
            "pinch/rotate gated: one finger in the noise band",
            cx,
            y,
            mono,
            &warn(),
        ) + 16.0;
    }
    if !m.pinch_admitted || !m.rotate_admitted {
        cx += text(
            &format!(
                "policy: pinch {} rotate {}",
                if m.pinch_admitted { "on" } else { "off" },
                if m.rotate_admitted { "on" } else { "off" }
            ),
            cx,
            y,
            mono,
            &warn(),
        ) + 16.0;
    }
    if m.lock_deferred {
        text(
            "lock deferred a frame for a lagging finger",
            cx,
            y,
            mono,
            &warn(),
        );
    }
    y + 18.0
}

fn draw_multi(area: NSRect, top: f64, m: &MultiMetrics, mono: &NSFont) -> f64 {
    let x = area.origin.x;
    let mut y = top;

    let axis = match m.axis {
        Some(SwipeAxis::Horizontal) => "horizontal",
        Some(SwipeAxis::Vertical) => "vertical",
        None => "not locked",
    };
    let mut cx = x;
    cx += text(&format!("{} fingers", m.fingers), cx, y, mono, &ink()) + 16.0;
    let axis_color = if m.axis.is_some() { ink() } else { warn() };
    cx += text(&format!("axis {axis}"), cx, y, mono, &axis_color) + 16.0;
    text(
        &format!(
            "travel {:+.1}, {:+.1} mm (locks at {:.0})",
            m.travel_mm.0, m.travel_mm.1, m.axis_lock_mm
        ),
        cx,
        y,
        mono,
        &dim(),
    );
    y += 20.0;

    if let Some(progress) = m.progress {
        // Progress is signed, so the bar grows either side of centre —
        // which is also how the Dock reads it.
        let bar_x = x + 62.0;
        let bar_w = (area.size.width - 62.0 - 230.0).max(120.0);
        let track = NSRect::new(NSPoint::new(bar_x, y + 4.0), NSSize::new(bar_w, 12.0));
        fill(track, &rgb(0.14, 0.15, 0.19));
        let mid = bar_x + bar_w / 2.0;
        let half = (progress.clamp(-1.0, 1.0)) * bar_w / 2.0;
        let (fx, fw) = if half >= 0.0 {
            (mid, half)
        } else {
            (mid + half, -half)
        };
        fill(
            NSRect::new(NSPoint::new(fx, y + 4.0), NSSize::new(fw, 12.0)),
            &good(),
        );
        line(
            NSPoint::new(mid, y + 1.0),
            NSPoint::new(mid, y + 19.0),
            &rgba(1.0, 1.0, 1.0, 0.55),
            1.0,
        );
        text("progress", x, y + 3.0, mono, &ink());
        text(
            &format!("{progress:+.2} of {:.0} mm", m.progress_ref_mm),
            bar_x + bar_w + 10.0,
            y + 3.0,
            mono,
            &ink(),
        );
        y += 22.0;
    }

    text(
        &format!(
            "velocity {:+.0}, {:+.0} mm/s   policy: horizontal {} vertical {}",
            m.velocity_mm_per_sec.0,
            m.velocity_mm_per_sec.1,
            if m.horizontal_admitted { "on" } else { "off" },
            if m.vertical_admitted { "on" } else { "off" }
        ),
        x,
        y,
        mono,
        &dim(),
    );
    y + 20.0
}

/// The lock, frozen at the frame it fired. Same fields, same order as
/// the `2F lock=…` log line, so a screenshot and a log line can be read
/// against each other.
///
/// It deliberately outlives the gesture: you look up *after* your
/// fingers have left the pad, and a banner that cleared on lift would
/// be gone exactly when you went to read it. The tracks stay for the
/// same reason. But a record of something finished must not look like
/// something happening — `past` dims the whole box and changes the
/// verb, matching the tracks, which ghost at the same moment. Both
/// clear when the next touch lands.
fn draw_lock(area: NSRect, top: f64, lock: &LockRecord, past: bool, mono: &NSFont) {
    let m = &lock.metrics;
    let box_rect = NSRect::new(
        NSPoint::new(area.origin.x, top),
        NSSize::new(area.size.width, 44.0),
    );
    let (wash, edge) = if past { (0.02, 0.07) } else { (0.05, 0.12) };
    fill(box_rect, &rgba(1.0, 1.0, 1.0, wash));
    stroke(box_rect, &rgba(1.0, 1.0, 1.0, edge), 1.0);

    let x = area.origin.x + 10.0;
    let label = match (past, lock.kind) {
        (false, GestureKind::TwoFingerPan) => "LOCKED → scroll",
        (false, _) => "LOCKED → pinch+rotate",
        (true, GestureKind::TwoFingerPan) => "last gesture · scroll",
        (true, _) => "last gesture · pinch+rotate",
    };
    let label_color = if past { dim() } else { good() };
    let body_color = if past { dim() } else { ink() };
    let w = text(label, x, top + 6.0, mono, &label_color);
    text(
        &format!(
            "locked after {:.0} ms{}",
            lock.after.as_secs_f64() * 1000.0,
            if past { " · fingers lifted" } else { "" }
        ),
        x + w + 14.0,
        top + 6.0,
        mono,
        &dim(),
    );
    text(
        &format!(
            "scores[pan={:.2}{} pinch={:.2}{} rot={:.2}{}] common={:.2}mm diff={:.2}mm align={:.2} balance={:.2}",
            m.pan_raw,
            m.pan_tag(),
            m.pinch_raw,
            m.pinch_tag(),
            m.rot_raw,
            m.rot_tag(),
            m.common_mm,
            m.differential_mm,
            m.alignment,
            m.balance,
        ),
        x,
        top + 24.0,
        mono,
        &body_color,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gesture::PadGeometry;

    // The drawing can't be tested without a screen, but the state it
    // draws from can — and this is where a wrong answer is invisible.
    // The ghost flag in particular: if it ever inverted, the lock
    // banner would claim a finished gesture is still live, which is
    // exactly the thing it was added to stop doing.

    fn snap(kind: GestureKind, contacts: &[(u8, f64, f64)]) -> Snapshot {
        Snapshot {
            pad: Some(PadGeometry {
                width_mm: 200.0,
                height_mm: 120.0,
            }),
            kind,
            button: false,
            physical_drag: false,
            contacts: contacts
                .iter()
                .map(|&(id, x, y)| ContactTrack {
                    id,
                    x_mm: x,
                    y_mm: y,
                    down_x_mm: x,
                    down_y_mm: y,
                    max_move_mm: 0.0,
                    age: Duration::from_millis(10),
                    confidence: true,
                })
                .collect(),
            since_start: Duration::from_millis(120),
            max_move_mm: 2.0,
            tap_window_open: false,
            two_finger: None,
            multi: None,
            cursor_accel: crate::gesture::CursorAccel::default(),
            cursor_speed_mm_per_sec: None,
        }
    }

    fn metrics() -> TwoFingerMetrics {
        TwoFingerMetrics {
            distance_mm: 24.0,
            initial_distance_mm: 20.0,
            angle_rad: 0.1,
            angle_delta_rad: 0.02,
            common_mm: 0.2,
            differential_mm: 2.0,
            alignment: -0.9,
            balance: 0.8,
            travel_mm: (2.0, 2.1),
            pan_raw: 0.5,
            pinch_raw: 5.0,
            rot_raw: 0.3,
            pan: 0.0,
            pinch: 5.0,
            rot: 0.3,
            margin_ok: false,
            balance_ok: true,
            aligned: false,
            pan_qualified: false,
            pinch_rot_admissible: true,
            pinch_admitted: true,
            rotate_admitted: true,
            lock_deferred: false,
            scroll_speed_mm_per_sec: 0.0,
        }
    }

    #[test]
    fn a_track_follows_its_contact() {
        let mut st = ScopeState::default();
        st.ingest(&snap(GestureKind::OneFinger, &[(3, 10.0, 10.0)]));
        st.ingest(&snap(GestureKind::OneFinger, &[(3, 12.0, 10.0)]));
        st.ingest(&snap(GestureKind::OneFinger, &[(3, 14.0, 10.0)]));
        assert_eq!(st.tracks.len(), 1);
        assert_eq!(st.tracks[0].id, 3);
        assert_eq!(st.tracks[0].points.len(), 3);
    }

    #[test]
    fn a_resting_contact_does_not_fill_its_track() {
        let mut st = ScopeState::default();
        st.ingest(&snap(GestureKind::OneFinger, &[(0, 10.0, 10.0)]));
        for i in 0..50 {
            // Well under TRACK_MIN_STEP_MM: chip noise, not motion.
            let jitter = if i % 2 == 0 { 0.01 } else { -0.01 };
            st.ingest(&snap(GestureKind::OneFinger, &[(0, 10.0 + jitter, 10.0)]));
        }
        assert_eq!(st.tracks[0].points.len(), 1, "noise is not a path");
    }

    #[test]
    fn a_track_is_capped_rather_than_growing_without_bound() {
        let mut st = ScopeState::default();
        for i in 0..(TRACK_POINTS_MAX + 200) {
            let x = 10.0 + i as f64 * 0.1;
            st.ingest(&snap(GestureKind::OneFinger, &[(0, x, 10.0)]));
        }
        assert_eq!(st.tracks[0].points.len(), TRACK_POINTS_MAX);
        // The cap drops the oldest, so the live end of the path is
        // always what stays on screen.
        let last = *st.tracks[0].points.last().unwrap();
        assert!(last.0 > 80.0, "{last:?}");
    }

    #[test]
    fn lifting_ghosts_the_tracks_and_the_next_touch_clears_them() {
        let mut st = ScopeState::default();
        st.ingest(&snap(GestureKind::OneFinger, &[(0, 10.0, 10.0)]));
        st.ingest(&snap(GestureKind::OneFinger, &[(0, 20.0, 10.0)]));
        assert!(!st.ghost, "fingers are still down");

        st.ingest(&snap(GestureKind::Idle, &[]));
        assert!(st.ghost, "the gesture is over but still worth reading");
        assert_eq!(st.tracks.len(), 1, "a lift must not erase what happened");

        st.ingest(&snap(GestureKind::OneFinger, &[(1, 50.0, 50.0)]));
        assert!(!st.ghost);
        assert_eq!(st.tracks.len(), 1, "the new touch starts a clean slate");
        assert_eq!(st.tracks[0].id, 1);
    }

    #[test]
    fn idle_frames_before_any_touch_do_not_ghost() {
        let mut st = ScopeState::default();
        st.ingest(&snap(GestureKind::Idle, &[]));
        st.ingest(&snap(GestureKind::Idle, &[]));
        assert!(!st.ghost, "nothing has happened yet to be in the past");
    }

    #[test]
    fn a_lock_is_latched_with_the_scores_of_the_frame_it_fired() {
        let mut st = ScopeState::default();
        st.ingest(&snap(
            GestureKind::TwoFingerUnclassified,
            &[(0, 10.0, 10.0), (1, 30.0, 10.0)],
        ));
        assert!(st.lock.is_none(), "nothing has locked yet");

        let mut locking = snap(
            GestureKind::TwoFingerPinchAndRotate,
            &[(0, 8.0, 10.0), (1, 32.0, 10.0)],
        );
        locking.two_finger = Some(metrics());
        st.ingest(&locking);

        let lock = st.lock.as_ref().expect("the lock is latched");
        assert_eq!(lock.kind, GestureKind::TwoFingerPinchAndRotate);
        assert_eq!(lock.metrics.pinch_raw, 5.0);
        assert_eq!(lock.after, Duration::from_millis(120));

        // Scores keep climbing after the lock; the banner must keep
        // showing what actually crossed, not the latest reading.
        let mut later = snap(
            GestureKind::TwoFingerPinchAndRotate,
            &[(0, 2.0, 10.0), (1, 38.0, 10.0)],
        );
        let mut grown = metrics();
        grown.pinch_raw = 40.0;
        later.two_finger = Some(grown);
        st.ingest(&later);
        assert_eq!(st.lock.as_ref().unwrap().metrics.pinch_raw, 5.0);
    }

    #[test]
    fn a_lock_resumed_after_a_partial_lift_is_latched_too() {
        // The engine can re-enter a locked kind straight from OneFinger
        // when a dropped contact comes back inside the rejoin window.
        // That is a lock the scope has to show, and it arrives by a
        // different transition than a fresh one.
        let mut st = ScopeState::default();
        st.ingest(&snap(GestureKind::OneFinger, &[(0, 10.0, 10.0)]));
        let mut rejoined = snap(
            GestureKind::TwoFingerPan,
            &[(0, 10.0, 12.0), (1, 30.0, 12.0)],
        );
        rejoined.two_finger = Some(metrics());
        st.ingest(&rejoined);

        let lock = st.lock.as_ref().expect("a resumed lock still latches");
        assert_eq!(lock.kind, GestureKind::TwoFingerPan);
    }

    #[test]
    fn a_new_gesture_clears_the_previous_lock() {
        let mut st = ScopeState::default();
        let mut locking = snap(
            GestureKind::TwoFingerPan,
            &[(0, 10.0, 10.0), (1, 30.0, 10.0)],
        );
        locking.two_finger = Some(metrics());
        st.ingest(&locking);
        assert!(st.lock.is_some());

        st.ingest(&snap(GestureKind::Idle, &[]));
        assert!(st.lock.is_some(), "still readable after the fingers lift");

        st.ingest(&snap(GestureKind::OneFinger, &[(2, 60.0, 60.0)]));
        assert!(st.lock.is_none(), "a new gesture is not the old one");
    }

    #[test]
    fn the_fallback_tuning_is_the_documented_defaults() {
        // The canvas draws a curve before the config file has been
        // read; if this drifted from `Config::default()` it would draw
        // a curve nobody is running.
        let t = Tuning::fallback();
        let cfg = crate::config::Config::default();
        assert_eq!(t.cursor_sensitivity, cfg.cursor.sensitivity);
        assert_eq!(t.cursor_accel_exponent, cfg.cursor.accel_exponent);
        assert_eq!(t.cursor_accel_ref, cfg.cursor.accel_ref);
        assert_eq!(t.scroll_sensitivity, cfg.scroll.sensitivity);
        assert_eq!(t.scroll_accel_exponent, cfg.scroll.accel_exponent);
        assert_eq!(t.scroll_accel_ref, cfg.scroll.accel_ref);
    }

    #[test]
    fn the_scroll_curve_crosses_linear_at_its_reference() {
        // The property the readout claims: at `accel_ref`, effective
        // px/mm equals the sensitivity slider's value.
        let t = Tuning::fallback();
        let v = t.scroll_accel_ref;
        let px_per_sec = crate::output::accelerate_scroll(v, t.scroll_curve());
        assert!(
            (px_per_sec / v - t.scroll_sensitivity).abs() < 1e-9,
            "{} vs {}",
            px_per_sec / v,
            t.scroll_sensitivity
        );
    }
}
