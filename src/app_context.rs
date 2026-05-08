//! Resolve "what app is the cursor over?" for the gesture allowlist /
//! denylist filter. macOS dispatches pinch/rotate/scroll/click to the
//! window under the cursor (regardless of frontmost), so the filter
//! lines up with where the gesture would actually land. Mission
//! Control / Spaces 3F/4F swipes are the system-wide exception — for
//! those the lookup just expresses user intent ("don't fire this
//! gesture when my cursor is parked over Terminal").
//!
//! Implementation: query the on-screen window list (front-to-back),
//! find the first normal-layer window whose bounds contain the cursor,
//! map its owner PID to a bundle ID via `NSRunningApplication`. Returns
//! `None` when the cursor sits over the desktop or any window we don't
//! recognise (menu bar, dock, screen-saver, etc.) — callers decide how
//! that interacts with `Only` / `Except` policies.

use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
use core_foundation::base::{CFRelease, CFTypeRef, TCFType};
use core_foundation::dictionary::{CFDictionaryGetValue, CFDictionaryRef};
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::geometry::CGPoint;
use core_graphics::window::{
    CGWindowListOption, kCGNullWindowID, kCGWindowBounds, kCGWindowLayer,
    kCGWindowListExcludeDesktopElements, kCGWindowListOptionOnScreenOnly, kCGWindowOwnerPID,
};
use objc2_app_kit::NSRunningApplication;
use std::ffi::c_void;

unsafe extern "C" {
    fn CGEventCreate(source: *mut c_void) -> *mut c_void;
    fn CGEventGetLocation(event: *mut c_void) -> CGPoint;
    fn CGWindowListCopyWindowInfo(
        option: CGWindowListOption,
        relative_to_window: u32,
    ) -> CFArrayRef;
}

/// Bundle ID of the application owning the topmost normal window
/// underneath the current cursor position, or `None` when no such
/// window exists (cursor over desktop / menu bar / dock / unknown
/// layer).
pub fn bundle_id_under_cursor() -> Option<String> {
    let cursor = current_cursor_location()?;
    let pid = pid_under_cursor(cursor)?;
    bundle_id_for_pid(pid)
}

fn current_cursor_location() -> Option<CGPoint> {
    // CGEventCreate(NULL) returns an "empty" event whose `location`
    // reflects the *current* cursor position — same trick `output.rs`
    // uses, replicated here so this module doesn't need an `Emitter`.
    unsafe {
        let e = CGEventCreate(std::ptr::null_mut());
        if e.is_null() {
            return None;
        }
        let p = CGEventGetLocation(e);
        CFRelease(e as CFTypeRef);
        Some(p)
    }
}

/// Walk the on-screen window list (front-to-back per CGWindowList docs)
/// and return the owner PID of the first window whose bounds contain
/// `cursor`. Skips non-zero-layer windows so menu-bar items, the Dock,
/// and floating utility chrome don't shadow the real app underneath.
fn pid_under_cursor(cursor: CGPoint) -> Option<i32> {
    let opts: CGWindowListOption =
        kCGWindowListOptionOnScreenOnly | kCGWindowListExcludeDesktopElements;
    let arr = unsafe { CGWindowListCopyWindowInfo(opts, kCGNullWindowID) };
    if arr.is_null() {
        return None;
    }
    let count = unsafe { CFArrayGetCount(arr) };
    let mut found: Option<i32> = None;
    for i in 0..count {
        let dict = unsafe { CFArrayGetValueAtIndex(arr, i) } as CFDictionaryRef;
        if dict.is_null() {
            continue;
        }
        let layer = dict_get_i64(dict, unsafe { kCGWindowLayer }).unwrap_or(i64::MAX);
        if layer != 0 {
            continue;
        }
        let Some(bounds) = dict_get_cgrect_bounds(dict) else {
            continue;
        };
        if !rect_contains(bounds, cursor) {
            continue;
        }
        if let Some(pid) = dict_get_i64(dict, unsafe { kCGWindowOwnerPID }) {
            found = Some(pid as i32);
            break;
        }
    }
    unsafe { CFRelease(arr as CFTypeRef) };
    found
}

fn bundle_id_for_pid(pid: i32) -> Option<String> {
    let app = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)?;
    let id = app.bundleIdentifier()?;
    Some(id.to_string())
}

fn dict_get_i64(dict: CFDictionaryRef, key: CFStringRef) -> Option<i64> {
    let v = unsafe { CFDictionaryGetValue(dict, key as *const c_void) };
    if v.is_null() {
        return None;
    }
    let n = unsafe { CFNumber::wrap_under_get_rule(v as _) };
    n.to_i64()
}

/// `kCGWindowBounds` stores a CFDictionary with X/Y/Width/Height
/// CFNumbers (the same shape `CGRectMakeWithDictionaryRepresentation`
/// consumes). Doing the four lookups directly is cheaper than building
/// a temporary dict wrapper just to call that API.
fn dict_get_cgrect_bounds(dict: CFDictionaryRef) -> Option<(f64, f64, f64, f64)> {
    let bounds_val =
        unsafe { CFDictionaryGetValue(dict, kCGWindowBounds as *const c_void) } as CFDictionaryRef;
    if bounds_val.is_null() {
        return None;
    }
    let x = sub_dict_f64(bounds_val, "X")?;
    let y = sub_dict_f64(bounds_val, "Y")?;
    let w = sub_dict_f64(bounds_val, "Width")?;
    let h = sub_dict_f64(bounds_val, "Height")?;
    Some((x, y, w, h))
}

fn sub_dict_f64(dict: CFDictionaryRef, key: &str) -> Option<f64> {
    let k = CFString::new(key);
    let v = unsafe { CFDictionaryGetValue(dict, k.as_concrete_TypeRef() as *const c_void) };
    if v.is_null() {
        return None;
    }
    let n = unsafe { CFNumber::wrap_under_get_rule(v as _) };
    n.to_f64().or_else(|| n.to_i64().map(|i| i as f64))
}

fn rect_contains(b: (f64, f64, f64, f64), p: CGPoint) -> bool {
    let (x, y, w, h) = b;
    p.x >= x && p.x < x + w && p.y >= y && p.y < y + h
}
