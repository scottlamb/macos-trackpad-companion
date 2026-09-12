//! Record a device's frame stream, and read it back.
//!
//! The point is to make a trackpad's *behaviour* portable. A descriptor
//! (see `--dump-descriptors`) is enough to test the parser, but nothing
//! covered the gesture engine against a real stream — which is what you
//! need to answer "why does this pad misclassify a pinch?" for hardware
//! you don't own. A capture is small, diffable, and can be replayed
//! offline through the same engine the daemon runs.
//!
//! The format is deliberately plain text rather than a serialisation
//! format: it survives being pasted into an issue, and a human can see
//! what the pad reported.
//!
//! ```text
//! # macos-trackpad-companion capture v1
//! # device PTP TouchPad vid=0x258a pid=0x0010
//! # pad 209.804x119.126
//! F <t_ns> <button> [<id>,<x_mm>,<y_mm>,<tip>,<confidence> ...]
//! ```

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::report::{Contact, Frame};
use crate::time::Timestamp;

const HEADER: &str = "# macos-trackpad-companion capture v1";

pub struct Writer {
    out: BufWriter<File>,
    wrote_pad: bool,
}

impl Writer {
    pub fn create(path: &Path) -> Result<Self> {
        let file =
            File::create(path).with_context(|| format!("create capture {}", path.display()))?;
        let mut out = BufWriter::new(file);
        writeln!(out, "{HEADER}")?;
        Ok(Self {
            out,
            wrote_pad: false,
        })
    }

    /// Record what device this came from. Called once, when a device is
    /// matched; a capture without it still replays, just without pad
    /// dimensions.
    pub fn device(&mut self, summary: &str, pad: Option<(f64, f64)>) -> Result<()> {
        writeln!(self.out, "# device {summary}")?;
        if let Some((w, h)) = pad {
            writeln!(self.out, "# pad {w:.3}x{h:.3}")?;
            self.wrote_pad = true;
        }
        Ok(())
    }

    pub fn frame(&mut self, frame: &Frame, ts: Timestamp) -> Result<()> {
        write!(self.out, "F {} {}", ts.as_nanos(), u8::from(frame.button))?;
        for c in &frame.contacts {
            write!(
                self.out,
                " {},{:.3},{:.3},{},{}",
                c.id,
                c.x,
                c.y,
                u8::from(c.tip),
                u8::from(c.confidence)
            )?;
        }
        writeln!(self.out)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        self.out.flush().context("flush capture")?;
        Ok(())
    }
}

pub struct Capture {
    pub device: Option<String>,
    pub pad: Option<(f64, f64)>,
    pub frames: Vec<(Timestamp, Frame)>,
}

pub fn read(path: &Path) -> Result<Capture> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut device = None;
    let mut pad = None;
    let mut frames = Vec::new();
    let mut saw_header = false;

    for (n, line) in BufReader::new(file).lines().enumerate() {
        let line = line.with_context(|| format!("read {}", path.display()))?;
        let lineno = n + 1;

        if line.starts_with(HEADER) {
            saw_header = true;
            continue;
        }
        if let Some(rest) = line.strip_prefix("# device ") {
            device = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("# pad ") {
            let (w, h) = rest
                .trim()
                .split_once('x')
                .with_context(|| format!("line {lineno}: malformed pad size"))?;
            pad = Some((w.trim().parse()?, h.trim().parse()?));
            continue;
        }
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }

        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("F") => {}
            Some(other) => bail!("line {lineno}: unknown record {other:?}"),
            None => continue,
        }
        let ts: u64 = parts
            .next()
            .with_context(|| format!("line {lineno}: missing timestamp"))?
            .parse()
            .with_context(|| format!("line {lineno}: bad timestamp"))?;
        let button = parts
            .next()
            .with_context(|| format!("line {lineno}: missing button"))?
            != "0";

        let mut contacts = Vec::new();
        for field in parts {
            let f: Vec<&str> = field.split(',').collect();
            if f.len() != 5 {
                bail!("line {lineno}: contact needs 5 fields, got {}", f.len());
            }
            contacts.push(Contact {
                id: f[0].parse().with_context(|| format!("line {lineno}: id"))?,
                x: f[1].parse().with_context(|| format!("line {lineno}: x"))?,
                y: f[2].parse().with_context(|| format!("line {lineno}: y"))?,
                tip: f[3] != "0",
                confidence: f[4] != "0",
            });
        }

        frames.push((
            Timestamp::from_nanos(ts),
            Frame {
                contacts,
                scan_time_100us: 0,
                button,
            },
        ));
    }

    if !saw_header {
        bail!("{} is not a capture file", path.display());
    }
    Ok(Capture {
        device,
        pad,
        frames,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_frame_stream() {
        let dir = std::env::temp_dir().join(format!("tpc-cap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.txt");

        let mut w = Writer::create(&path).unwrap();
        w.device("Test Pad vid=0x1 pid=0x2", Some((209.804, 119.126)))
            .unwrap();
        w.frame(
            &Frame {
                contacts: vec![Contact {
                    id: 3,
                    x: 12.5,
                    y: 7.25,
                    tip: true,
                    confidence: true,
                }],
                scan_time_100us: 0,
                button: false,
            },
            Timestamp::from_nanos(1_000_000),
        )
        .unwrap();
        w.frame(
            &Frame {
                contacts: Vec::new(),
                scan_time_100us: 0,
                button: true,
            },
            Timestamp::from_nanos(9_000_000),
        )
        .unwrap();
        w.finish().unwrap();

        let c = read(&path).unwrap();
        assert_eq!(c.device.as_deref(), Some("Test Pad vid=0x1 pid=0x2"));
        assert_eq!(c.pad, Some((209.804, 119.126)));
        assert_eq!(c.frames.len(), 2);

        let (ts, f) = &c.frames[0];
        assert_eq!(ts.as_nanos(), 1_000_000);
        assert_eq!(f.contacts.len(), 1);
        assert_eq!(f.contacts[0].id, 3);
        assert!((f.contacts[0].x - 12.5).abs() < 1e-6);
        assert!(f.contacts[0].tip && f.contacts[0].confidence);
        assert!(!f.button);

        let (ts2, f2) = &c.frames[1];
        assert_eq!(ts2.as_nanos(), 9_000_000);
        assert!(f2.contacts.is_empty());
        assert!(f2.button);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_a_file_that_is_not_a_capture() {
        let dir = std::env::temp_dir().join(format!("tpc-cap-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("nope.txt");
        std::fs::write(&path, "hello\n").unwrap();
        assert!(read(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
