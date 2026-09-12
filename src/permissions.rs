//! The two privacy grants the companion needs, and how to ask for them.
//!
//! Input Monitoring gates `IOHIDManagerOpen`; Accessibility gates
//! `CGEventPost`. The second one is the dangerous one: without it every
//! call still *succeeds*, the gesture engine runs normally, and nothing
//! moves on screen. There is no error to report unless we check for the
//! grant explicitly, which is what this module is for.
//!
//! Both are checked through the real APIs rather than inferred from a
//! failed operation. Inference can't tell a denial apart from an
//! unrelated failure — `IOHIDManagerOpen` returns `kIOReturnExclusiveAccess`
//! when another process holds the devices, which has nothing to do with
//! permissions but looks identical from the outside.

use std::ffi::c_void;

use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::string::CFString;
use core_foundation_sys::string::CFStringRef;
use objc2_app_kit::NSWorkspace;
use objc2_foundation::{NSString, NSURL};

// IOKit/hidsystem/IOHIDLib.h. Both enums are plain C enums, so the
// discriminants are positional: PostEvent = 0, ListenEvent = 1, and
// Granted = 0, Denied = 1, Unknown = 2.
const K_IOHID_REQUEST_TYPE_LISTEN_EVENT: u32 = 1;
const K_IOHID_ACCESS_TYPE_GRANTED: u32 = 0;
const K_IOHID_ACCESS_TYPE_DENIED: u32 = 1;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    /// macOS 10.15+. Returns an `IOHIDAccessType`.
    fn IOHIDCheckAccess(request_type: u32) -> u32;
    /// Prompts the user. Returns whether access ended up granted.
    /// Only shows a prompt the first time — once denied, the user has
    /// to change it in System Settings.
    fn IOHIDRequestAccess(request_type: u32) -> u8;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// Returns `Boolean` (unsigned char), not C99 `bool`.
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: *const c_void) -> u8;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
}

/// System Settings deep links. Dropping the user on the right pane beats
/// "go find it in Privacy & Security".
const INPUT_MONITORING_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ListenEvent";
const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    Granted,
    /// Explicitly refused. The system prompt will not appear again;
    /// only System Settings can change this.
    Denied,
    /// Never asked. A prompt is still possible.
    Unknown,
}

impl Access {
    pub fn is_granted(self) -> bool {
        self == Access::Granted
    }
}

/// Snapshot of both grants. Cheap enough to re-read on a timer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct State {
    pub input_monitoring: Access,
    /// Accessibility has no tri-state equivalent — `AXIsProcessTrusted`
    /// only says yes or no.
    pub accessibility: bool,
}

impl State {
    pub fn current() -> Self {
        Self {
            input_monitoring: input_monitoring(),
            accessibility: accessibility(),
        }
    }

    pub fn all_granted(&self) -> bool {
        self.input_monitoring.is_granted() && self.accessibility
    }
}

pub fn input_monitoring() -> Access {
    match unsafe { IOHIDCheckAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) } {
        K_IOHID_ACCESS_TYPE_GRANTED => Access::Granted,
        K_IOHID_ACCESS_TYPE_DENIED => Access::Denied,
        _ => Access::Unknown,
    }
}

pub fn accessibility() -> bool {
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Ask for Input Monitoring. Shows the system prompt only if the user
/// has never answered; returns the resulting state either way.
///
/// TODO(permissions): revisit — this does not reliably prompt.
///
/// Observed on macOS 26.6.2 with a Developer ID-signed LSUIElement
/// bundle launched via `open`:
///   * no dialog ever appeared for this app;
///   * `IOHIDCheckAccess` went Unknown -> Denied within ~70 ms of the
///     call, i.e. macOS recorded a refusal without asking;
///   * the app never appeared in System Settings > Privacy & Security >
///     Input Monitoring, so "Open Settings" landed on a pane with
///     nothing to toggle;
///   * adding the bundle by hand with the `+` button worked, and the
///     grant then persisted across rebuilds.
///
/// By contrast `AXIsProcessTrustedWithOptions` (Accessibility) prompted
/// correctly on the first try, so this is specific to the HID path.
///
/// Worth testing when revisited:
///   * whether an app with Regular (non-agent) activation policy
///     prompts, i.e. whether LSUIElement is the trigger;
///   * whether requesting later — after NSApp is active and the window
///     is on screen — behaves differently from requesting during
///     startup;
///   * whether a denial cached earlier in the same boot session
///     suppresses later prompts;
///   * whether notarization or hardened runtime changes it.
///
/// Reset state before each attempt:
///   tccutil reset ListenEvent net.guemez.trackpad-companion
///
/// Note that a fresh grant does not take effect in this process —
/// macOS requires a restart before `IOHIDManagerOpen` will succeed.
pub fn request_input_monitoring() -> bool {
    unsafe { IOHIDRequestAccess(K_IOHID_REQUEST_TYPE_LISTEN_EVENT) != 0 }
}

/// Ask for Accessibility, showing the system dialog with its
/// "Open System Settings" button.
pub fn request_accessibility() -> bool {
    let key = unsafe { CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt) };
    let value = CFBoolean::true_value();
    let options: CFDictionary<CFString, CFType> =
        CFDictionary::from_CFType_pairs(&[(key, value.as_CFType())]);
    unsafe { AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef() as *const c_void) != 0 }
}

pub fn open_input_monitoring_settings() {
    open_url(INPUT_MONITORING_PANE);
}

/// Accessibility > Pointer Control, where the built-in trackpad can be
/// kept enabled while an external pointing device is attached.
pub fn open_pointer_control_settings() {
    open_url("x-apple.systempreferences:com.apple.preference.universalaccess");
}

pub fn open_accessibility_settings() {
    open_url(ACCESSIBILITY_PANE);
}

fn open_url(url: &str) {
    let string = NSString::from_str(url);
    match NSURL::URLWithString(&string) {
        Some(url_obj) => {
            let opened = NSWorkspace::sharedWorkspace().openURL(&url_obj);
            if !opened {
                log::warn!("failed to open settings pane: {url}");
            }
        }
        None => log::warn!("malformed settings URL: {url}"),
    }
}
