//! Read-only event tap that dumps every gesture-related event the
//! WindowServer routes through `kCGSessionEventTap`. Pass-through
//! (returns the event unchanged) so real trackpad gestures keep
//! working while you record.
//!
//! Usage: `cargo run --bin gesture-tap`, then perform swipes / scrolls
//! on the trackpad you want to characterize. Each line shows the event
//! kind, top-level CGEvent timestamp, a fresh `Timestamp::now()` sample
//! at the callback, the millisecond delta between them, the
//! kIOHIDEventType (110) where applicable, and every nonzero integer
//! and double field in 100–200.
//!
//! Listens for `kCGSEventGesture` (CGS subtype 29 — pinch / rotate
//! synthesized via `output::synthesize_gesture_event`),
//! `kCGSEventDockControl` (subtype 30 — DockSwipes from
//! `output::post_dock_swipe`), and `kCGEventScrollWheel` (CGEventType
//! 22, NB: identified by the wrapper-level event type, not the CGS
//! subtype field). Field names follow Apple's WebKit test SPI header
//! `CoreGraphicsTestSPI.h` (BSD-2-Clause).

#![allow(non_upper_case_globals)]

use core_foundation::base::TCFType;
use core_foundation::runloop::{CFRunLoop, kCFRunLoopCommonModes, kCFRunLoopDefaultMode};
use core_foundation_sys::base::CFRelease;
use core_foundation_sys::mach_port::CFMachPortCreateRunLoopSource;
use core_foundation_sys::runloop::CFRunLoopAddSource;
use macos_trackpad_companion::time::Timestamp;
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
    unsafe fn CGEventGetTimestamp(event: CGEventRef) -> u64;
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

/// `CGEventType::kCGEventScrollWheel`. The tap callback's `ty` parameter
/// is the CGEventType wrapper-level type (not the CGS subtype in field
/// 55), so scroll events are matched on `ty == 22`.
const kCGEventScrollWheel: u32 = 22;

/// CGEventField name lookup. Fields without an entry here print with
/// just their numeric index; named fields get a label prefix. Names
/// taken from the public CoreGraphics SDK header `CGEventTypes.h` (via
/// the phracker/MacOSX-SDKs mirror on github), WebKit's
/// `CoreGraphicsTestSPI.h`, and the constants already in `output.rs`.
/// CGS gesture-private fields outside these ranges have no public
/// name; rather than guess we leave them as numeric.
fn field_name(idx: u32) -> Option<&'static str> {
    match idx {
        // Common — mouse / cursor.
        1 => Some("MouseClickState"),
        35 => Some("MouseDeltaX"),
        36 => Some("MouseDeltaY"),
        // Routing / source identity (CGEventTypes.h).
        39 => Some("TargetProcessSerialNumber"),
        40 => Some("TargetUnixProcessID"),
        45 => Some("SourceStateID"),
        // CGS event subtype, used by gesture/dock/etc.
        55 => Some("CGSEventType"),
        // Scroll wheel (CGEventTypes.h).
        88 => Some("Scroll.IsContinuous"),
        93 => Some("Scroll.FixedPtDeltaAxis1"),
        94 => Some("Scroll.FixedPtDeltaAxis2"),
        96 => Some("Scroll.PointDeltaAxis1"),
        97 => Some("Scroll.PointDeltaAxis2"),
        99 => Some("Scroll.Phase"),
        // Gesture (CGS subtype 29). 110 doubles as the IOHID event-type
        // field for type-29 events; we surface that separately as `hid=`
        // and don't relabel it here.
        113 => Some("Gesture.MagnifyValue"),
        114 => Some("Gesture.RotateValue"),
        132 => Some("Gesture.Phase"),
        135 => Some("Gesture.ScrollFlagBits"),
        // DockControl (CGS subtype 30) — 123 also doubles as
        // Scroll.MomentumPhase on scroll-wheel events.
        123 => Some("Dock.SwipeMotion / Scroll.MomentumPhase"),
        124 => Some("Dock.SwipeProgress"),
        129 => Some("Dock.SwipeVelocityX"),
        130 => Some("Dock.SwipeVelocityY"),
        // Acceleration-bypass mouse motion (CGEventTypes.h, modern macOS).
        170 => Some("UnacceleratedPointerMovementX"),
        171 => Some("UnacceleratedPointerMovementY"),
        _ => None,
    }
}

/// Decode a `kCGEventGestureHIDType` value to its IOHIDEventType name
/// (from `IOHIDEventTypes.h`). Used for both type-29 Gesture events
/// (where this field disambiguates Magnify vs Rotate) and type-30
/// DockControl events (where it identifies DockSwipe).
fn hid_type_name(t: i64) -> Option<&'static str> {
    match t {
        0 => Some("NULL"),
        1 => Some("VendorDefined"),
        2 => Some("Button"),
        3 => Some("Keyboard"),
        4 => Some("Translation"),
        5 => Some("Rotation"),
        6 => Some("Scroll"),
        7 => Some("Scale"),
        8 => Some("Zoom"),
        9 => Some("Velocity"),
        10 => Some("Orientation"),
        11 => Some("Digitizer"),
        16 => Some("NavigationSwipe"),
        23 => Some("DockSwipe"),
        _ => None,
    }
}

/// CGS gesture phase / IOHID phase bits — same encoding for both, used
/// in field 132 (Gesture.Phase) and 99 (Scroll.Phase).
fn phase_name(p: i64) -> Option<&'static str> {
    match p {
        0 => Some("None"),
        1 => Some("Began"),
        2 => Some("Changed"),
        4 => Some("Ended"),
        8 => Some("Cancelled"),
        16 => Some("MayBegin"),
        _ => None,
    }
}

/// Some gesture fields encode a Float32 value in an integer slot (e.g.
/// `kCGEventScrollGestureFlagBits` carries the bits of a `(Float32)
/// progress` value — see the long comment in `output.rs`). If `int_val`
/// reinterprets cleanly as a small finite f32 with a fractional part,
/// return it so the dumper can show both forms. The bounds (~1e-6 to
/// ~1e6) are tuned to skip values that *happen* to bit-decode but only
/// to absurdly small/large floats — those are almost certainly genuine
/// integer fields, not float-bit-pattern fields.
fn maybe_f32_bits(int_val: i64) -> Option<f32> {
    if int_val == 0 || int_val < i32::MIN as i64 || int_val > u32::MAX as i64 {
        return None;
    }
    let bits = int_val as u32;
    let f = f32::from_bits(bits);
    if !f.is_finite() {
        return None;
    }
    let abs = f.abs();
    if abs < 1e-6 || abs > 1e6 {
        return None;
    }
    if f.fract() == 0.0 {
        // Exact integer — the int interpretation is more useful.
        return None;
    }
    Some(f)
}

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
    let (label, hid_type): (&str, Option<i64>) = if ty == kCGEventScrollWheel {
        // Scroll wheel events don't carry a CGS subtype in field 55;
        // they're identified by the wrapper-level CGEventType. No
        // analogue of `kCGEventGestureHIDType` either.
        ("Scroll", None)
    } else if cgs_type == kCGSEventGesture {
        let h = unsafe { CGEventGetIntegerValueField(event, kCGEventGestureHIDType) };
        ("Gesture", Some(h))
    } else if cgs_type == kCGSEventDockControl {
        let h = unsafe { CGEventGetIntegerValueField(event, kCGEventGestureHIDType) };
        ("DockCtl", Some(h))
    } else {
        return event;
    };

    let ts = unsafe { CGEventGetTimestamp(event) };
    let now_ns = Timestamp::now().as_nanos();
    let delta_ms = (now_ns as i128 - ts as i128) as f64 / 1_000_000.0;

    // Header: event kind, optional HID type, our timestamp story.
    let hid_str = match hid_type {
        None => String::new(),
        Some(h) => match hid_type_name(h) {
            Some(n) => format!(" hid={h}({n})"),
            None => format!(" hid={h}"),
        },
    };
    let delta_tag = if delta_ms.abs() < 50.0 {
        "" // post-to-tap latency, OS likely rewrote
    } else if delta_ms > 0.0 {
        " [offset preserved: backdated]"
    } else {
        " [offset preserved: post-dated]"
    };
    println!(
        "{label}{hid_str} ts={ts} Δ={delta_ms:+.3}ms{delta_tag}"
    );

    // Per-field detail. Walk a wide range so scroll-wheel fields (88,
    // 93–99) are covered alongside gesture fields (100–200). 0–250 is
    // a safe overshoot — unset fields read as 0 and get filtered.
    for f in 0u32..=250 {
        if f == kCGSEventTypeField || f == kCGEventGestureHIDType {
            continue; // already shown in header
        }
        let i = unsafe { CGEventGetIntegerValueField(event, f) };
        let d = unsafe { CGEventGetDoubleValueField(event, f) };

        // Skip fields where both views are zero. This is the dominant
        // case (CGEvent has many fields; few are set per event).
        if i == 0 && d == 0.0 {
            continue;
        }

        let mut tokens: Vec<String> = Vec::new();

        // Integer view, with phase / hid / float-bits annotations.
        if i != 0 {
            let mut int_tok = format!("{i}");
            // Phase fields decode the same bit values for both 99
            // (Scroll.Phase) and 132 (Gesture.Phase).
            if (f == 99 || f == 132)
                && let Some(name) = phase_name(i)
            {
                int_tok = format!("{i}({name})");
            } else if f == 110
                && let Some(name) = hid_type_name(i)
            {
                int_tok = format!("{i}({name})");
            } else if i as u64 == ts {
                // Quite a few gesture fields mirror the top-level
                // timestamp (e.g. 169 in user-observed pinch events).
                int_tok = format!("{i}(=ts)");
            } else if let Some(f32_val) = maybe_f32_bits(i) {
                // `kCGEventScrollGestureFlagBits` and several other
                // synthesized fields stuff a Float32 into an integer
                // slot. Show both forms.
                int_tok = format!("{i}(≈{f32_val:+.4}f)");
            }
            tokens.push(int_tok);
        }

        // Double view, only when distinct from the integer view (the
        // CGEvent API blurs types — fields stored as int return
        // int-as-double from the double-getter, which would duplicate).
        if d != 0.0 && d != i as f64 && (d.fract() != 0.0 || d.abs() > 1e15) {
            tokens.push(format!("≈{d:+.4}"));
        }

        if tokens.is_empty() {
            continue;
        }

        let label_prefix = match field_name(f) {
            Some(name) => format!("    {name}({f})"),
            None => format!("    field {f}"),
        };
        println!("{label_prefix} = {}", tokens.join(" / "));
    }

    event
}

fn main() -> anyhow::Result<()> {
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
    }

    let mask = (1u64 << kCGSEventGesture)
        | (1u64 << kCGSEventDockControl)
        | (1u64 << kCGEventScrollWheel);
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
        "gesture-tap: listening on kCGSessionEventTap (Gesture / DockCtl / Scroll). \
         Perform gestures on your trackpad. Ctrl-C to stop."
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
