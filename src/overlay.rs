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

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSFont, NSPanel, NSScreen, NSTextAlignment, NSTextField,
    NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::run_loop_timer::Timer;

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
    hide_timer: RefCell<Option<Timer>>,
}

impl Overlay {
    /// Build the panel. Caller must be on the main thread (the
    /// `MainThreadMarker` enforces this) — AppKit constructors panic
    /// otherwise.
    pub fn new(duration_ms: u32) -> Box<Self> {
        let mtm = MainThreadMarker::new().expect("Overlay::new must run on the main thread");

        // Shared with the menu-bar status item — whichever feature is
        // enabled first brings NSApp up, and neither overrides the
        // other's activation policy.
        crate::app_kit::ensure_app(mtm);

        let screen_frame = NSScreen::mainScreen(mtm)
            .map(|s| s.frame())
            .unwrap_or(NSRect::new(
                NSPoint::new(0.0, 0.0),
                NSSize::new(1440.0, 900.0),
            ));
        let origin_x = screen_frame.origin.x + (screen_frame.size.width - PANEL_WIDTH) / 2.0;
        // AppKit Y grows upward from the bottom of the screen.
        let origin_y = screen_frame.origin.y + screen_frame.size.height - TOP_INSET - PANEL_HEIGHT;
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

        unsafe { panel.setReleasedWhenClosed(false) };
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

        Box::new(Self {
            panel,
            name_label,
            seq_label,
            duration: Duration::from_millis(duration_ms.max(50) as u64),
            hide_timer: RefCell::new(None),
        })
    }

    /// Show a gesture badge and (re)start the auto-hide timer. `name`
    /// goes on the top line, `#seq` on the bottom — the seq matches the
    /// `overlay #N: …` log line emitted by the caller.
    pub fn flash(&self, name: &str, seq: u64) {
        self.name_label.setStringValue(&NSString::from_str(name));
        self.seq_label
            .setStringValue(&NSString::from_str(&format!("#{seq}")));
        self.panel.orderFrontRegardless();

        // Capturing the panel avoids a raw pointer into this movable
        // owner. Dropping/replacing the guard cancels the pending hide.
        self.hide_timer.borrow_mut().take();
        let panel = self.panel.clone();
        let timer = Timer::new(self.duration.as_secs_f64(), 0.0, move || {
            panel.orderOut(None);
        });
        *self.hide_timer.borrow_mut() = Some(timer);
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.hide_timer.get_mut().take();
        self.panel.orderOut(None);
    }
}
