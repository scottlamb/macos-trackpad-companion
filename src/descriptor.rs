//! Walk a HID report descriptor enough to extract the geometry of a PTP
//! touch report (ID 0x01): number of contact slots, logical X/Y max,
//! and the byte offsets of the per-contact array and the trailing
//! fields (scan time, contact count, button).
//!
//! Bit offsets are reported relative to the buffer macOS hands us via
//! `IOHIDDeviceRegisterInputReportCallback`, which on macOS *includes*
//! the report-ID byte at offset 0. Field byte offsets in the returned
//! [`Layout`] are therefore directly indexable into that buffer.

use anyhow::{Result, anyhow, bail};
use std::collections::HashMap;

const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;
const PAGE_DIGITIZER: u16 = 0x0D;

const USAGE_GD_X: u16 = 0x30;
const USAGE_GD_Y: u16 = 0x31;
const USAGE_DIG_FINGER: u16 = 0x22;
const USAGE_DIG_TIP_SWITCH: u16 = 0x42;
const USAGE_DIG_CONFIDENCE: u16 = 0x47;
const USAGE_DIG_CONTACT_ID: u16 = 0x51;
const USAGE_DIG_CONTACT_COUNT: u16 = 0x54;
const USAGE_DIG_SCAN_TIME: u16 = 0x56;

const FINGER_USAGE: u32 = ((PAGE_DIGITIZER as u32) << 16) | (USAGE_DIG_FINGER as u32);

#[derive(Debug, Clone)]
pub struct Layout {
    pub report_id: u8,
    pub contact_slots: usize,
    pub bytes_per_contact: usize,
    pub fingers_offset: usize,
    pub scan_time_offset: usize,
    pub contact_count_offset: usize,
    pub button_offset: usize,
    pub button_bit: u8,
    pub logical_x_max: i32,
    pub logical_y_max: i32,
    /// Physical pad width in millimeters, derived from the descriptor's
    /// Physical Maximum + Unit + Unit Exponent items for the X field.
    /// Required: `parse` rejects descriptors that omit physical units,
    /// since gesture thresholds and cursor sensitivity are expressed in
    /// mm and there's no sane fallback.
    pub physical_x_max_mm: f64,
    pub physical_y_max_mm: f64,
    pub total_payload_bytes: usize,
}

impl Layout {
    /// Conversion factor from one chip-pixel of X displacement to
    /// millimeters. Density typically differs between axes
    /// (e.g. SoflePLUS2 IQS5xx panel: ~41.8 px/mm on X, ~47.3 px/mm on Y),
    /// so always scale per-axis when comparing distances.
    pub fn mm_per_logical_px_x(&self) -> f64 {
        self.physical_x_max_mm / self.logical_x_max.max(1) as f64
    }
    pub fn mm_per_logical_px_y(&self) -> f64 {
        self.physical_y_max_mm / self.logical_y_max.max(1) as f64
    }
}

impl Layout {
    pub fn validate(&self) -> Result<()> {
        if self.bytes_per_contact != 6 {
            bail!(
                "non-standard contact layout: {} bytes/contact (expected 6)",
                self.bytes_per_contact
            );
        }
        Ok(())
    }
}

pub fn parse(desc: &[u8]) -> Result<Layout> {
    let mut walker = Walker::new(desc);
    walker.walk()?;
    walker.into_layout()
}

#[derive(Debug)]
struct Walker<'a> {
    data: &'a [u8],
    pos: usize,

    usage_page: u16,
    logical_min: i32,
    logical_max: i32,
    physical_min: i32,
    physical_max: i32,
    /// HID Unit item (32-bit nibble-encoded). Nibble 0 is the unit
    /// system (1 = SI Linear → cm, 3 = English Linear → in); nibble 1
    /// is the length exponent (4-bit signed). Other nibbles are
    /// unused for X/Y length fields.
    unit: u32,
    /// HID Unit Exponent item: power of 10 applied to the on-wire
    /// physical value, 4-bit signed (raw 0..7 = 0..7, raw 8..F = -8..-1).
    unit_exponent: i32,
    report_size: u32,
    report_count: u32,
    report_id: u8,

    usages: Vec<u32>,
    usage_min: Option<u32>,
    usage_max: Option<u32>,

    collections: Vec<Collection>,

    /// Bit cursor per report ID, relative to the start of the on-wire
    /// buffer (which includes the report-ID byte at offset 0). Always
    /// starts at 8 for any non-zero report ID.
    bit_cursor: HashMap<u8, usize>,

    touch_report_id: Option<u8>,
    finger_blocks: Vec<FingerBlock>,
    current_finger_block: Option<FingerBlockBuilder>,
    scan_time: Option<FieldRef>,
    contact_count: Option<FieldRef>,
    /// Button 0x01 fields keyed by the report id they belong to. PTP
    /// descriptors commonly include a sibling Mouse TLC (e.g. Microsoft's
    /// reference, RMK's firmware) which also declares Button 0x01 in its
    /// own report — capturing the first such field across the whole
    /// descriptor would store a `bit_offset` valid only for the Mouse
    /// report, then apply it to the Touchpad report at decode time
    /// (where that offset typically lands inside finger 0's confidence
    /// bit). Resolution to a single field happens at `into_layout` once
    /// `touch_report_id` is known.
    buttons: HashMap<u8, FieldRef>,
    logical_x_max: Option<i32>,
    logical_y_max: Option<i32>,
    physical_x_max_mm: Option<f64>,
    physical_y_max_mm: Option<f64>,
}

#[derive(Debug)]
struct FingerBlockBuilder {
    start_bit: usize,
    has_tip: bool,
    has_id: bool,
    has_x: bool,
    has_y: bool,
}

#[derive(Debug)]
struct FingerBlock {
    start_bit: usize,
    end_bit: usize,
}

#[derive(Debug, Clone, Copy)]
struct FieldRef {
    bit_offset: usize,
}

#[derive(Debug, Clone, Copy)]
struct Collection {
    kind: u8,
    primary_usage: u32,
}

impl<'a> Walker<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            pos: 0,
            usage_page: 0,
            logical_min: 0,
            logical_max: 0,
            physical_min: 0,
            physical_max: 0,
            unit: 0,
            unit_exponent: 0,
            report_size: 0,
            report_count: 0,
            report_id: 0,
            usages: Vec::new(),
            usage_min: None,
            usage_max: None,
            collections: Vec::new(),
            bit_cursor: HashMap::new(),
            touch_report_id: None,
            finger_blocks: Vec::new(),
            current_finger_block: None,
            scan_time: None,
            contact_count: None,
            buttons: HashMap::new(),
            logical_x_max: None,
            logical_y_max: None,
            physical_x_max_mm: None,
            physical_y_max_mm: None,
        }
    }

    fn walk(&mut self) -> Result<()> {
        while self.pos < self.data.len() {
            let head = self.data[self.pos];
            self.pos += 1;

            // Long item form (rare, never used in PTP descriptors).
            if head == 0xFE {
                if self.pos + 1 >= self.data.len() {
                    bail!("truncated long item");
                }
                let dsize = self.data[self.pos] as usize;
                self.pos += 2 + dsize;
                continue;
            }

            let dsize = match head & 0b11 {
                0b00 => 0,
                0b01 => 1,
                0b10 => 2,
                _ => 4,
            };
            let kind = (head >> 2) & 0b11;
            let tag = (head >> 4) & 0b1111;

            if self.pos + dsize > self.data.len() {
                bail!("item data exceeds descriptor");
            }
            let raw = &self.data[self.pos..self.pos + dsize];
            self.pos += dsize;

            let udata = read_uint(raw);
            let sdata = read_sint(raw);

            match kind {
                0 => self.handle_main(tag, udata)?,
                1 => self.handle_global(tag, udata, sdata),
                2 => self.handle_local(tag, udata),
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_main(&mut self, tag: u8, udata: u32) -> Result<()> {
        match tag {
            0b1000 => self.handle_input(udata),
            0b1010 => self.open_collection(udata),
            0b1100 => self.close_collection(),
            // Output / Feature / others — we don't decode features at the
            // input-layout level, so ignore.
            _ => {}
        }
        // Local state resets after every Main item.
        self.usages.clear();
        self.usage_min = None;
        self.usage_max = None;
        Ok(())
    }

    fn open_collection(&mut self, udata: u32) {
        let kind = (udata & 0xFF) as u8;
        let primary_usage = self
            .usages
            .first()
            .copied()
            .unwrap_or(((self.usage_page as u32) << 16) | 0);

        self.collections.push(Collection { kind, primary_usage });

        if kind == 0x02 && primary_usage == FINGER_USAGE {
            let cursor = *self.bit_cursor.entry(self.report_id).or_insert(8);
            self.current_finger_block = Some(FingerBlockBuilder {
                start_bit: cursor,
                has_tip: false,
                has_id: false,
                has_x: false,
                has_y: false,
            });
        }
    }

    fn close_collection(&mut self) {
        let Some(popped) = self.collections.pop() else {
            return;
        };
        if !(popped.kind == 0x02 && popped.primary_usage == FINGER_USAGE) {
            return;
        }
        let Some(builder) = self.current_finger_block.take() else {
            return;
        };
        let end_bit = *self.bit_cursor.get(&self.report_id).unwrap_or(&0);
        if builder.has_tip && builder.has_id && builder.has_x && builder.has_y {
            self.finger_blocks.push(FingerBlock {
                start_bit: builder.start_bit,
                end_bit,
            });
            self.touch_report_id.get_or_insert(self.report_id);
        }
    }

    fn handle_input(&mut self, flags: u32) {
        let constant = (flags & 0x01) != 0;
        let bit_size = self.report_size;
        let count = self.report_count;
        let total_bits = (bit_size * count) as usize;

        let cursor_initial = if self.report_id != 0 { 8 } else { 0 };
        let cursor = self.bit_cursor.entry(self.report_id).or_insert(cursor_initial);
        let start_bit = *cursor;
        *cursor += total_bits;

        if constant {
            return;
        }

        let usages = self.expanded_usages(count as usize);
        for (i, usage32) in usages.into_iter().enumerate() {
            let page = (usage32 >> 16) as u16;
            let usage = (usage32 & 0xFFFF) as u16;
            let field_bit_offset = start_bit + (i * bit_size as usize);
            let field = FieldRef {
                bit_offset: field_bit_offset,
            };

            match (page, usage) {
                (PAGE_GENERIC_DESKTOP, USAGE_GD_X) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.has_x = true;
                        if self.logical_x_max.is_none() {
                            self.logical_x_max = Some(self.logical_max);
                            self.physical_x_max_mm =
                                physical_to_mm(self.physical_max, self.unit, self.unit_exponent);
                        }
                    }
                }
                (PAGE_GENERIC_DESKTOP, USAGE_GD_Y) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.has_y = true;
                        if self.logical_y_max.is_none() {
                            self.logical_y_max = Some(self.logical_max);
                            self.physical_y_max_mm =
                                physical_to_mm(self.physical_max, self.unit, self.unit_exponent);
                        }
                    }
                }
                (PAGE_DIGITIZER, USAGE_DIG_TIP_SWITCH) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.has_tip = true;
                    }
                }
                (PAGE_DIGITIZER, USAGE_DIG_CONFIDENCE) => {
                    // Useful but optional — track if we ever want to filter
                    // low-confidence contacts.
                }
                (PAGE_DIGITIZER, USAGE_DIG_CONTACT_ID) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.has_id = true;
                    }
                }
                (PAGE_DIGITIZER, USAGE_DIG_SCAN_TIME) => {
                    self.scan_time.get_or_insert(field);
                }
                (PAGE_DIGITIZER, USAGE_DIG_CONTACT_COUNT) => {
                    self.contact_count.get_or_insert(field);
                }
                (PAGE_BUTTON, 0x01) => {
                    // Record the field per-report-id; the touch report's
                    // entry wins at `into_layout`. Don't fold sibling
                    // Mouse-TLC buttons in here — their bit_offsets are
                    // valid only for the Mouse report payload.
                    self.buttons.entry(self.report_id).or_insert(field);
                }
                _ => {}
            }
        }
    }

    fn handle_global(&mut self, tag: u8, udata: u32, sdata: i32) {
        match tag {
            0 => self.usage_page = udata as u16,
            1 => self.logical_min = sdata,
            2 => self.logical_max = sdata,
            3 => self.physical_min = sdata,
            4 => self.physical_max = sdata,
            5 => {
                // Unit Exponent is 4-bit signed in the data's low nibble
                // (raw 0..7 = 0..7, raw 8..F = -8..-1). Higher bits of
                // the data field are unused.
                let nib = (udata & 0xF) as i32;
                self.unit_exponent = if nib & 0x8 != 0 { nib - 16 } else { nib };
            }
            6 => self.unit = udata,
            7 => self.report_size = udata,
            8 => {
                let id = udata as u8;
                self.report_id = id;
                let initial = if id != 0 { 8 } else { 0 };
                self.bit_cursor.entry(id).or_insert(initial);
            }
            9 => self.report_count = udata,
            _ => {}
        }
    }

    fn handle_local(&mut self, tag: u8, udata: u32) {
        match tag {
            0 => {
                let usage = if udata <= 0xFFFF {
                    ((self.usage_page as u32) << 16) | udata
                } else {
                    udata
                };
                self.usages.push(usage);
            }
            1 => self.usage_min = Some(udata),
            2 => self.usage_max = Some(udata),
            _ => {}
        }
    }

    fn expanded_usages(&self, count: usize) -> Vec<u32> {
        if !self.usages.is_empty() {
            let mut out = self.usages.clone();
            if out.len() < count {
                let last = *out.last().unwrap();
                while out.len() < count {
                    out.push(last);
                }
            }
            out.truncate(count);
            return out;
        }
        if let (Some(lo), Some(hi)) = (self.usage_min, self.usage_max) {
            let mut out = Vec::with_capacity(count);
            for u in lo..=hi {
                out.push(((self.usage_page as u32) << 16) | u);
                if out.len() == count {
                    break;
                }
            }
            while out.len() < count {
                let last = *out.last().unwrap_or(&0);
                out.push(last);
            }
            return out;
        }
        vec![0u32; count]
    }

    fn into_layout(self) -> Result<Layout> {
        let report_id = self
            .touch_report_id
            .ok_or_else(|| anyhow!("descriptor has no Digitizer/Finger collection"))?;
        let first = self
            .finger_blocks
            .first()
            .ok_or_else(|| anyhow!("finger collection lacked tip/id/X/Y"))?;
        let bytes_per_contact = (first.end_bit - first.start_bit) / 8;
        let fingers_offset = first.start_bit / 8;

        let scan_time = self
            .scan_time
            .ok_or_else(|| anyhow!("descriptor missing Scan Time field"))?;
        let contact_count = self
            .contact_count
            .ok_or_else(|| anyhow!("descriptor missing Contact Count field"))?;
        let button = self
            .buttons
            .get(&report_id)
            .copied()
            .ok_or_else(|| anyhow!("descriptor missing Button 1 field in touch report {report_id:#04x}"))?;

        let total_bits = self.bit_cursor.get(&report_id).copied().unwrap_or(0);

        let physical_x_max_mm = self
            .physical_x_max_mm
            .ok_or_else(|| anyhow!("descriptor missing Physical Max + Unit (cm/in length) for X"))?;
        let physical_y_max_mm = self
            .physical_y_max_mm
            .ok_or_else(|| anyhow!("descriptor missing Physical Max + Unit (cm/in length) for Y"))?;
        let layout = Layout {
            report_id,
            contact_slots: self.finger_blocks.len(),
            bytes_per_contact,
            fingers_offset,
            scan_time_offset: scan_time.bit_offset / 8,
            contact_count_offset: contact_count.bit_offset / 8,
            button_offset: button.bit_offset / 8,
            button_bit: (button.bit_offset % 8) as u8,
            logical_x_max: self.logical_x_max.unwrap_or(1),
            logical_y_max: self.logical_y_max.unwrap_or(1),
            physical_x_max_mm,
            physical_y_max_mm,
            total_payload_bytes: total_bits.div_ceil(8),
        };
        layout.validate()?;
        Ok(layout)
    }
}

/// Convert a Physical Maximum value to millimeters using the active
/// HID Unit and Unit Exponent. Returns `None` if the unit isn't a pure
/// length in a system we know how to scale (SI Linear → cm, English
/// Linear → in), or if the firmware never declared a Physical Maximum
/// (`physical == 0` and unit nibbles also zero, the post-reset default).
///
/// HID Unit encoding (Usage Tables §6.2.2.7): nibble 0 selects the unit
/// system (1 = SI Linear, 3 = English Linear), nibble 1 is the length
/// exponent (4-bit signed; we only handle ^1 — pure length, not area or
/// inverse length).
fn physical_to_mm(physical: i32, unit: u32, unit_exponent: i32) -> Option<f64> {
    if physical == 0 && unit == 0 {
        return None;
    }
    let system = unit & 0xF;
    let length_nib = ((unit >> 4) & 0xF) as i32;
    let length_exp = if length_nib & 0x8 != 0 { length_nib - 16 } else { length_nib };
    if length_exp != 1 {
        return None;
    }
    let scale_to_mm = match system {
        1 => 10.0,   // SI Linear: cm → mm
        3 => 25.4,   // English Linear: in → mm
        _ => return None,
    };
    Some((physical as f64) * 10f64.powi(unit_exponent) * scale_to_mm)
}

fn read_uint(bytes: &[u8]) -> u32 {
    let mut v: u32 = 0;
    for (i, b) in bytes.iter().enumerate() {
        v |= (*b as u32) << (8 * i);
    }
    v
}

fn read_sint(bytes: &[u8]) -> i32 {
    let v = read_uint(bytes) as i32;
    match bytes.len() {
        0 => 0,
        1 => v as i8 as i32,
        2 => v as i16 as i32,
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the descriptor the firmware at commit 7f3ee1c emits
    /// for the PTP digitizer interface (5 contacts, 65×40 mm, logical
    /// 3936×2424).
    fn wpt_descriptor_5_contacts() -> Vec<u8> {
        let mut d = vec![0x05, 0x0D, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x01];
        for _ in 0..5 {
            d.extend_from_slice(&[
                0x05, 0x0D, 0x09, 0x22, 0xA1, 0x02, 0x09, 0x47, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01,
                0x75, 0x01, 0x95, 0x02, 0x81, 0x02, 0x95, 0x06, 0x81, 0x03, 0x75, 0x08, 0x09, 0x51,
                0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x26, 0x60, 0x0F, 0x75, 0x10, 0x55, 0x0E, 0x65,
                0x11, 0x09, 0x30, 0x35, 0x00, 0x46, 0x8A, 0x02, 0x95, 0x01, 0x81, 0x02, 0x46, 0x90,
                0x01, 0x26, 0x78, 0x09, 0x09, 0x31, 0x81, 0x02, 0xC0,
            ]);
        }
        d.extend_from_slice(&[
            0x05, 0x0D, 0x55, 0x0C, 0x66, 0x01, 0x10, 0x47, 0xFF, 0xFF, 0x00, 0x00, 0x27, 0xFF,
            0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x09, 0x56, 0x81, 0x02, 0x09, 0x54, 0x25,
            0x7F, 0x95, 0x01, 0x75, 0x08, 0x81, 0x02, 0x05, 0x09, 0x09, 0x01, 0x25, 0x01, 0x75,
            0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0xC0,
        ]);
        d
    }

    #[test]
    fn parses_wpt_descriptor() {
        let desc = wpt_descriptor_5_contacts();
        let layout = parse(&desc).expect("parse");
        assert_eq!(layout.report_id, 0x01);
        assert_eq!(layout.contact_slots, 5);
        assert_eq!(layout.bytes_per_contact, 6);
        assert_eq!(layout.fingers_offset, 1);
        assert_eq!(layout.scan_time_offset, 31);
        assert_eq!(layout.contact_count_offset, 33);
        assert_eq!(layout.button_offset, 34);
        assert_eq!(layout.button_bit, 0);
        assert_eq!(layout.logical_x_max, 3936);
        assert_eq!(layout.logical_y_max, 2424);
        // Physical Max + Unit (SI cm) + Unit Exponent (-2): X Physical
        // Max = 0x028A (650) → 6.50 cm = 65.0 mm; Y Physical Max = 0x0190
        // (400) → 4.00 cm = 40.0 mm.
        assert!((layout.physical_x_max_mm - 65.0).abs() < 1e-6);
        assert!((layout.physical_y_max_mm - 40.0).abs() < 1e-6);
        assert_eq!(layout.total_payload_bytes, 35);
    }

    /// RMK's PTP firmware emits a sibling Mouse TLC (Report ID 0x01)
    /// that also declares Button 0x01 / 0x02, *before* the Touchpad TLC
    /// (Report ID 0x05). Earlier walker code stored the first Button 0x01
    /// it saw via `get_or_insert`, capturing the Mouse TLC's bit_offset
    /// (8, i.e. byte 1 bit 0 of the Mouse Report). At decode time that
    /// offset was applied to the Touchpad Report and read finger 0's
    /// confidence bit instead — every active touch decoded as
    /// `button=true`. Regression test: Touchpad button must be at byte
    /// 34 of the Touchpad report (after 5×6 fingers + scan_time +
    /// contact_count), bit 0.
    fn wpt_descriptor_with_mouse_tlc() -> Vec<u8> {
        // ===== Mouse TLC (Report ID 0x01) — declares Button 0x01..0x02 =====
        let mut d = vec![
            0x05, 0x01,                         // Usage Page (Generic Desktop)
            0x09, 0x02,                         // Usage (Mouse)
            0xA1, 0x01,                         // Collection (Application)
                0x85, 0x01,                         //   Report ID (1)
                0x09, 0x01,                         //   Usage (Pointer)
                0xA1, 0x00,                         //   Collection (Physical)
                    0x05, 0x09, 0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01,
                    0x75, 0x01, 0x95, 0x02, 0x81, 0x02,     // 2 buttons (1 bit each)
                    0x95, 0x06, 0x81, 0x03,                 // 6 bits padding
                    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F,
                    0x75, 0x08, 0x95, 0x02, 0x81, 0x06,     // 2x 8-bit X/Y deltas
                0xC0,
            0xC0,
        ];

        // ===== Touchpad TLC (Report ID 0x05) — five fingers + scan + count + button =====
        d.extend_from_slice(&[0x05, 0x0D, 0x09, 0x05, 0xA1, 0x01, 0x85, 0x05]);
        for _ in 0..5 {
            d.extend_from_slice(&[
                0x05, 0x0D, 0x09, 0x22, 0xA1, 0x02, 0x09, 0x47, 0x09, 0x42, 0x15, 0x00, 0x25, 0x01,
                0x75, 0x01, 0x95, 0x02, 0x81, 0x02, 0x95, 0x06, 0x81, 0x03, 0x75, 0x08, 0x09, 0x51,
                0x95, 0x01, 0x81, 0x02, 0x05, 0x01, 0x26, 0x60, 0x0F, 0x75, 0x10, 0x55, 0x0E, 0x65,
                0x11, 0x09, 0x30, 0x35, 0x00, 0x46, 0x8A, 0x02, 0x95, 0x01, 0x81, 0x02, 0x46, 0x90,
                0x01, 0x26, 0x78, 0x09, 0x09, 0x31, 0x81, 0x02, 0xC0,
            ]);
        }
        d.extend_from_slice(&[
            0x05, 0x0D, 0x55, 0x0C, 0x66, 0x01, 0x10, 0x47, 0xFF, 0xFF, 0x00, 0x00, 0x27, 0xFF,
            0xFF, 0x00, 0x00, 0x75, 0x10, 0x95, 0x01, 0x09, 0x56, 0x81, 0x02, 0x09, 0x54, 0x25,
            0x7F, 0x95, 0x01, 0x75, 0x08, 0x81, 0x02, 0x05, 0x09, 0x09, 0x01, 0x25, 0x01, 0x75,
            0x01, 0x95, 0x01, 0x81, 0x02, 0x95, 0x07, 0x81, 0x03, 0xC0,
        ]);
        d
    }

    #[test]
    fn touch_report_button_wins_over_sibling_mouse_button() {
        let desc = wpt_descriptor_with_mouse_tlc();
        let layout = parse(&desc).expect("parse");
        // We pick the Touchpad TLC (whose finger collections set
        // touch_report_id) as the report we decode, and its button —
        // not the Mouse TLC's earlier Button 0x01 — must populate
        // the layout.
        assert_eq!(layout.report_id, 0x05);
        assert_eq!(layout.button_offset, 34);
        assert_eq!(layout.button_bit, 0);
        // Sanity: still parses the rest correctly.
        assert_eq!(layout.contact_slots, 5);
        assert_eq!(layout.fingers_offset, 1);
        assert_eq!(layout.scan_time_offset, 31);
        assert_eq!(layout.contact_count_offset, 33);
    }
}
