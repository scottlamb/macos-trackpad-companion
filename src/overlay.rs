//! Optional on-screen HUD that flashes the recognized gesture as the
//! engine locks into it. Drives a borderless click-through `NSPanel`
//! on the same main thread that runs `CFRunLoopRun`, so AppKit and
//! IOHID share the existing run loop.
//!
//! Only the `Phase::Began` of each stream causes a flash — that's the
//! lock-in moment the user wants visibility into. The panel stays
//! visible for `duration_ms` then hides itself via a one-shot
//! `CFRunLoopTimer`.

use std::cell::RefCell;
use std::time::Duration;

use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};
use core_foundation_sys::runloop::CFRunLoopTimerRef;
use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSColor, NSFont,
    NSPanel, NSScreen, NSTextAlignment, NSTextField, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use std::ffi::c_void;

const PANEL_WIDTH: f64 = 320.0;
const PANEL_HEIGHT: f64 = 72.0;
/// Distance from the top edge of the main screen to the top of the panel.
const TOP_INSET: f64 = 64.0;
/// Apple's `NSFloatingWindowLevel` constant. objc2-app-kit's
/// `NSWindowLevel` is a typed alias for this NSInteger; keeping the
/// magic number local avoids fishing for the exact reexport name.
const NS_FLOATING_WINDOW_LEVEL: isize = 3;

pub struct Overlay {
    panel: Retained<NSPanel>,
    name_label: Retained<NSTextField>,
    seq_label: Retained<NSTextField>,
    duration: Duration,
    /// Pending hide timer. Replaced on each `flash()` so a rapid second
    /// flash extends the visible window rather than letting the first
    /// timer cut it short.
    hide_timer: RefCell<Option<CFRunLoopTimer>>,
    /// Self-pointer the timer callback uses to find us. Stored as a raw
    /// usize so it doesn't trip aliasing checks; the `Box<Overlay>` is
    /// owned by `main` for the duration of the daemon, so the pointer
    /// is valid for as long as the timer can fire.
    self_addr: usize,
}

impl Overlay {
    /// Build the panel. Caller must be on the main thread (the
    /// `MainThreadMarker` enforces this) — AppKit constructors panic
    /// otherwise.
    pub fn new(duration_ms: u32) -> Box<Self> {
        let mtm = MainThreadMarker::new()
            .expect("Overlay::new must run on the main thread");

        // Bring up NSApp as an accessory so the daemon doesn't grow a
        // Dock icon or steal focus. Idempotent if NSApp already exists.
        let app = NSApplication::sharedApplication(mtm);
        app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
        app.finishLaunching();

        let screen_frame = NSScreen::mainScreen(mtm)
            .map(|s| s.frame())
            .unwrap_or(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(1440.0, 900.0),
            ));
        let origin_x =
            screen_frame.origin.x + (screen_frame.size.width - PANEL_WIDTH) / 2.0;
        // AppKit Y grows upward from the bottom of the screen.
        let origin_y =
            screen_frame.origin.y + screen_frame.size.height - TOP_INSET - PANEL_HEIGHT;
        let rect = NSRect::new(
            NSPoint::new(origin_x, origin_y),
            NSSize::new(PANEL_WIDTH, PANEL_HEIGHT),
        );

        let style = NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel;
        let alloc = mtm.alloc::<NSPanel>();
        let panel: Retained<NSPanel> = NSPanel::initWithContentRect_styleMask_backing_defer(
            alloc,
            rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        );

        panel.setOpaque(false);
        let bg = NSColor::colorWithCalibratedRed_green_blue_alpha(0.0, 0.0, 0.0, 0.72);
        panel.setBackgroundColor(Some(&bg));
        panel.setIgnoresMouseEvents(true);
        panel.setLevel(NS_FLOATING_WINDOW_LEVEL);
        panel.setHasShadow(false);
        panel.setHidesOnDeactivate(false);

        // AppKit Y grows upward inside contentView, so the bottom label
        // gets the lower y-origin and the top label sits above it.
        let placeholder = NSString::from_str("");
        let name_label = NSTextField::labelWithString(&placeholder, mtm);
        name_label.setFrame(NSRect::new(
            NSPoint::new(0.0, 28.0),
            NSSize::new(PANEL_WIDTH, 38.0),
        ));
        name_label.setBezeled(false);
        name_label.setDrawsBackground(false);
        name_label.setEditable(false);
        name_label.setSelectable(false);
        name_label.setAlignment(NSTextAlignment::Center);
        let white = NSColor::whiteColor();
        name_label.setTextColor(Some(&white));
        let name_font = NSFont::boldSystemFontOfSize(26.0);
        name_label.setFont(Some(&name_font));

        let seq_label = NSTextField::labelWithString(&placeholder, mtm);
        seq_label.setFrame(NSRect::new(
            NSPoint::new(0.0, 4.0),
            NSSize::new(PANEL_WIDTH, 22.0),
        ));
        seq_label.setBezeled(false);
        seq_label.setDrawsBackground(false);
        seq_label.setEditable(false);
        seq_label.setSelectable(false);
        seq_label.setAlignment(NSTextAlignment::Center);
        let dim = NSColor::colorWithCalibratedRed_green_blue_alpha(1.0, 1.0, 1.0, 0.6);
        seq_label.setTextColor(Some(&dim));
        let seq_font = NSFont::monospacedSystemFontOfSize_weight(14.0, 0.0);
        seq_label.setFont(Some(&seq_font));

        let content = panel
            .contentView()
            .expect("NSPanel auto-creates a contentView");
        content.addSubview(&name_label);
        content.addSubview(&seq_label);

        let me = Box::new(Self {
            panel,
            name_label,
            seq_label,
            duration: Duration::from_millis(duration_ms.max(50) as u64),
            hide_timer: RefCell::new(None),
            self_addr: 0,
        });
        let raw = Box::into_raw(me);
        unsafe {
            (*raw).self_addr = raw as usize;
            Box::from_raw(raw)
        }
    }

    /// Show a gesture badge and (re)start the auto-hide timer. `name`
    /// goes on the top line, `#seq` on the bottom — the seq matches the
    /// `overlay #N: …` log line emitted by the caller.
    pub fn flash(&self, name: &str, seq: u64) {
        self.name_label
            .setStringValue(&NSString::from_str(name));
        self.seq_label
            .setStringValue(&NSString::from_str(&format!("#{seq}")));
        self.panel.orderFrontRegardless();

        // Cancel the previous hide-timer (if any) and install a fresh one.
        if let Some(prev) = self.hide_timer.borrow_mut().take() {
            unsafe {
                core_foundation_sys::runloop::CFRunLoopTimerInvalidate(
                    prev.as_concrete_TypeRef(),
                );
            }
        }

        let fire_at = unsafe { CFAbsoluteTimeGetCurrent() } + self.duration.as_secs_f64();
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: self.self_addr as *mut c_void,
            retain: None,
            release: None,
            copyDescription: None,
        };
        let timer = CFRunLoopTimer::new(fire_at, 0.0, 0, 0, hide_callback, &mut ctx);
        let mode = unsafe { kCFRunLoopCommonModes };
        CFRunLoop::get_current().add_timer(&timer, mode);
        *self.hide_timer.borrow_mut() = Some(timer);
    }

    fn hide(&self) {
        self.panel.orderOut(None);
        self.hide_timer.borrow_mut().take();
    }
}

extern "C" fn hide_callback(_timer: CFRunLoopTimerRef, info: *mut c_void) {
    if info.is_null() {
        return;
    }
    let overlay = unsafe { &*(info as *const Overlay) };
    overlay.hide();
}
