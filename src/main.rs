//! macos-trackpad-companion — userspace bridge from a PTP HID device
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
//!
//! Configuration: all tuning lives in a TOML file at
//! `$XDG_CONFIG_HOME/macos-trackpad-companion/config.toml` (default
//! `~/.config/macos-trackpad-companion/config.toml`). The CLI surface
//! intentionally only carries `--config PATH` and `-v` — see `config.rs`
//! / README for the full schema.

mod config;
mod descriptor;
mod gesture;
mod hid;
mod instance_lock;
mod output;
mod report;
mod scan_clock;
mod time;

use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Path to TOML config. Default:
    /// `$XDG_CONFIG_HOME/macos-trackpad-companion/config.toml`
    /// (or `~/.config/macos-trackpad-companion/config.toml` if
    /// `XDG_CONFIG_HOME` is unset). Missing file → all defaults.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Verbose logging (-v debug, -vv trace). Overrides `[log].level`
    /// from the config file when set.
    #[arg(short, long, action = clap::ArgAction::Count)]
    verbose: u8,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let (cfg, cfg_path) = config::load(args.config.as_deref())?;

    let level = if args.verbose > 0 {
        match args.verbose {
            1 => "debug",
            _ => "trace",
        }
        .to_string()
    } else {
        cfg.log.level.clone()
    };
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level.as_str()))
        .format_timestamp_millis()
        .init();

    if cfg_path.exists() {
        log::info!(
            "macos-trackpad-companion starting (config={})",
            cfg_path.display()
        );
    } else {
        log::info!(
            "macos-trackpad-companion starting (no config at {} — using defaults)",
            cfg_path.display(),
        );
    }
    log::debug!("resolved config: {:#?}", cfg);

    // Bound to a non-underscore name so the guard lives until end of
    // main; closing the fd releases the kernel's flock.
    let lock = instance_lock::acquire()?;
    log::debug!("acquired instance lock at {}", lock.path.display());

    // Frontmost-app filtering isn't wired yet (no NSWorkspace source).
    // Warn loudly so a user who configured `only` / `except` knows their
    // gesture is effectively off / on rather than mysteriously inert.
    warn_app_filter_unwired("pinch", &cfg.gestures.pinch.enable);
    warn_app_filter_unwired("rotate", &cfg.gestures.rotate.enable);
    warn_app_filter_unwired("swipe.horizontal", &cfg.gestures.swipe.horizontal.enable);
    warn_app_filter_unwired("swipe.vertical", &cfg.gestures.swipe.vertical.enable);

    let out_cfg = output::Config {
        scroll_accel: cfg.scroll.sensitivity,
        natural_scroll: cfg.scroll.natural,
        pinch_enabled: enable_to_bool(&cfg.gestures.pinch.enable),
        rotate_enabled: enable_to_bool(&cfg.gestures.rotate.enable),
        horizontal_swipe: resolve_swipe(&cfg.gestures.swipe.horizontal),
        vertical_swipe: resolve_swipe(&cfg.gestures.swipe.vertical),
    };
    let cursor_accel = gesture::CursorAccel {
        px_per_mm_at_ref: cfg.cursor.sensitivity,
        exponent: cfg.cursor.accel_exponent,
        ref_mm_per_sec: cfg.cursor.accel_ref,
    };
    let emitter = output::Emitter::new(out_cfg);
    let mut state = gesture::State::new(emitter, cursor_accel);

    let mut manager = hid::Manager::new(hid::Filter {
        vid: cfg.device.vid,
        pid: cfg.device.pid,
    })
    .context("open IOHIDManager")?;

    manager.run(move |frame, ts| state.on_frame_at(frame, ts))?;

    Ok(())
}

/// Collapse a [`config::GestureEnable`] to a single boolean for the
/// pinch/rotate gates. `Only` (no frontmost source yet) → `false`;
/// `Except` → `true`. The startup warning lives in
/// [`warn_app_filter_unwired`].
fn enable_to_bool(en: &config::GestureEnable) -> bool {
    match en {
        config::GestureEnable::On => true,
        config::GestureEnable::Off => false,
        config::GestureEnable::Only(_) => false,
        config::GestureEnable::Except(_) => true,
    }
}

fn resolve_swipe(c: &config::SwipeAxisCfg) -> output::SwipeBackend {
    let backend = match c.backend {
        config::SwipeBackend::Synthetic => output::SwipeBackend::Synthetic,
        config::SwipeBackend::Notification => output::SwipeBackend::Notification,
        config::SwipeBackend::Off => output::SwipeBackend::Off,
    };
    if enable_to_bool(&c.enable) {
        backend
    } else {
        output::SwipeBackend::Off
    }
}

fn warn_app_filter_unwired(name: &str, en: &config::GestureEnable) {
    match en {
        config::GestureEnable::Only(apps) => log::warn!(
            "[gestures.{name}] enable.only = {:?}: frontmost-app source not wired yet, gesture is OFF",
            apps,
        ),
        config::GestureEnable::Except(apps) => log::warn!(
            "[gestures.{name}] enable.except = {:?}: frontmost-app source not wired yet, gesture is ON",
            apps,
        ),
        _ => {}
    }
}
