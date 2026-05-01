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
use core_foundation::runloop::{CFRunLoop, CFRunLoopRun, kCFRunLoopDefaultMode};
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

        unsafe { CFRunLoopRun() };

        Ok(())
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
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
            log::warn!("matched \"{product}\" but descriptor parse failed: {e:#}");
            return;
        }
    };
    log::info!(
        "matched \"{product}\": {} contacts, logical {}×{}, payload {} bytes",
        layout.contact_slots,
        layout.logical_x_max,
        layout.logical_y_max,
        layout.total_payload_bytes
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

    bridge.devices.push(state);
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
    let Some(frame) = report::decode(&state.layout, bytes) else {
        return;
    };
    (bridge.on_frame)(frame);
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
