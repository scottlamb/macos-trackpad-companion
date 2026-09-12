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

const MAX_REPORT_BYTES: usize = 64 * 1024;
const MAX_REPORT_BITS: usize = MAX_REPORT_BYTES * 8;

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

const USAGE_DIG_INPUT_MODE: u16 = 0x52;
const USAGE_DIG_SURFACE_SWITCH: u16 = 0x57;
const USAGE_DIG_BUTTON_SWITCH: u16 = 0x58;
const USAGE_DIG_LATENCY_MODE: u16 = 0x60;

const FINGER_USAGE: u32 = ((PAGE_DIGITIZER as u32) << 16) | (USAGE_DIG_FINGER as u32);

/// A field's position within one contact, in bits relative to the start
/// of that contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitField {
    pub offset: usize,
    pub size: usize,
}

/// Where each per-contact field lives.
///
/// Recorded from the descriptor rather than assumed, so a device that
/// reports extra per-contact data (Width, Height, Pressure, Azimuth —
/// all optional in the PTP spec) is handled by reading past it instead
/// of being rejected for having an unfamiliar stride.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContactFields {
    /// Optional in the spec; treated as "confident" when absent.
    pub confidence: Option<BitField>,
    pub tip: BitField,
    pub id: BitField,
    pub x: BitField,
    pub y: BitField,
}

#[derive(Debug, Clone)]
pub struct Layout {
    pub report_id: u8,
    pub contact_slots: usize,
    /// Contact stride in bytes. Derived from `contact_stride_bits`, and
    /// kept because it reads better in logs than a bit count.
    pub bytes_per_contact: usize,
    pub fingers_offset: usize,
    /// Bit position of the first contact, and the stride between them.
    /// These drive decoding; the byte-granular fields above are for
    /// display.
    pub fingers_bit_offset: usize,
    pub contact_stride_bits: usize,
    pub contact: ContactFields,
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

    // PTP Extensions
    pub input_mode_report_id: Option<u8>,
    pub selective_reporting_report_id: Option<u8>,
    pub latency_mode_report_id: Option<u8>,
    /// Report IDs of feature reports declared on a vendor-defined usage
    /// page (0xFF00 and above).
    pub vendor_feature_report_ids: Vec<u8>,
    /// Report IDs of feature reports declared on a standard usage page.
    ///
    /// Together these say whether writing a given feature report would
    /// be addressing something the device declared for another purpose.
    pub standard_feature_report_ids: Vec<u8>,
}

impl Layout {
    /// Whether probing feature report `id` with vendor semantics would
    /// be writing over a report this device declared for something else.
    ///
    /// The RMK firmware answers report 0x10 without declaring it, so
    /// "not declared" must stay probeable or its heartbeat path is
    /// lost. What must not happen is writing a vendor-defined byte into
    /// a report the device declared on a *standard* page, where it
    /// means something specific and certainly not ours — SET_FEATURE
    /// succeeding tells us only that the bytes were accepted.
    pub fn vendor_probe_would_collide(&self, id: u8) -> bool {
        self.standard_feature_report_ids.contains(&id)
            && !self.vendor_feature_report_ids.contains(&id)
    }

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
        if self.contact_stride_bits == 0 {
            bail!("contact stride is zero");
        }
        // A sanity bound, not a compatibility rule: a plausible contact
        // is a few bytes, and anything past 32 means the descriptor was
        // misread rather than that the device is exotic.
        if self.contact_stride_bits > 32 * 8 {
            bail!(
                "implausible contact stride: {} bits",
                self.contact_stride_bits
            );
        }

        if self.report_id == 0
            || self.total_payload_bytes == 0
            || self.total_payload_bytes > MAX_REPORT_BYTES
        {
            bail!("unsupported report ID or payload size");
        }
        if self.contact_slots == 0 || self.contact_slots > 256 || self.fingers_bit_offset < 8 {
            bail!("invalid contact count or contact offset");
        }
        let c = &self.contact;
        for (field, max_size) in [(c.tip, 1), (c.id, 8), (c.x, 32), (c.y, 32)]
            .into_iter()
            .chain(c.confidence.map(|field| (field, 1)))
        {
            if field.size == 0
                || field.size > max_size
                || field
                    .offset
                    .checked_add(field.size)
                    .is_none_or(|end| end > self.contact_stride_bits)
            {
                bail!("invalid contact field width or field outside stride");
            }
        }
        let contact_end = self
            .contact_slots
            .checked_mul(self.contact_stride_bits)
            .and_then(|size| self.fingers_bit_offset.checked_add(size));
        if contact_end.is_none_or(|end| end > self.total_payload_bytes * 8) {
            bail!("contacts run past the report payload");
        }
        for (offset, width) in [
            (self.scan_time_offset, 2),
            (self.contact_count_offset, 1),
            (self.button_offset, 1),
        ] {
            if offset == 0
                || offset
                    .checked_add(width)
                    .is_none_or(|end| end > self.total_payload_bytes)
            {
                bail!("trailing field outside report payload");
            }
        }
        if self.button_bit >= 8 {
            bail!("button bit outside its byte");
        }
        if self.logical_x_max <= 0
            || self.logical_y_max <= 0
            || !self.physical_x_max_mm.is_finite()
            || self.physical_x_max_mm <= 0.0
            || !self.physical_y_max_mm.is_finite()
            || self.physical_y_max_mm <= 0.0
        {
            bail!("invalid coordinate geometry");
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
    scan_time: HashMap<u8, FieldRef>,
    contact_count: HashMap<u8, FieldRef>,
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

    // PTP Extensions
    input_mode_report_id: Option<u8>,
    selective_reporting_report_id: Option<u8>,
    latency_mode_report_id: Option<u8>,
    vendor_feature_report_ids: Vec<u8>,
    standard_feature_report_ids: Vec<u8>,
}

#[derive(Debug)]
struct FingerBlockBuilder {
    report_id: u8,
    start_bit: usize,
    confidence: Option<BitField>,
    tip: Option<BitField>,
    id: Option<BitField>,
    x: Option<BitField>,
    y: Option<BitField>,
}

#[derive(Debug)]
struct FingerBlock {
    report_id: u8,
    start_bit: usize,
    end_bit: usize,
    fields: ContactFields,
}

#[derive(Debug, Clone, Copy)]
struct FieldRef {
    bit_size: usize,
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
            scan_time: HashMap::new(),
            contact_count: HashMap::new(),
            buttons: HashMap::new(),
            logical_x_max: None,
            logical_y_max: None,
            physical_x_max_mm: None,
            physical_y_max_mm: None,
            input_mode_report_id: None,
            selective_reporting_report_id: None,
            latency_mode_report_id: None,
            vendor_feature_report_ids: Vec::new(),
            standard_feature_report_ids: Vec::new(),
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
                if dsize > self.data.len() - self.pos - 2 {
                    bail!("long item data exceeds descriptor");
                }
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
                1 => self.handle_global(tag, udata, sdata)?,
                2 => self.handle_local(tag, udata),
                _ => {}
            }
        }
        Ok(())
    }

    fn handle_feature(&mut self) {
        let usages = self.expanded_usages(self.report_count as usize);

        for usage32 in usages {
            let page = (usage32 >> 16) as u16;
            let usage = (usage32 & 0xffff) as u16;

            // Vendor-defined pages start at 0xFF00. Note the report id
            // so callers can check a vendor report exists before
            // writing one.
            if page >= 0xFF00 {
                if !self.vendor_feature_report_ids.contains(&self.report_id) {
                    self.vendor_feature_report_ids.push(self.report_id);
                }
            } else if !self.standard_feature_report_ids.contains(&self.report_id) {
                self.standard_feature_report_ids.push(self.report_id);
            }

            if page != PAGE_DIGITIZER {
                continue;
            }

            match usage {
                USAGE_DIG_INPUT_MODE => {
                    self.input_mode_report_id.get_or_insert(self.report_id);
                }

                USAGE_DIG_SURFACE_SWITCH | USAGE_DIG_BUTTON_SWITCH => {
                    self.selective_reporting_report_id
                        .get_or_insert(self.report_id);
                }

                USAGE_DIG_LATENCY_MODE => {
                    self.latency_mode_report_id.get_or_insert(self.report_id);
                }

                _ => {}
            }
        }
    }

    fn handle_main(&mut self, tag: u8, udata: u32) -> Result<()> {
        if matches!(tag, 0b1000 | 0b1001 | 0b1011) {
            let bits = self
                .report_size
                .checked_mul(self.report_count)
                .ok_or_else(|| anyhow!("report size/count overflow"))?;
            if bits as usize > MAX_REPORT_BITS
                || self.report_count as usize > MAX_REPORT_BITS
                || (self.report_count != 0 && self.report_size == 0)
            {
                bail!("implausible report size/count");
            }
        }
        match tag {
            0b1000 => self.handle_input(udata)?,
            0b1011 => self.handle_feature(),
            0b1010 => self.open_collection(udata)?,
            0b1100 => self.close_collection(),
            _ => {}
        }

        self.usages.clear();
        self.usage_min = None;
        self.usage_max = None;

        Ok(())
    }

    fn open_collection(&mut self, udata: u32) -> Result<()> {
        let kind = (udata & 0xFF) as u8;
        let primary_usage = self
            .usages
            .first()
            .copied()
            .unwrap_or((self.usage_page as u32) << 16);

        self.collections.push(Collection {
            kind,
            primary_usage,
        });

        if kind == 0x02 && primary_usage == FINGER_USAGE {
            if self.current_finger_block.is_some() {
                bail!("nested finger collections");
            }
            let cursor = *self.bit_cursor.entry(self.report_id).or_insert(8);
            self.current_finger_block = Some(FingerBlockBuilder {
                report_id: self.report_id,
                start_bit: cursor,
                confidence: None,
                tip: None,
                id: None,
                x: None,
                y: None,
            });
        }
        Ok(())
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
        if let (Some(tip), Some(id), Some(x), Some(y)) =
            (builder.tip, builder.id, builder.x, builder.y)
        {
            self.finger_blocks.push(FingerBlock {
                report_id: builder.report_id,
                start_bit: builder.start_bit,
                end_bit,
                fields: ContactFields {
                    confidence: builder.confidence,
                    tip,
                    id,
                    x,
                    y,
                },
            });
            self.touch_report_id.get_or_insert(self.report_id);
        }
    }

    fn handle_input(&mut self, flags: u32) -> Result<()> {
        let constant = (flags & 0x01) != 0;
        let bit_size = self.report_size;
        let count = self.report_count;
        let total_bits = (bit_size * count) as usize;

        let cursor_initial = if self.report_id != 0 { 8 } else { 0 };
        let cursor = self
            .bit_cursor
            .entry(self.report_id)
            .or_insert(cursor_initial);
        let start_bit = *cursor;
        *cursor = cursor
            .checked_add(total_bits)
            .filter(|end| *end <= MAX_REPORT_BITS)
            .ok_or_else(|| anyhow!("input report exceeds size limit"))?;

        if constant {
            return Ok(());
        }

        let usages = self.expanded_usages(count as usize);
        for (i, usage32) in usages.into_iter().enumerate() {
            let page = (usage32 >> 16) as u16;
            let usage = (usage32 & 0xFFFF) as u16;
            let field_bit_offset = start_bit + (i * bit_size as usize);
            let field = FieldRef {
                bit_size: bit_size as usize,
                bit_offset: field_bit_offset,
            };

            match (page, usage) {
                (PAGE_GENERIC_DESKTOP, USAGE_GD_X) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.x.get_or_insert(BitField {
                            offset: field_bit_offset - b.start_bit,
                            size: bit_size as usize,
                        });
                        if self.logical_x_max.is_none() {
                            self.logical_x_max = Some(self.logical_max);
                            self.physical_x_max_mm =
                                physical_to_mm(self.physical_max, self.unit, self.unit_exponent);
                        }
                    }
                }
                (PAGE_GENERIC_DESKTOP, USAGE_GD_Y) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.y.get_or_insert(BitField {
                            offset: field_bit_offset - b.start_bit,
                            size: bit_size as usize,
                        });
                        if self.logical_y_max.is_none() {
                            self.logical_y_max = Some(self.logical_max);
                            self.physical_y_max_mm =
                                physical_to_mm(self.physical_max, self.unit, self.unit_exponent);
                        }
                    }
                }
                (PAGE_DIGITIZER, USAGE_DIG_TIP_SWITCH) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.tip.get_or_insert(BitField {
                            offset: field_bit_offset - b.start_bit,
                            size: bit_size as usize,
                        });
                    }
                }
                (PAGE_DIGITIZER, USAGE_DIG_CONFIDENCE) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.confidence.get_or_insert(BitField {
                            offset: field_bit_offset - b.start_bit,
                            size: bit_size as usize,
                        });
                    }
                }
                (PAGE_DIGITIZER, USAGE_DIG_CONTACT_ID) => {
                    if let Some(b) = self.current_finger_block.as_mut() {
                        b.id.get_or_insert(BitField {
                            offset: field_bit_offset - b.start_bit,
                            size: bit_size as usize,
                        });
                    }
                }
                (PAGE_DIGITIZER, USAGE_DIG_SCAN_TIME) => {
                    self.scan_time.entry(self.report_id).or_insert(field);
                }
                (PAGE_DIGITIZER, USAGE_DIG_CONTACT_COUNT) => {
                    self.contact_count.entry(self.report_id).or_insert(field);
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
        Ok(())
    }

    fn handle_global(&mut self, tag: u8, udata: u32, sdata: i32) -> Result<()> {
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
                let id = u8::try_from(udata).map_err(|_| anyhow!("report ID exceeds one byte"))?;
                if id == 0 {
                    bail!("report ID zero is reserved");
                }
                if self
                    .current_finger_block
                    .as_ref()
                    .is_some_and(|b| b.report_id != id)
                {
                    bail!("report ID changed within a finger collection");
                }
                self.report_id = id;
                let initial = if id != 0 { 8 } else { 0 };
                self.bit_cursor.entry(id).or_insert(initial);
            }
            9 => self.report_count = udata,
            _ => {}
        }
        Ok(())
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
        if count == 0 {
            return Vec::new();
        }
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
            for u in (lo..=hi).take(count) {
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
        let contact_stride_bits = first
            .end_bit
            .checked_sub(first.start_bit)
            .ok_or_else(|| anyhow!("finger ends before it starts"))?;
        for (i, block) in self.finger_blocks.iter().enumerate() {
            let expected_start = i
                .checked_mul(contact_stride_bits)
                .and_then(|offset| first.start_bit.checked_add(offset));
            if block.report_id != report_id
                || block.fields != first.fields
                || Some(block.start_bit) != expected_start
                || block.end_bit.checked_sub(block.start_bit) != Some(contact_stride_bits)
            {
                bail!("inconsistent contact layouts or report IDs");
            }
        }
        let bytes_per_contact = contact_stride_bits / 8;
        let fingers_offset = first.start_bit / 8;
        let fingers_bit_offset = first.start_bit;
        let contact = first.fields;

        let scan_time = self
            .scan_time
            .get(&report_id)
            .ok_or_else(|| anyhow!("descriptor missing Scan Time field"))?;
        let contact_count = self
            .contact_count
            .get(&report_id)
            .ok_or_else(|| anyhow!("descriptor missing Contact Count field"))?;
        let button = self.buttons.get(&report_id).copied().ok_or_else(|| {
            anyhow!("descriptor missing Button 1 field in touch report {report_id:#04x}")
        })?;

        if scan_time.bit_size != 16
            || scan_time.bit_offset % 8 != 0
            || contact_count.bit_size != 8
            || contact_count.bit_offset % 8 != 0
            || button.bit_size != 1
        {
            bail!("unsupported scan time, contact count or button packing");
        }

        let total_bits = self.bit_cursor.get(&report_id).copied().unwrap_or(0);

        let physical_x_max_mm = self.physical_x_max_mm.ok_or_else(|| {
            anyhow!("descriptor missing Physical Max + Unit (cm/in length) for X")
        })?;
        let physical_y_max_mm = self.physical_y_max_mm.ok_or_else(|| {
            anyhow!("descriptor missing Physical Max + Unit (cm/in length) for Y")
        })?;
        let layout = Layout {
            report_id,
            contact_slots: self.finger_blocks.len(),
            bytes_per_contact,
            fingers_offset,
            fingers_bit_offset,
            contact_stride_bits,
            contact,
            scan_time_offset: scan_time.bit_offset / 8,
            contact_count_offset: contact_count.bit_offset / 8,
            button_offset: button.bit_offset / 8,
            button_bit: (button.bit_offset % 8) as u8,
            logical_x_max: self.logical_x_max.unwrap_or(1),
            logical_y_max: self.logical_y_max.unwrap_or(1),
            physical_x_max_mm,
            physical_y_max_mm,
            total_payload_bytes: total_bits.div_ceil(8),
            input_mode_report_id: self.input_mode_report_id,
            selective_reporting_report_id: self.selective_reporting_report_id,
            latency_mode_report_id: self.latency_mode_report_id,
            vendor_feature_report_ids: self.vendor_feature_report_ids.clone(),
            standard_feature_report_ids: self.standard_feature_report_ids.clone(),
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
    let length_exp = if length_nib & 0x8 != 0 {
        length_nib - 16
    } else {
        length_nib
    };
    if length_exp != 1 {
        return None;
    }
    let scale_to_mm = match system {
        1 => 10.0, // SI Linear: cm → mm
        3 => 25.4, // English Linear: in → mm
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

    #[test]
    fn trailing_fields_belong_to_the_touch_report_in_composite_descriptors() {
        // Sibling report puts Scan Time and Contact Count well beyond the
        // end of the touch report. Neither order may contaminate its layout.
        let sibling = from_hex("050d0905a10185027508954081037510950109568102750809548102c0");
        let touch = wpt_descriptor_5_contacts();
        for sibling_first in [false, true] {
            let desc = if sibling_first {
                [sibling.clone(), touch.clone()].concat()
            } else {
                [touch.clone(), sibling.clone()].concat()
            };
            let layout = parse(&desc).expect("valid composite descriptor");
            assert_eq!(layout.scan_time_offset, 31);
            assert_eq!(layout.contact_count_offset, 33);
            let mut report = vec![0; layout.total_payload_bytes];
            report[0] = layout.report_id;
            report[31..33].copy_from_slice(&0x1234u16.to_le_bytes());
            assert_eq!(
                crate::report::decode(&layout, &report)
                    .unwrap()
                    .scan_time_100us,
                0x1234
            );
        }
    }

    #[test]
    fn malformed_descriptor_arithmetic_and_field_packing_are_rejected() {
        // A 32-bit size/count product must not overflow in debug or wrap
        // in release. Large but non-overflowing reports are bounded too.
        for bytes in [
            "77ffffffff95028102",
            "752097ffffffff8102",
            "750095018102",
            "750897000001008102",
            "fe030001",
        ] {
            assert!(parse(&from_hex(bytes)).is_err(), "accepted {bytes}");
        }
        let good = wpt_descriptor_5_contacts();
        for (needle, replacement) in [
            (&[0x75, 0x10, 0x95, 0x01, 0x09, 0x56][..], 8), // Scan Time needs 16 bits.
            (&[0x75, 0x08, 0x81, 0x02, 0x05, 0x09][..], 16), // Count needs 8 bits.
            (&[0x75, 0x01, 0x95, 0x01, 0x81, 0x02][..], 2), // Button needs 1 bit.
        ] {
            let mut desc = good.clone();
            let pos = desc
                .windows(needle.len())
                .position(|w| w == needle)
                .unwrap();
            desc[pos + 1] = replacement;
            assert!(parse(&desc).is_err());
        }
        // Invalid Report ID values must not truncate or alias another ID.
        for id in [0u16, 256] {
            let mut desc = good.clone();
            desc.splice(6..8, [0x86, id as u8, (id >> 8) as u8]);
            assert!(parse(&desc).is_err());
        }
    }

    #[test]
    fn truncated_and_mutated_descriptors_never_produce_unsafe_layouts() {
        // Exercise every truncation and sample byte mutations throughout a
        // real descriptor. Accepted variants must remain safe to decode at the
        // exact reported boundary, including maximal contact count.
        let desc = from_hex(THIRD_PARTY_PTP_DESCRIPTOR);
        let check = |bytes: &[u8]| {
            if let Ok(layout) = parse(bytes) {
                let mut report = vec![0xff; layout.total_payload_bytes];
                report[0] = layout.report_id;
                assert!(crate::report::decode(&layout, &report).is_some());
                assert!(crate::report::decode(&layout, &report[..report.len() - 1]).is_none());
            }
        };
        for end in 0..=desc.len() {
            check(&desc[..end]);
        }
        for pos in 0..desc.len() {
            for value in [0, 1, 0x7f, 0xff] {
                let mut mutated = desc.clone();
                mutated[pos] = value;
                check(&mutated);
            }
        }
    }

    #[test]
    fn a_zero_report_count_does_not_expand_a_usage_range() {
        let mut walker = Walker::new(&[]);
        walker.usage_min = Some(1);
        walker.usage_max = Some(8);
        assert!(walker.expanded_usages(0).is_empty());
    }

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

    fn layout_with(vendor: &[u8], standard: &[u8]) -> Layout {
        Layout {
            report_id: 1,
            contact_slots: 5,
            bytes_per_contact: 6,
            fingers_offset: 1,
            fingers_bit_offset: 8,
            contact_stride_bits: 48,
            contact: ContactFields {
                confidence: Some(BitField { offset: 0, size: 1 }),
                tip: BitField { offset: 1, size: 1 },
                id: BitField { offset: 8, size: 8 },
                x: BitField {
                    offset: 16,
                    size: 16,
                },
                y: BitField {
                    offset: 32,
                    size: 16,
                },
            },
            scan_time_offset: 31,
            contact_count_offset: 33,
            button_offset: 34,
            button_bit: 0,
            logical_x_max: 100,
            logical_y_max: 100,
            physical_x_max_mm: 10.0,
            physical_y_max_mm: 10.0,
            total_payload_bytes: 35,
            input_mode_report_id: None,
            selective_reporting_report_id: None,
            latency_mode_report_id: None,
            vendor_feature_report_ids: vendor.to_vec(),
            standard_feature_report_ids: standard.to_vec(),
        }
    }

    #[test]
    fn undeclared_report_stays_probeable() {
        // The RMK firmware answers 0x10 without declaring it; losing
        // that would cost the heartbeat path.
        let l = layout_with(&[], &[0x25, 0x22]);
        assert!(!l.vendor_probe_would_collide(0x10));
    }

    #[test]
    fn vendor_declared_report_is_probeable() {
        let l = layout_with(&[0x10], &[0x25]);
        assert!(!l.vendor_probe_would_collide(0x10));
    }

    #[test]
    fn report_declared_on_a_standard_page_is_not_ours_to_write() {
        let l = layout_with(&[], &[0x10, 0x25]);
        assert!(l.vendor_probe_would_collide(0x10));
    }

    /// Real descriptor from a third-party PTP trackpad (vid 0x258a,
    /// pid 0x0010), captured with `--dump-descriptors`.
    ///
    /// Kept verbatim because a descriptor is the entire compatibility
    /// contract: this pad can be regression-tested forever without
    /// anyone owning one. Notably it uses a 5-byte contact stride and
    /// report id 0x1e, neither of which matches the reference firmware.
    const THIRD_PARTY_PTP_DESCRIPTOR: &str = concat!(
        "0601000980a10185022501150075010a81000a82000a83009503810695058101",
        "c0060c000901a10185032501150075010ab5000ab6000a6f000a70000ae2000a",
        "30000ae9000aea00950881020a83010a94010aae010a88010a8a010a92010ab7",
        "000acd00950881020a21020a40000a24020a25020a26020a27020a2a020a9601",
        "950881020aa8020a84010ab1010a82010aae010a30000a07030a010395088102",
        "c00600ff0901a1018505150026ff001901290575089504b102c00600ff0901a1",
        "018506150025ff1a01002a0f047508960f04b102c0050d0905a101851e050d09",
        "22a102150025010947094295027501810295017506253f095181020501150026",
        "70087510550e651309303500463a039501810246d50126400609318102c0050d",
        "0922a102150025010947094295027501810295017506253f0951810205011500",
        "2670087510550e651309303500463a039501810246d50126400609318102c005",
        "0d0922a102150025010947094295027501810295017506253f09518102050115",
        "002670087510550e651309303500463a039501810246d50126400609318102c0",
        "050d0922a102150025010947094295027501810295017506253f095181020501",
        "15002670087510550e651309303500463a039501810246d50126400609318102",
        "c0050d0922a102150025010947094295027501810295017506253f0951810205",
        "0115002670087510550e651309303500463a039501810246d501264006093181",
        "02c0050d550c66011047ffff000027ffff000075109501095681020954257f95",
        "01750881020509090109020903250175019503810295058103050d851f095509",
        "5975049502250fb102852309607501950115002501b1029507b10385200600ff",
        "09c5150026ff007508960001b102c0050d090ea10185250922a1020952150025",
        "0a75089501b102c00922a100852209570958750195022501b1029506b103c0c0",
        "0600ff0901a1018512150026ff001901290775089507b102c006c0ff0901a101",
        "8513150026ff001a0100292175089521b102c0",
    );

    fn from_hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
            .collect()
    }

    #[test]
    fn parses_a_real_third_party_ptp_descriptor() {
        let desc = from_hex(THIRD_PARTY_PTP_DESCRIPTOR);
        assert_eq!(desc.len(), 755);
        let layout = parse(&desc).expect("parse");

        assert_eq!(layout.report_id, 0x1e);
        assert_eq!(layout.contact_slots, 5);
        assert_eq!(layout.bytes_per_contact, 5);
        assert_eq!(layout.total_payload_bytes, 30);
        assert_eq!(layout.logical_x_max, 2160);
        assert_eq!(layout.logical_y_max, 1600);
        assert!((layout.physical_x_max_mm - 209.8).abs() < 0.1);
        assert!((layout.physical_y_max_mm - 119.1).abs() < 0.1);

        // Discovered, not assumed: this device's Input Mode is 0x25.
        assert_eq!(layout.input_mode_report_id, Some(0x25));
        assert_eq!(layout.selective_reporting_report_id, Some(0x22));
        assert_eq!(layout.latency_mode_report_id, Some(0x23));

        // It declares no report 0x10 at all, so the vendor probe stays
        // available — the case that must not regress for RMK firmware.
        assert!(!layout.vendor_probe_would_collide(0x10));
    }

    /// A synthetic pad that reports Width and Height per contact, on top
    /// of the required fields — both optional in the PTP spec and common
    /// on real hardware.
    ///
    /// Its 10-byte stride was rejected outright until the layout was
    /// described by per-field positions: `validate` allowed only 5 or 6
    /// bytes per contact, so any device carrying extra per-contact data
    /// was refused despite being perfectly parseable.
    const CONTACT_WITH_WIDTH_HEIGHT: &str = concat!(
        "050d0905a10185010922a1021500250175019502094709428102950681037508",
        "9501267f000951810205012670087510550e651309303500463a039501810246",
        "d50126400609318102050d5500650026ff0f0948810209498102c00956751095",
        "0127ffff0000810209547508257f810205090901750195012501810295078103",
        "c0",
    );

    #[test]
    fn accepts_contacts_carrying_extra_fields() {
        let desc = from_hex(CONTACT_WITH_WIDTH_HEIGHT);
        let layout = parse(&desc).expect("a 10-byte contact stride must parse");

        assert_eq!(layout.bytes_per_contact, 10);
        assert_eq!(layout.contact_stride_bits, 80);

        // The fields we need are found by usage, wherever they sit; the
        // Width and Height between them are simply skipped.
        assert_eq!(
            layout.contact.confidence,
            Some(BitField { offset: 0, size: 1 })
        );
        assert_eq!(layout.contact.tip, BitField { offset: 1, size: 1 });
        assert_eq!(layout.contact.id, BitField { offset: 8, size: 8 });
        assert_eq!(
            layout.contact.x,
            BitField {
                offset: 16,
                size: 16
            }
        );
        assert_eq!(
            layout.contact.y,
            BitField {
                offset: 32,
                size: 16
            }
        );
    }

    #[test]
    fn decodes_a_contact_with_extra_fields() {
        let desc = from_hex(CONTACT_WITH_WIDTH_HEIGHT);
        let layout = parse(&desc).expect("parse");

        // report id, flags, id, X, Y, width, height, scan time, count, button
        let mut report = vec![0u8; layout.total_payload_bytes];
        report[0] = 0x01;
        report[1] = 0b0000_0011; // confidence + tip
        report[2] = 7; // contact id
        report[3..5].copy_from_slice(&1080u16.to_le_bytes()); // X
        report[5..7].copy_from_slice(&800u16.to_le_bytes()); // Y
        report[7..9].copy_from_slice(&1234u16.to_le_bytes()); // width, ignored
        report[9..11].copy_from_slice(&5678u16.to_le_bytes()); // height, ignored
        report[layout.contact_count_offset] = 1;

        let frame = crate::report::decode(&layout, &report).expect("decode");
        assert_eq!(frame.contacts.len(), 1);
        let c = &frame.contacts[0];
        assert_eq!(c.id, 7);
        assert!(c.tip && c.confidence);
        // Half the pad across, in millimetres.
        assert!((c.x - 104.9).abs() < 0.5, "x was {}", c.x);
        assert!((c.y - 59.5).abs() < 0.5, "y was {}", c.y);
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
            0x05, 0x01, // Usage Page (Generic Desktop)
            0x09, 0x02, // Usage (Mouse)
            0xA1, 0x01, // Collection (Application)
            0x85, 0x01, //   Report ID (1)
            0x09, 0x01, //   Usage (Pointer)
            0xA1, 0x00, //   Collection (Physical)
            0x05, 0x09, 0x19, 0x01, 0x29, 0x02, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02,
            0x81, 0x02, // 2 buttons (1 bit each)
            0x95, 0x06, 0x81, 0x03, // 6 bits padding
            0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x02,
            0x81, 0x06, // 2x 8-bit X/Y deltas
            0xC0, 0xC0,
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

    #[test]
    fn discovers_standard_ptp_feature_report_ids() {
        let mut desc = wpt_descriptor_with_mouse_tlc();

        // Standard Precision Touchpad configuration features.
        //
        // Report 0x25: Input Mode (Digitizer Usage 0x52)
        // Report 0x22: Selective Reporting
        //              Surface Switch 0x57 + Button Switch 0x58
        // Report 0x23: Latency Mode (Digitizer Usage 0x60)
        desc.extend_from_slice(&[
            // Usage Page (Digitizer)
            0x05, 0x0d, // Configuration collection
            0x09, 0x0e, 0xa1, 0x01, // Input Mode -- Report ID 0x25
            0x85, 0x25, 0x09, 0x52, 0x15, 0x00, 0x25, 0x0a, 0x75, 0x08, 0x95, 0x01, 0xb1, 0x02,
            // Selective Reporting -- Report ID 0x22
            0x85, 0x22, 0x09, 0x57, 0x09, 0x58, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x02,
            0xb1, 0x02, // 6 bits padding
            0x95, 0x06, 0xb1, 0x03, // Latency Mode -- Report ID 0x23
            0x85, 0x23, 0x09, 0x60, 0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x01, 0xb1, 0x02,
            // 7 bits padding
            0x95, 0x07, 0xb1, 0x03, // End Configuration collection
            0xc0,
        ]);

        let layout = parse(&desc).expect("descriptor should parse");

        assert_eq!(layout.input_mode_report_id, Some(0x25));
        assert_eq!(layout.selective_reporting_report_id, Some(0x22));
        assert_eq!(layout.latency_mode_report_id, Some(0x23));
    }
}
