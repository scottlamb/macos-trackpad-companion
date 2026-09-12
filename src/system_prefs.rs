//! System settings the companion has to reason about but must never
//! change on the user's behalf.
//!
//! Currently one: whether macOS is configured to ignore the built-in
//! trackpad while an external pointing device is attached. That decides
//! whether quitting can leave the machine with no pointer at all, and
//! so whether the quit warning is worth showing or just noise.

use core_foundation::base::TCFType;
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFGetTypeID, CFRelease, CFTypeRef};
use core_foundation_sys::number::{
    CFBooleanGetTypeID, CFBooleanGetValue, CFBooleanRef, CFNumberGetTypeID, CFNumberGetValue,
    CFNumberRef, kCFNumberIntType,
};
use core_foundation_sys::preferences::CFPreferencesCopyAppValue;

/// System Settings > Accessibility > Pointer Control > "Ignore built-in
/// trackpad when mouse or wireless trackpad is present".
///
/// The key is `USBMouseStopsTrackpad`, and it is mirrored across the
/// wired and Bluetooth trackpad domains — System Settings writes both.
/// It is emphatically *not* `mouseDriverIgnoreTrackpad` under
/// `com.apple.universalaccess`, which is what the pane's location
/// suggests and what older references describe; that key does not exist
/// on macOS 26.
const KEY: &str = "USBMouseStopsTrackpad";
const DOMAINS: [&str; 2] = [
    "com.apple.AppleMultitouchTrackpad",
    "com.apple.driver.AppleBluetoothMultitouch.trackpad",
];

/// Whether macOS will ignore the built-in trackpad while an external
/// pointing device is attached.
///
/// An absent key means the switch has never been touched, which is the
/// same as off — preferences are written lazily, so "missing" is a
/// normal state rather than an error.
pub fn builtin_trackpad_ignored() -> bool {
    DOMAINS.iter().any(|domain| read_flag(domain, KEY))
}

fn read_flag(domain: &str, key: &str) -> bool {
    let cf_key = CFString::new(key);
    let cf_domain = CFString::new(domain);

    // Copy semantics: we own the result and must release it.
    let value = unsafe {
        CFPreferencesCopyAppValue(
            cf_key.as_concrete_TypeRef(),
            cf_domain.as_concrete_TypeRef(),
        )
    };
    if value.is_null() {
        return false;
    }

    let set = unsafe {
        let type_id = CFGetTypeID(value as CFTypeRef);
        if type_id == CFBooleanGetTypeID() {
            CFBooleanGetValue(value as CFBooleanRef)
        } else if type_id == CFNumberGetTypeID() {
            // System Settings writes this one as a number, not a bool.
            let mut n: i32 = 0;
            let ok = CFNumberGetValue(
                value as CFNumberRef,
                kCFNumberIntType,
                &mut n as *mut i32 as *mut std::ffi::c_void,
            );
            ok && n != 0
        } else {
            log::debug!("{domain}/{key} has an unexpected type; treating as off");
            false
        }
    };
    unsafe { CFRelease(value as CFTypeRef) };
    set
}
