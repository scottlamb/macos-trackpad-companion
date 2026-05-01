//! IOHIDManager wrapper. Matches PTP-class digitizer interfaces
//! (DeviceUsagePage 0x0D / Usage 0x05) and pumps input reports into
//! a user-supplied callback on the main run loop.
//!
//! On macOS the Input Monitoring privacy bucket gates `IOHIDManagerOpen`
//! for any device we don't own; the first run will prompt the user to
//! grant it via System Settings. Returns a clear error message on the
//! known failure code (0xE00002C5).

#![allow(non_upper_case_globals)]

use crate::descriptor::{self, Layout};
use crate::report::{self, Frame};
use anyhow::{Result, bail};
use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::runloop::{CFRunLoop, CFRunLoopRun, CFRunLoopStop, kCFRunLoopDefaultMode};
use core_foundation::string::CFString;
use std::ffi::c_void;
use std::os::raw::c_int;
use std::pin::Pin;

// ---- IOHID types & constants ----

type IOHIDManagerRef = *mut c_void;
type IOHIDDeviceRef = *mut c_void;
type IOOptionBits = u32;
type IOReturn = c_int;
type IOHIDReportType = u32;

const kIOHIDOptionsTypeNone: IOOptionBits = 0;
const kIOReturnSuccess: IOReturn = 0;
const kIOHIDReportTypeFeature: IOHIDReportType = 2;

/// Microsoft Precision Touchpad "Input Mode" feature report ID (declared
/// in the firmware's PTP descriptor at `rmk/src/hid.rs:PTP_REPORT_DESC`).
/// Writing `[0x03]` here flips the firmware's `TRACKPAD_USE_PTP` atomic
/// — without this, the firmware never knows to switch from its mouse
/// path to publishing PTP reports.
const PTP_INPUT_MODE_REPORT_ID: isize = 0x08;
const PTP_INPUT_MODE_MULTITOUCH: u8 = 0x03;
const PTP_INPUT_MODE_MOUSE: u8 = 0x00;

const KEY_VENDOR_ID: &str = "VendorID";
const KEY_PRODUCT_ID: &str = "ProductID";
const KEY_DEVICE_USAGE_PAGE: &str = "DeviceUsagePage";
const KEY_DEVICE_USAGE: &str = "DeviceUsage";
const KEY_PRODUCT: &str = "Product";
const KEY_REPORT_DESCRIPTOR: &str = "ReportDescriptor";

const PTP_USAGE_PAGE: i32 = 0x0D;
const PTP_USAGE: i32 = 0x05;

type IOHIDReportCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    report_type: IOHIDReportType,
    report_id: u32,
    report: *mut u8,
    report_length: isize,
);

type IOHIDDeviceCallback = unsafe extern "C" fn(
    context: *mut c_void,
    result: IOReturn,
    sender: *mut c_void,
    device: IOHIDDeviceRef,
);

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(allocator: *mut c_void, options: IOOptionBits) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: *const c_void);
    fn IOHIDManagerRegisterDeviceMatchingCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerRegisterDeviceRemovalCallback(
        manager: IOHIDManagerRef,
        callback: IOHIDDeviceCallback,
        context: *mut c_void,
    );
    fn IOHIDManagerScheduleWithRunLoop(
        manager: IOHIDManagerRef,
        run_loop: *mut c_void,
        run_loop_mode: *const c_void,
    );
    fn IOHIDManagerOpen(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;
    fn IOHIDManagerClose(manager: IOHIDManagerRef, options: IOOptionBits) -> IOReturn;

    fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: *const c_void) -> *const c_void;
    fn IOHIDDeviceRegisterInputReportCallback(
        device: IOHIDDeviceRef,
        report: *mut u8,
        report_length: isize,
        callback: IOHIDReportCallback,
        context: *mut c_void,
    );
    /// `IOReturn IOHIDDeviceSetReport(IOHIDDeviceRef, IOHIDReportType,
    /// CFIndex reportID, const uint8_t *report, CFIndex reportLength)`.
    /// CFIndex is `long` → `isize` on 64-bit macOS.
    fn IOHIDDeviceSetReport(
        device: IOHIDDeviceRef,
        report_type: IOHIDReportType,
        report_id: isize,
        report: *const u8,
        report_length: isize,
    ) -> IOReturn;
}

// ---- Public API ----

#[derive(Clone, Copy, Debug)]
pub struct Filter {
    pub vid: Option<u16>,
    pub pid: Option<u16>,
}

pub struct Manager {
    raw: IOHIDManagerRef,
    filter: Filter,
    bridge: Option<Pin<Box<Bridge>>>,
}

/// Owns the user's per-frame callback and the per-device state. All
/// callbacks fire on the run-loop thread, so single-threaded `&mut`
/// access through raw pointers is safe.
struct Bridge {
    on_frame: Box<dyn FnMut(Frame)>,
    devices: Vec<Pin<Box<DeviceState>>>,
}

struct DeviceState {
    device: IOHIDDeviceRef,
    layout: Layout,
    buf: Vec<u8>,
    bridge: *mut Bridge,
}

impl Drop for DeviceState {
    fn drop(&mut self) {
        // Revert the firmware to mouse mode. Fires both on USB removal
        // (after the device is gone — the SET will fail, that's fine)
        // and on graceful companion shutdown when `Manager` drops the
        // bridge (device still attached, the SET takes effect and the
        // user's trackpad keeps working as a plain mouse).
        set_ptp_input_mode(self.device, PTP_INPUT_MODE_MOUSE);
    }
}

impl Manager {
    pub fn new(filter: Filter) -> Result<Self> {
        let raw = unsafe { IOHIDManagerCreate(std::ptr::null_mut(), kIOHIDOptionsTypeNone) };
        if raw.is_null() {
            bail!("IOHIDManagerCreate returned NULL");
        }
        Ok(Self {
            raw,
            filter,
            bridge: None,
        })
    }

    /// Open the manager and pump the run loop. Calls `on_frame` for every
    /// decoded touch report from any matched PTP device. Blocks until
    /// SIGINT or the run loop is stopped.
    pub fn run<F>(&mut self, on_frame: F) -> Result<()>
    where
        F: FnMut(Frame) + 'static,
    {
        let bridge = Box::pin(Bridge {
            on_frame: Box::new(on_frame),
            devices: Vec::new(),
        });
        self.bridge = Some(bridge);
        let bridge_ptr: *mut Bridge =
            unsafe { self.bridge.as_mut().unwrap().as_mut().get_unchecked_mut() };

        let matching = build_match_dict(&self.filter);

        unsafe {
            IOHIDManagerSetDeviceMatching(self.raw, matching.as_concrete_TypeRef() as *const _);
            IOHIDManagerRegisterDeviceMatchingCallback(
                self.raw,
                on_device_matched,
                bridge_ptr as *mut c_void,
            );
            IOHIDManagerRegisterDeviceRemovalCallback(
                self.raw,
                on_device_removed,
                bridge_ptr as *mut c_void,
            );
            IOHIDManagerScheduleWithRunLoop(
                self.raw,
                CFRunLoop::get_current().as_concrete_TypeRef() as *mut _,
                kCFRunLoopDefaultMode as *const _,
            );
        }

        let rv = unsafe { IOHIDManagerOpen(self.raw, kIOHIDOptionsTypeNone) };
        if rv != kIOReturnSuccess {
            if rv as u32 == 0xE00002C5 {
                bail!(
                    "IOHIDManagerOpen denied (0xE00002C5): grant Input Monitoring \
                     in System Settings → Privacy & Security → Input Monitoring."
                );
            }
            bail!("IOHIDManagerOpen failed: {:#x}", rv as u32);
        }

        log::info!(
            "waiting for PTP device (vid={:?} pid={:?})",
            self.filter.vid,
            self.filter.pid
        );

        // Without an explicit teardown path the kernel SIGTERMs us straight
        // to exit and `DeviceState::drop` (which reverts the firmware to
        // mouse mode) never runs — the trackpad would stay stuck in PTP
        // mode after the companion exits. Stop the run loop on signal so
        // `CFRunLoopRun` returns and Rust unwinding drops the bridge
        // normally.
        install_signal_shutdown();

        unsafe { CFRunLoopRun() };

        Ok(())
    }
}

/// Block SIGINT/SIGTERM in the main thread and spawn a sigwait worker
/// that stops the main run loop when either arrives. `sigwait` on a
/// dedicated thread is the supported way to handle signals from a
/// CFRunLoop process (raw signal handlers can't safely call CF APIs;
/// most CF functions aren't async-signal-safe).
///
/// Must be called on the main thread (the one that will run
/// `CFRunLoopRun`); the captured run loop is whichever
/// `CFRunLoop::get_current()` returns at this call site.
fn install_signal_shutdown() {
    use std::mem;
    use std::ptr;

    // The CFRunLoopRef from get_current() is reference-counted by Apple
    // but the main run loop has effectively static lifetime, so capturing
    // its raw pointer as a `usize` for the worker thread is safe.
    // Going through `usize` side-steps `!Send` on `CFRunLoop` itself.
    let run_loop_ref = CFRunLoop::get_current().as_concrete_TypeRef() as usize;

    unsafe {
        let mut set: libc::sigset_t = mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGINT);
        libc::sigaddset(&mut set, libc::SIGTERM);
        // Block in the main thread *before* spawning the worker so the
        // worker inherits the block; otherwise a signal arriving between
        // pthread_sigmask and the spawn would be delivered to the main
        // thread (default-action: terminate) and we'd skip cleanup.
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, ptr::null_mut());
    }

    std::thread::spawn(move || {
        unsafe {
            let mut set: libc::sigset_t = mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGINT);
            libc::sigaddset(&mut set, libc::SIGTERM);
            let mut sig: libc::c_int = 0;
            // sigwait removes the matching signal from the pending set
            // and returns it; safe to call CF APIs once we're back in
            // ordinary thread context (not a signal handler).
            let _ = libc::sigwait(&set, &mut sig);
            log::info!("received signal {sig}, shutting down");
            CFRunLoopStop(run_loop_ref as *mut _);
        }
    });
}

impl Drop for Manager {
    fn drop(&mut self) {
        // Drop the bridge first so each `DeviceState::drop` (writing
        // Input Mode = 0 back to the firmware) fires while the
        // IOHIDManager is still open. Closing the manager closes every
        // opened device, after which `IOHIDDeviceSetReport` returns
        // kIOReturnNotOpen and the cleanup write would be wasted.
        self.bridge = None;
        unsafe {
            IOHIDManagerClose(self.raw, kIOHIDOptionsTypeNone);
        }
    }
}

unsafe extern "C" fn on_device_matched(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    device: IOHIDDeviceRef,
) {
    let bridge = unsafe { &mut *(context as *mut Bridge) };

    let product = read_string_property(device, KEY_PRODUCT).unwrap_or_else(|| "<unknown>".into());
    let vid = read_number_property(device, KEY_VENDOR_ID);
    let pid = read_number_property(device, KEY_PRODUCT_ID);
    let desc = match read_data_property(device, KEY_REPORT_DESCRIPTOR) {
        Some(d) => d,
        None => {
            log::warn!("matched \"{product}\" but couldn't read report descriptor");
            return;
        }
    };
    let layout = match descriptor::parse(&desc) {
        Ok(l) => l,
        Err(e) => {
            log::warn!(
                "matched \"{product}\" but descriptor parse failed: {e:#}; descriptor was {} bytes",
                desc.len()
            );
            return;
        }
    };
    log::info!(
        "matched \"{product}\" (vid={} pid={}): {} contacts, logical max {}×{}, \
         {} bytes/contact, payload {} bytes total",
        vid.map(|v| format!("{:#06x}", v as u16)).unwrap_or_else(|| "?".into()),
        pid.map(|v| format!("{:#06x}", v as u16)).unwrap_or_else(|| "?".into()),
        layout.contact_slots,
        layout.logical_x_max,
        layout.logical_y_max,
        layout.bytes_per_contact,
        layout.total_payload_bytes,
    );
    log::info!(
        "  layout offsets: report_id=0x{:02x} fingers@{} scan_time@{} contact_count@{} \
         button@{} bit{} (descriptor: {} bytes)",
        layout.report_id,
        layout.fingers_offset,
        layout.scan_time_offset,
        layout.contact_count_offset,
        layout.button_offset,
        layout.button_bit,
        desc.len(),
    );

    let buf_len = layout.total_payload_bytes.max(64);
    let mut state = Box::pin(DeviceState {
        device,
        layout,
        buf: vec![0u8; buf_len],
        bridge: bridge as *mut Bridge,
    });

    unsafe {
        let s = state.as_mut().get_unchecked_mut();
        let buf_ptr = s.buf.as_mut_ptr();
        let buf_len_isize = s.buf.len() as isize;
        let ctx_ptr = s as *mut DeviceState as *mut c_void;
        IOHIDDeviceRegisterInputReportCallback(
            device,
            buf_ptr,
            buf_len_isize,
            on_input_report,
            ctx_ptr,
        );
    }

    // Tell the firmware to enter PTP mode. Without this the firmware's
    // existing HID-mouse `TrackpadProcessor` keeps publishing on the
    // standard mouse path and the PTP input reports we're about to read
    // never get produced. The corresponding `TRACKPAD_USE_PTP` gate is
    // in `rmk/src/hid.rs`.
    set_ptp_input_mode(device, PTP_INPUT_MODE_MULTITOUCH);

    bridge.devices.push(state);
}

/// Send a 1-byte SET_FEATURE on the PTP Input Mode report. Best-effort:
/// errors are logged but don't abort the matched-device flow — even if
/// the SET fails the companion can still observe traffic, just nothing
/// useful arrives until the firmware enters PTP mode by some other path.
fn set_ptp_input_mode(device: IOHIDDeviceRef, mode: u8) {
    let payload = [mode];
    let rv = unsafe {
        IOHIDDeviceSetReport(
            device,
            kIOHIDReportTypeFeature,
            PTP_INPUT_MODE_REPORT_ID,
            payload.as_ptr(),
            payload.len() as isize,
        )
    };
    if rv == kIOReturnSuccess {
        log::info!("PTP input mode set to {:#04x}", mode);
    } else {
        // 0xE00002C7 = kIOReturnUnsupported; common when the matched
        // interface lacks the InputMode feature (i.e. it's a real PTP
        // device that uses a different report ID or layout). Our own
        // firmware uses 0x08 by construction.
        log::warn!(
            "SET_FEATURE InputMode={:#04x} failed: {:#x}",
            mode,
            rv as u32
        );
    }
}

unsafe extern "C" fn on_device_removed(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    device: IOHIDDeviceRef,
) {
    let bridge = unsafe { &mut *(context as *mut Bridge) };
    bridge.devices.retain(|d| d.device != device);
    log::info!("device removed");
}

unsafe extern "C" fn on_input_report(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    _report_type: IOHIDReportType,
    _report_id: u32,
    report: *mut u8,
    report_length: isize,
) {
    let state = unsafe { &mut *(context as *mut DeviceState) };
    let bridge = unsafe { &mut *state.bridge };
    let bytes = unsafe { std::slice::from_raw_parts(report, report_length as usize) };
    if log::log_enabled!(log::Level::Trace) {
        log::trace!("input report ({} bytes): {}", bytes.len(), hex(bytes));
    }
    let Some(frame) = report::decode(&state.layout, bytes) else {
        log::debug!("decode failed for {}-byte report", bytes.len());
        return;
    };
    if log::log_enabled!(log::Level::Trace) {
        log::trace!(
            "  frame: contact_count={} scan_time={} button={} contacts={:?}",
            frame.contacts.len(),
            frame.scan_time_100us,
            frame.button,
            frame.contacts,
        );
    } else if log::log_enabled!(log::Level::Debug) && let Some(c) = frame.contacts.first() {
        log::debug!(
            "frame n={} c0 id={} raw=({:>4},{:>4}) norm=({:.3},{:.3}) tip={} button={}",
            frame.contacts.len(),
            c.id,
            c.raw_x,
            c.raw_y,
            c.x,
            c.y,
            c.tip,
            frame.button,
        );
    }
    (bridge.on_frame)(frame);
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && i % 4 == 0 {
            s.push(' ');
        }
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn build_match_dict(filter: &Filter) -> CFDictionary<CFString, CFType> {
    let mut pairs: Vec<(CFString, CFType)> = Vec::new();
    pairs.push((
        CFString::from_static_string(KEY_DEVICE_USAGE_PAGE),
        CFNumber::from(PTP_USAGE_PAGE).as_CFType(),
    ));
    pairs.push((
        CFString::from_static_string(KEY_DEVICE_USAGE),
        CFNumber::from(PTP_USAGE).as_CFType(),
    ));
    if let Some(v) = filter.vid {
        pairs.push((
            CFString::from_static_string(KEY_VENDOR_ID),
            CFNumber::from(v as i32).as_CFType(),
        ));
    }
    if let Some(p) = filter.pid {
        pairs.push((
            CFString::from_static_string(KEY_PRODUCT_ID),
            CFNumber::from(p as i32).as_CFType(),
        ));
    }
    CFDictionary::from_CFType_pairs(&pairs)
}

fn read_string_property(device: IOHIDDeviceRef, key: &str) -> Option<String> {
    let cfkey = CFString::new(key);
    let raw = unsafe { IOHIDDeviceGetProperty(device, cfkey.as_concrete_TypeRef() as *const _) };
    if raw.is_null() {
        return None;
    }
    let cfs: CFString = unsafe { CFString::wrap_under_get_rule(raw as *const _) };
    Some(cfs.to_string())
}

fn read_data_property(device: IOHIDDeviceRef, key: &str) -> Option<Vec<u8>> {
    let cfkey = CFString::new(key);
    let raw = unsafe { IOHIDDeviceGetProperty(device, cfkey.as_concrete_TypeRef() as *const _) };
    if raw.is_null() {
        return None;
    }
    let cfd: CFData = unsafe { CFData::wrap_under_get_rule(raw as *const _) };
    Some(cfd.bytes().to_vec())
}

fn read_number_property(device: IOHIDDeviceRef, key: &str) -> Option<i32> {
    let cfkey = CFString::new(key);
    let raw = unsafe { IOHIDDeviceGetProperty(device, cfkey.as_concrete_TypeRef() as *const _) };
    if raw.is_null() {
        return None;
    }
    let n: CFNumber = unsafe { CFNumber::wrap_under_get_rule(raw as *const _) };
    n.to_i32()
}
