//! magic-trackpad-companion — userspace bridge from a PTP HID device
//! (Windows Precision Touchpad / Microsoft Precision Touchpad) to native
//! macOS gesture events.
//!
//! On Linux and Windows, PTP devices are handled natively. macOS has no
//! built-in PTP consumer, so this process opens the device's digitizer
//! interface, decodes touch frames, and synthesizes CGEvents for cursor,
//! click, scroll, pinch, rotate, and 3+/4-finger swipe.
//!
//! Permissions: needs Input Monitoring (to read raw HID) and Accessibility
//! (to post CGEvents) the first run; macOS will prompt.

mod descriptor;
mod gesture;
mod hid;
mod output;
mod report;

use anyhow::{Context, Result};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Match only devices with this USB vendor ID (hex). Default: any PTP device.
    #[arg(long, value_parser = parse_hex_u16)]
    vid: Option<u16>,

    /// Match only devices with this USB product ID (hex). Default: any PTP device.
    #[arg(long, value_parser = parse_hex_u16)]
    pid: Option<u16>,

    /// Disable private CGEvent gesture-type injection. Pinch/rotate/swipe
    /// fall back to keyboard-shortcut emulation (poor UX) or no-op.
    #[arg(long)]
    no_private_gestures: bool,

    /// Cursor pixels per logical normalized unit. Higher = faster cursor.
    #[arg(long, default_value_t = 1500.0)]
    accel: f64,

    /// Scroll pixels per logical normalized unit.
    #[arg(long, default_value_t = 1200.0)]
    scroll_accel: f64,

    /// Verbose logging. Repeat for trace-level (-vv).
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let s = s.trim_start_matches("0x").trim_start_matches("0X");
    u16::from_str_radix(s, 16).map_err(|e| e.to_string())
}

fn main() -> Result<()> {
    let args = Args::parse();

    let level = match args.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level))
        .format_timestamp_millis()
        .init();

    log::info!(
        "magic-trackpad-companion starting (vid={:?} pid={:?} private_gestures={})",
        args.vid,
        args.pid,
        !args.no_private_gestures
    );

    let cfg = output::Config {
        accel: args.accel,
        scroll_accel: args.scroll_accel,
        private_gestures: !args.no_private_gestures,
    };
    let emitter = output::Emitter::new(cfg);
    let mut state = gesture::State::new(emitter);

    let mut manager = hid::Manager::new(hid::Filter {
        vid: args.vid,
        pid: args.pid,
    })
    .context("open IOHIDManager")?;

    manager.run(move |frame| state.on_frame(frame))?;

    Ok(())
}
