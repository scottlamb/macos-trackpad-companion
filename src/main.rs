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
mod scan_clock;
mod time;

use anyhow::{Context, Result};
use clap::Parser;
use output::SwipeBackend;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Match only devices with this USB vendor ID (hex). Default: any PTP device.
    #[arg(long, value_parser = parse_hex_u16)]
    vid: Option<u16>,

    /// Match only devices with this USB product ID (hex). Default: any PTP device.
    #[arg(long, value_parser = parse_hex_u16)]
    pid: Option<u16>,

    /// Disable private CGEvent gesture-type injection for pinch and rotate.
    /// Doesn't affect swipes — those have their own per-axis backend (see
    /// --swipe-h / --swipe-v), including a non-private notification fallback.
    #[arg(long)]
    no_private_gestures: bool,

    /// Backend for left/right (horizontal) 3F/4F swipes — Spaces and
    /// Full-Screen Apps. `synthetic` posts trackpad DockSwipe events
    /// for an animated transition; `notification` is silently treated
    /// as `off` here (no Dock notification for switching Spaces).
    #[arg(long, value_enum, default_value_t = SwipeBackend::Synthetic)]
    swipe_h: SwipeBackend,

    /// Backend for up/down (vertical) 3F/4F swipes — Mission Control
    /// and App Exposé. `synthetic` animates via DockSwipe events;
    /// `notification` fires the discrete Dock notification on lift past
    /// a commit threshold (no live animation).
    #[arg(long, value_enum, default_value_t = SwipeBackend::Synthetic)]
    swipe_v: SwipeBackend,

    /// Cursor sensitivity: screen pixels per millimeter of finger
    /// motion at the curve's reference velocity
    /// (`--cursor-accel-ref`). With the default
    /// `--cursor-accel-exponent=1.0` (linear), this value applies at
    /// every speed — ~25 matches the old default's feel on a 65 mm-wide
    /// pad. Pad-density independent.
    #[arg(long, default_value_t = 25.0)]
    sensitivity: f64,

    /// Power-curve exponent for cursor acceleration. `1.0` (default)
    /// is linear: `--sensitivity` pixels per mm regardless of speed.
    /// Values `> 1` boost fast movements (cross-screen flicks) and
    /// slow movements get sub-linear gain (more precision). Try
    /// `1.3`–`1.5` for a moderate curve. `< 1` would invert that —
    /// almost never useful for cursor (it's what `accelerate_scroll`
    /// does for scrolls, where the goal is to tame fast flicks).
    #[arg(long, default_value_t = 1.0)]
    cursor_accel_exponent: f64,

    /// Reference velocity (mm/s of finger travel) at which the curve
    /// reproduces the linear `--sensitivity` feel. Below this → less
    /// gain than linear; above this → more. Only matters when
    /// `--cursor-accel-exponent` differs from 1. ~80 mm/s is roughly
    /// "moving the cursor at conversational speed" on a small pad.
    #[arg(long, default_value_t = 80.0)]
    cursor_accel_ref: f64,

    /// Screen pixels per millimeter of finger motion in scroll mode.
    #[arg(long, default_value_t = 20.0)]
    scroll_accel: f64,

    /// Use the legacy "wheel" scroll direction (finger-down → content up,
    /// the way macOS shipped before 10.7). Off by default → finger-down →
    /// content-down, matching macOS's "Natural" scrolling.
    #[arg(long)]
    invert_scroll: bool,

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
        "magic-trackpad-companion starting (vid={:?} pid={:?} private_gestures={} swipe_h={:?} swipe_v={:?})",
        args.vid,
        args.pid,
        !args.no_private_gestures,
        args.swipe_h,
        args.swipe_v,
    );

    let cfg = output::Config {
        scroll_accel: args.scroll_accel,
        natural_scroll: !args.invert_scroll,
        private_gestures: !args.no_private_gestures,
        horizontal_swipe: args.swipe_h,
        vertical_swipe: args.swipe_v,
    };
    let cursor_accel = gesture::CursorAccel {
        px_per_mm_at_ref: args.sensitivity,
        exponent: args.cursor_accel_exponent,
        ref_mm_per_sec: args.cursor_accel_ref,
    };
    let emitter = output::Emitter::new(cfg);
    let mut state = gesture::State::new(emitter, cursor_accel);

    let mut manager = hid::Manager::new(hid::Filter {
        vid: args.vid,
        pid: args.pid,
    })
    .context("open IOHIDManager")?;

    manager.run(move |frame, ts| state.on_frame_at(frame, ts))?;

    Ok(())
}
