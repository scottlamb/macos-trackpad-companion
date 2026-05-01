//! Decode a PTP touch input report (ID 0x01) using a [`Layout`] from
//! [`crate::descriptor::parse`]. Coordinates are normalized to [0,1]
//! using the descriptor's logical max so downstream gesture code is
//! firmware-agnostic.

use crate::descriptor::Layout;

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
pub struct Contact {
    pub id: u8,
    /// Normalized X in [0.0, 1.0] (left → right).
    pub x: f64,
    /// Normalized Y in [0.0, 1.0] (top → bottom; PTP origin is top-left).
    pub y: f64,
    /// Raw chip-unit X straight from the report. Kept alongside the
    /// normalized value so logging / tests can see what the firmware
    /// actually emitted vs. what we computed; load-bearing for diagnosing
    /// descriptor logical-max mismatches (firmware says max=1023 but
    /// chip emits 0..2047 → normalized clamps and cursor stalls).
    pub raw_x: u16,
    pub raw_y: u16,
    pub tip: bool,
    pub confidence: bool,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Frame {
    pub contacts: Vec<Contact>,
    pub scan_time_100us: u16,
    pub button: bool,
}

pub fn decode(layout: &Layout, report: &[u8]) -> Option<Frame> {
    if report.len() < layout.total_payload_bytes {
        return None;
    }
    if report[0] != layout.report_id {
        return None;
    }

    let contact_count = report[layout.contact_count_offset] as usize;
    let n = contact_count.min(layout.contact_slots);

    let mut contacts = Vec::with_capacity(n);
    for i in 0..n {
        let off = layout.fingers_offset + i * layout.bytes_per_contact;
        if off + layout.bytes_per_contact > report.len() {
            break;
        }
        let flags = report[off];
        let id = report[off + 1];
        let x = u16::from_le_bytes([report[off + 2], report[off + 3]]) as i32;
        let y = u16::from_le_bytes([report[off + 4], report[off + 5]]) as i32;

        let confidence = (flags & 0x01) != 0;
        let tip = (flags & 0x02) != 0;

        let nx = (x as f64) / layout.logical_x_max.max(1) as f64;
        let ny = (y as f64) / layout.logical_y_max.max(1) as f64;

        contacts.push(Contact {
            id,
            x: nx.clamp(0.0, 1.0),
            y: ny.clamp(0.0, 1.0),
            raw_x: x as u16,
            raw_y: y as u16,
            tip,
            confidence,
        });
    }

    let scan_time = u16::from_le_bytes([
        report[layout.scan_time_offset],
        report[layout.scan_time_offset + 1],
    ]);
    let button =
        (report[layout.button_offset] & (1 << layout.button_bit)) != 0;

    Some(Frame {
        contacts,
        scan_time_100us: scan_time,
        button,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_layout() -> Layout {
        Layout {
            report_id: 0x01,
            contact_slots: 5,
            bytes_per_contact: 6,
            fingers_offset: 1,
            scan_time_offset: 31,
            contact_count_offset: 33,
            button_offset: 34,
            button_bit: 0,
            logical_x_max: 3936,
            logical_y_max: 2424,
            total_payload_bytes: 35,
        }
    }

    #[test]
    fn decodes_two_contacts() {
        let layout = fake_layout();
        let mut buf = vec![0u8; 35];
        buf[0] = 0x01;
        // Contact 0: tip=1, conf=1, id=7, x=1968, y=1212
        buf[1] = 0x03;
        buf[2] = 7;
        buf[3..5].copy_from_slice(&1968u16.to_le_bytes());
        buf[5..7].copy_from_slice(&1212u16.to_le_bytes());
        // Contact 1: tip=1, conf=1, id=8, x=2952, y=606
        buf[7] = 0x03;
        buf[8] = 8;
        buf[9..11].copy_from_slice(&2952u16.to_le_bytes());
        buf[11..13].copy_from_slice(&606u16.to_le_bytes());
        // scan_time = 0x1234, count=2, button=1
        buf[31..33].copy_from_slice(&0x1234u16.to_le_bytes());
        buf[33] = 2;
        buf[34] = 0x01;

        let frame = decode(&layout, &buf).expect("decode");
        assert_eq!(frame.contacts.len(), 2);
        assert_eq!(frame.contacts[0].id, 7);
        assert!((frame.contacts[0].x - 0.5).abs() < 0.001);
        assert!((frame.contacts[0].y - 0.5).abs() < 0.001);
        assert_eq!(frame.scan_time_100us, 0x1234);
        assert!(frame.button);
    }
}
