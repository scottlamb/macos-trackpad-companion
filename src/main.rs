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

mod app_context;
mod config;
mod descriptor;
mod gesture;
mod hid;
mod instance_lock;
mod output;
mod overlay;
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

    let out_cfg = output::Config {
        scroll_accel: cfg.scroll.sensitivity,
        natural_scroll: cfg.scroll.natural,
        pinch: enable_to_policy(&cfg.gestures.pinch.enable),
        rotate: enable_to_policy(&cfg.gestures.rotate.enable),
        horizontal_swipe: resolve_swipe(&cfg.gestures.swipe.horizontal),
        vertical_swipe: resolve_swipe(&cfg.gestures.swipe.vertical),
    };
    let cursor_accel = gesture::CursorAccel {
        px_per_mm_at_ref: cfg.cursor.sensitivity,
        exponent: cfg.cursor.accel_exponent,
        ref_mm_per_sec: cfg.cursor.accel_ref,
    };
    let emitter = output::Emitter::new(out_cfg);
    let mut manager = hid::Manager::new(hid::Filter {
        vid: cfg.device.vid,
        pid: cfg.device.pid,
    })
    .context("open IOHIDManager")?;

    if cfg.overlay.enable {
        let overlay = overlay::Overlay::new(cfg.overlay.duration_ms);
        let wrapped = output::OverlayOutput::new(emitter, overlay);
        let mut state = gesture::State::new(wrapped, cursor_accel);
        manager.run(move |frame, ts| state.on_frame_at(frame, ts))?;
    } else {
        let mut state = gesture::State::new(emitter, cursor_accel);
        manager.run(move |frame, ts| state.on_frame_at(frame, ts))?;
    }

    Ok(())
}

/// Translate a [`config::GestureEnable`] (TOML-shaped) into the
/// [`output::GesturePolicy`] the emitter consumes. Cheap clone — the
/// app lists are small and only constructed once at startup.
fn enable_to_policy(en: &config::GestureEnable) -> output::GesturePolicy {
    match en {
        config::GestureEnable::On => output::GesturePolicy::On,
        config::GestureEnable::Off => output::GesturePolicy::Off,
        config::GestureEnable::Only(apps) => output::GesturePolicy::Only(apps.clone()),
        config::GestureEnable::Except(apps) => output::GesturePolicy::Except(apps.clone()),
    }
}

fn resolve_swipe(c: &config::SwipeAxisCfg) -> output::SwipeConfig {
    let backend = match c.backend {
        config::SwipeBackend::Synthetic => output::SwipeBackend::Synthetic,
        config::SwipeBackend::Notification => output::SwipeBackend::Notification,
        config::SwipeBackend::Off => output::SwipeBackend::Off,
    };
    output::SwipeConfig {
        policy: enable_to_policy(&c.enable),
        backend,
    }
}
