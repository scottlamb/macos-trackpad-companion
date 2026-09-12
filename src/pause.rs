//! Suspend gesture synthesis without stopping the daemon.
//!
//! Pausing is gentler than quitting: the device stays acquired, so it
//! doesn't go dormant the way it does when nothing is driving it, and
//! resuming is instant. It is the right answer to "stop doing that for
//! a minute" — quitting is not.
//!
//! Pausing cancels the engine's active input without recognizing a
//! finger lift, so it cannot produce a tap, swipe commit or new inertia.
//! Simply dropping frames would leave the engine believing fingers are
//! still down, and macOS believing a scroll never finished.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};

static PAUSED: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// Registered by `main`, which owns the engine. Runs on the main
    /// thread, where both the menu action and the HID callbacks live.
    static SETTLE: RefCell<Option<Box<dyn Fn()>>> = const { RefCell::new(None) };
}

pub fn is_paused() -> bool {
    PAUSED.load(Ordering::Relaxed)
}

/// Register the "end any gesture in flight" action.
pub fn set_settle_hook(f: impl Fn() + 'static) {
    SETTLE.with(|s| *s.borrow_mut() = Some(Box::new(f)));
}

/// Flip the paused state, returning the new value.
pub fn toggle() -> bool {
    let now = !is_paused();
    PAUSED.store(now, Ordering::Relaxed);
    if now {
        SETTLE.with(|s| {
            if let Some(f) = s.borrow().as_ref() {
                f();
            }
        });
    }
    log::info!(
        "gesture synthesis {}",
        if now { "paused" } else { "resumed" }
    );
    now
}
