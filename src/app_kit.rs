//! Shared AppKit bring-up and main-loop control.
//!
//! Two features need `NSApp`: the gesture overlay ([`crate::overlay`])
//! and the menu-bar status item ([`crate::status_item`]). Both used to
//! be optional, so whichever ran first owned the application object —
//! which breaks as soon as both are enabled and each sets its own
//! activation policy. This module owns that setup instead, and both
//! call in.
//!
//! The loop itself is `[NSApp run]`, not `CFRunLoopRun()`. A status
//! item needs `sendEvent:` dispatch to respond to clicks, and only
//! `NSApplication`'s loop does that — a bare CFRunLoop renders the icon
//! but swallows every click. IOHID sources scheduled on the main run
//! loop still fire, since `[NSApp run]` pumps that same run loop.

use std::cell::RefCell;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2::runtime::{NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
    NSWindow,
};
use objc2_foundation::{NSActivityOptions, NSPoint, NSProcessInfo, NSString};

unsafe extern "C" {
    /// libdispatch's main queue. Declared by hand rather than pulling in
    /// a dispatch crate for one symbol.
    static _dispatch_main_q: c_void;

    fn dispatch_async_f(queue: *mut c_void, context: *mut c_void, work: extern "C" fn(*mut c_void));
}

/// `finishLaunching` posts `NSApplicationDidFinishLaunching`; calling it
/// twice posts it twice. The rest of the setup is idempotent, so only
/// this needs guarding.
static LAUNCHED: AtomicBool = AtomicBool::new(false);

/// Bring up `NSApp` as an accessory app — no Dock tile, no app-switcher
/// entry, no focus stealing. Safe to call from every feature that needs
/// AppKit; the first call wins and later ones are cheap no-ops.
///
/// Must run on the main thread; the `MainThreadMarker` enforces it.
pub fn ensure_app(mtm: MainThreadMarker) -> Retained<NSApplication> {
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    if !LAUNCHED.swap(true, Ordering::SeqCst) {
        app.finishLaunching();
    }
    app
}

thread_local! {
    /// Windows this app owns. Registered so that closing one doesn't
    /// drop the app back to accessory while another is still open.
    static WINDOWS: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
}

/// Track a window for activation-policy purposes. Call once per window.
pub fn register_window(window: Retained<NSWindow>) {
    WINDOWS.with(|w| w.borrow_mut().push(window));
}

/// Become a regular app so a window can take focus properly. An
/// accessory app can show a window but can't key it reliably.
pub fn activate_for_window(mtm: MainThreadMarker) {
    let app = ensure_app(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);
    app.activate();
}

/// Drop back to a menu-bar-only agent, but only once every registered
/// window is closed.
pub fn settle_activation(mtm: MainThreadMarker) {
    let any_visible = WINDOWS.with(|w| w.borrow().iter().any(|win| win.isVisible()));
    if !any_visible {
        ensure_app(mtm).setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    }
}

thread_local! {
    /// Held for the life of the process; dropping it ends the activity.
    static ACTIVITY: RefCell<Option<Retained<ProtocolObject<dyn NSObjectProtocol>>>> =
        const { RefCell::new(None) };
}

/// Opt out of App Nap.
///
/// A windowless agent is exactly what App Nap targets, and it coalesces
/// timers hard: a 3-second retry interval was measured firing at ~9
/// seconds. That matters here because timers are how the companion
/// recovers — retrying a failed HID open, noticing a config change.
///
/// `UserInitiatedAllowingIdleSystemSleep` rather than `UserInitiated`:
/// the work is user-initiated, but a trackpad daemon has no business
/// keeping the machine awake.
pub fn disable_app_nap() {
    let info = NSProcessInfo::processInfo();
    let token = info.beginActivityWithOptions_reason(
        NSActivityOptions::UserInitiatedAllowingIdleSystemSleep,
        &NSString::from_str("driving a trackpad; timers must fire on schedule"),
    );
    ACTIVITY.with(|a| *a.borrow_mut() = Some(token));
    log::debug!("App Nap disabled for this process");
}

/// Run the AppKit event loop. Blocks until [`request_stop`] fires.
///
/// Replaces the `CFRunLoopRun()` that `hid::Manager::run` used before
/// there was any UI.
pub fn run_event_loop(mtm: MainThreadMarker) {
    let app = ensure_app(mtm);
    app.run();
}

/// Ask the event loop to exit. Callable from any thread — the sigwait
/// worker calls it from its own thread.
///
/// `[NSApp stop:]` only takes effect when the loop next finishes
/// dispatching an event, so a stop with no events pending would hang
/// until the user happened to move the mouse. The dummy
/// `ApplicationDefined` event is the standard way to guarantee one more
/// turn of the loop.
pub fn request_stop() {
    unsafe {
        let queue = &raw const _dispatch_main_q as *mut c_void;
        dispatch_async_f(queue, std::ptr::null_mut(), stop_on_main);
    }
}

extern "C" fn stop_on_main(_ctx: *mut c_void) {
    // dispatch_async_f onto the main queue always lands on the main
    // thread, so this marker is sound.
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let app = NSApplication::sharedApplication(mtm);
    app.stop(None);

    let event =
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        );
    if let Some(event) = event {
        app.postEvent_atStart(&event, true);
    }
}
