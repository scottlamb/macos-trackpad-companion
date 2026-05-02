//! Read-only event tap that dumps every gesture event the WindowServer
//! routes through `kCGSessionEventTap`. Pass-through (returns the event
//! unchanged) so real trackpad gestures keep working while you record.
//!
//! Usage: `cargo run --bin gesture-tap`, then perform swipes on the
//! trackpad you want to characterize. Each line shows the
//! kCGSEventTypeField (55), kIOHIDEventType (110), every nonzero
//! integer field 100–200, and every nonzero double field 100–200.
//! That's enough to identify (a) which IOHID type Mission Control /
//! App Exposé actually use on this macOS build and (b) what velocity /
//! progress range the real driver emits.
//!
//! Listens on `kCGSessionEventTap` for `kCGSEventGesture` (29) and
//! `kCGSEventDockControl` (30). Field names follow Apple's WebKit
//! test SPI header `CoreGraphicsTestSPI.h` (BSD-2-Clause).

#![allow(non_upper_case_globals)]

use core_foundation::base::TCFType;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use core_foundation_sys::base::CFRelease;
use core_foundation_sys::mach_port::CFMachPortCreateRunLoopSource;
use core_foundation_sys::runloop::CFRunLoopAddSource;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};

type CGEventRef = *mut c_void;
type CFMachPortRef = *mut c_void;
type CGEventTapProxy = *mut c_void;
type CGEventTapCallBack = extern "C" fn(
    proxy: CGEventTapProxy,
    ty: u32,
    event: CGEventRef,
    user_info: *mut c_void,
) -> CGEventRef;

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGEventTapCreate(
        tap: u32,
        place: u32,
        options: u32,
        events_of_interest: u64,
        callback: CGEventTapCallBack,
        user_info: *mut c_void,
    ) -> CFMachPortRef;
    fn CGEventTapEnable(tap: CFMachPortRef, enable: bool);
    fn CGEventGetIntegerValueField(event: CGEventRef, field: u32) -> i64;
    fn CGEventGetDoubleValueField(event: CGEventRef, field: u32) -> f64;
}

const kCGSessionEventTap: u32 = 1;
const kCGHeadInsertEventTap: u32 = 0;
const kCGEventTapOptionListenOnly: u32 = 1;

// CGSEventType values + CGEventField indices from
// `WebKit/Tools/TestRunnerShared/spi/CoreGraphicsTestSPI.h`.
const kCGSEventGesture: i64 = 29;
const kCGSEventDockControl: i64 = 30;
const kCGSEventTypeField: u32 = 55;
const kCGEventGestureHIDType: u32 = 110;

static RUNNING: AtomicBool = AtomicBool::new(true);

extern "C" fn handle_signal(_: i32) {
    RUNNING.store(false, Ordering::SeqCst);
}

extern "C" fn callback(
    _proxy: CGEventTapProxy,
    ty: u32,
    event: CGEventRef,
    _info: *mut c_void,
) -> CGEventRef {
    // Pass through tap-disabled notifications (timeout / user input)
    // unmodified so the system can re-enable us.
    if ty == 0xFFFFFFFE || ty == 0xFFFFFFFF {
        return event;
    }
    let cgs_type = unsafe { CGEventGetIntegerValueField(event, kCGSEventTypeField) };
    if cgs_type != kCGSEventGesture && cgs_type != kCGSEventDockControl {
        return event;
    }
    let hid_type = unsafe { CGEventGetIntegerValueField(event, kCGEventGestureHIDType) };
    let label = if cgs_type == kCGSEventGesture {
        "Gesture"
    } else {
        "DockCtl"
    };
    let mut int_fields = String::new();
    let mut dbl_fields = String::new();
    for f in 100u32..=200 {
        if f == kCGEventGestureHIDType {
            continue;
        }
        let i = unsafe { CGEventGetIntegerValueField(event, f) };
        if i != 0 {
            int_fields.push_str(&format!(" {f}={i}"));
        }
        let d = unsafe { CGEventGetDoubleValueField(event, f) };
        // CGEventGetDoubleValueField returns the same value for
        // integer-typed fields, which would duplicate every entry.
        // Only print doubles where the value isn't representable as
        // the same integer (i.e. has a real fractional part or is
        // outside i64-safe range).
        if d != 0.0 && d != i as f64 && (d.fract() != 0.0 || d.abs() > 1e15) {
            dbl_fields.push_str(&format!(" {f}={d:+.4}"));
        }
    }
    println!("{label} hid={hid_type}{int_fields}{dbl_fields}");
    event
}

fn main() -> anyhow::Result<()> {
    unsafe {
        libc::signal(libc::SIGINT, handle_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handle_signal as *const () as libc::sighandler_t);
    }

    let mask = (1u64 << kCGSEventGesture) | (1u64 << kCGSEventDockControl);
    let tap = unsafe {
        CGEventTapCreate(
            kCGSessionEventTap,
            kCGHeadInsertEventTap,
            kCGEventTapOptionListenOnly,
            mask,
            callback,
            std::ptr::null_mut(),
        )
    };
    if tap.is_null() {
        anyhow::bail!(
            "CGEventTapCreate returned NULL — grant Accessibility permission \
             to the terminal/binary in System Settings → Privacy & Security."
        );
    }

    let src = unsafe { CFMachPortCreateRunLoopSource(std::ptr::null(), tap as *mut _, 0) };
    unsafe {
        CFRunLoopAddSource(
            CFRunLoop::get_current().as_concrete_TypeRef() as *mut _,
            src,
            kCFRunLoopCommonModes,
        );
        CGEventTapEnable(tap, true);
    }

    eprintln!(
        "gesture-tap: listening on kCGSessionEventTap. Perform swipes on your trackpad. Ctrl-C to stop."
    );
    while RUNNING.load(Ordering::SeqCst) {
        // kCFRunLoopCommonModes is a meta-mode for source registration;
        // CFRunLoopRunInMode rejects it. Use kCFRunLoopDefaultMode here
        // — sources added via CommonModes (above) will fire in it.
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            std::time::Duration::from_secs(1),
            false,
        );
    }

    unsafe {
        CGEventTapEnable(tap, false);
        CFRelease(src as *const c_void);
        CFRelease(tap as *const c_void);
    }
    Ok(())
}
