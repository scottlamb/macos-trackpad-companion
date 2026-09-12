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

// The modules live in the library (see lib.rs); this binary uses
// them rather than declaring them again. Declaring both compiles
// every module twice and runs every test twice.
use macos_trackpad_companion::{
    app_kit, capture, config, config_watch, gesture, hid, instance_lock, onboarding, output,
    overlay, pause, permissions, scope, settings, status_item, system_prefs, time,
};

use anyhow::{Context, Result};
use clap::Parser;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

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

    /// Print the HID report descriptor of every matching device and
    /// exit, without changing any device's mode.
    ///
    /// A descriptor is enough to diagnose an unsupported trackpad and
    /// to turn it into a parser test, so this makes compatibility
    /// questions answerable by email rather than by owning the hardware.
    #[arg(long)]
    dump_descriptors: bool,

    /// Record the device's frame stream to a file for offline replay.
    ///
    /// Captures what the pad reported, not what the engine did, so the
    /// same stream can be replayed through a changed engine. See the
    /// `replay` binary.
    #[arg(long, value_name = "PATH")]
    record: Option<PathBuf>,
}

fn main() -> Result<()> {
    // Block SIGINT/SIGTERM in the main thread *before* any other code
    // runs. Threads inherit the caller's signal mask at spawn time;
    // a clean Ctrl+C only fires `DeviceState::drop` (which writes the
    // firmware's "back to mouse" SET) if all threads have these
    // signals blocked, so the dedicated sigwait worker installed
    // later catches them. NSApplication, IOHIDManager, env_logger,
    // anything that calls into a framework that internally
    // pthread_create's must run after this.
    hid::block_shutdown_signals();

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
    let mut log_builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level.as_str()));
    log_builder.format_timestamp_millis();
    // --dump-descriptors is a diagnostic run: its output belongs on
    // stderr where the user can see it, not appended to the daemon's
    // log file.
    let log_file_path = if args.dump_descriptors {
        None
    } else {
        cfg.log.file.as_deref().map(config::expand_tilde)
    };
    if let Some(path) = log_file_path.as_deref() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create log dir {}", parent.display()))?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open log file {}", path.display()))?;
        log_builder.target(env_logger::Target::Pipe(Box::new(file)));
    }
    log_builder.init();
    config_watch::set_log_file_path(log_file_path.clone());

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
    // Before the instance lock on purpose: inspecting devices changes
    // nothing, so it must work while the daemon is running.
    if args.dump_descriptors {
        return dump_descriptors(&cfg);
    }

    let lock = match instance_lock::acquire() {
        Ok(lock) => lock,
        Err(e) if e.downcast_ref::<instance_lock::AlreadyRunning>().is_some() => {
            // Exit 0 on purpose. A duplicate launch is normal — the user
            // double-clicked, or launchd started one while another was
            // already up — and under a KeepAlive agent a non-zero exit
            // would be restarted immediately, forever.
            log::info!("{e}");
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    log::debug!("acquired instance lock at {}", lock.path.display());

    let perms = permissions::State::current();
    log::info!(
        "permissions: input monitoring = {:?}, accessibility = {}",
        perms.input_monitoring,
        perms.accessibility
    );
    if !perms.accessibility {
        log::warn!(
            "Accessibility is NOT granted — CGEvents will be posted and silently \
             discarded by macOS. Gestures will appear to work in the logs while \
             nothing moves on screen."
        );
    }

    log::info!(
        "pointer policy: macOS ignores built-in trackpad when external present = {}",
        system_prefs::builtin_trackpad_ignored()
    );

    let emitter = output::Emitter::new(output_config(&cfg));
    let mut manager = hid::Manager::new(hid::Filter {
        vid: cfg.device.vid,
        pid: cfg.device.pid,
    })
    .context("open IOHIDManager")?;

    // Install the menu-bar icon before the event loop starts. Bound to
    // a named guard: dropping a `StatusItem` pulls it out of the menu
    // bar, so it has to live as long as `main` does.
    let mtm = objc2::MainThreadMarker::new()
        .ok_or_else(|| anyhow::anyhow!("main() must run on the main thread"))?;
    // Before any timer is installed: App Nap coalesces them hard, and
    // the retry/reload timers are how this process recovers.
    app_kit::disable_app_nap();
    settings::set_config_path(cfg_path.clone());
    let _status_item = status_item::StatusItem::install(mtm);

    // Auto-open when either grant is missing: without Accessibility the
    // companion fails silently, and a first-run user has no reason to
    // go hunting in the menu.
    onboarding::show_if_needed(mtm);

    if cfg.overlay.enable {
        let overlay = overlay::Overlay::new(cfg.overlay.duration_ms);
        let wrapped = output::OverlayOutput::new(emitter, overlay);
        run(&mut manager, wrapped, &cfg, cfg_path, args.record.clone())?;
    } else {
        run(&mut manager, emitter, &cfg, cfg_path, args.record.clone())?;
    }

    Ok(())
}

/// Report what is attached and exit, touching nothing.
///
/// Deliberately skips the status item, the setup window, the config
/// watcher and — most importantly — the PTP mode switch: inspecting a
/// device should not reconfigure it.
fn dump_descriptors(cfg: &config::Config) -> Result<()> {
    hid::set_dump_only(true);

    let mut manager = hid::Manager::new(hid::Filter {
        vid: cfg.device.vid,
        pid: cfg.device.pid,
    })
    .context("open IOHIDManager")?;

    // Device matching is delivered by callbacks on the run loop, so
    // give them a moment to arrive and then shut down through the usual
    // signal path.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_secs(3));
        hid::request_shutdown();
    });

    log::info!("dumping descriptors for matching devices (3s)...");
    manager.run(|_frame, _ts| {})?;
    Ok(())
}

/// Drive the gesture engine, with the config file watched for changes.
///
/// The engine lives behind `Rc<RefCell<_>>` so two callbacks can reach
/// it: the per-frame HID callback and the config-reload callback. Both
/// run on the main run loop, so there is no contention — the `RefCell`
/// is bookkeeping, not synchronisation.
fn run<O: output::Output + 'static>(
    manager: &mut hid::Manager,
    out: O,
    cfg: &config::Config,
    cfg_path: PathBuf,
    record: Option<PathBuf>,
) -> Result<()> {
    let state = Rc::new(RefCell::new(gesture::State::new(out, cursor_accel(cfg))));

    // The gesture scope's feed, installed unconditionally. It refuses
    // frames until the window is open, so a daemon whose scope has
    // never been opened pays one `Cell` read per frame — and the engine
    // still knows nothing about the window, exactly as with pad
    // geometry below.
    state.borrow_mut().set_observer(Some(Box::new(scope::Feed)));

    // The config currently in force. The scope's tuning sliders change
    // six of its fields and hand the result straight to the engine;
    // the file watcher below replaces the whole thing when the file
    // changes. Both paths end at the same `apply_config`, so a slider
    // is felt immediately and the file — which the slider also writes,
    // debounced — stays the shape of truth.
    let live_cfg = Rc::new(RefCell::new(cfg.clone()));

    let tune_state = Rc::downgrade(&state);
    let tune_cfg = Rc::downgrade(&live_cfg);
    scope::set_live_apply(move |t: scope::Tuning| {
        let (Some(tune_state), Some(tune_cfg)) = (tune_state.upgrade(), tune_cfg.upgrade()) else {
            return;
        };
        let mut cfg = tune_cfg.borrow_mut();
        cfg.cursor.sensitivity = t.cursor_sensitivity;
        cfg.cursor.accel_exponent = t.cursor_accel_exponent;
        cfg.cursor.accel_ref = t.cursor_accel_ref;
        cfg.scroll.sensitivity = t.scroll_sensitivity;
        cfg.scroll.accel_exponent = t.scroll_accel_exponent;
        cfg.scroll.accel_ref = t.scroll_accel_ref;
        tune_state
            .borrow_mut()
            .apply_config(cursor_accel(&cfg), output_config(&cfg));
    });

    let reload_state = Rc::clone(&state);
    let reload_cfg = Rc::clone(&live_cfg);
    // Held until `run` returns: dropping the timer stops the watching.
    let _watch = config_watch::start(cfg_path, cfg, move |new_cfg| {
        log::debug!(
            "applying: cursor.sensitivity={} cursor.accel_exponent={} \
             scroll.sensitivity={} scroll.natural={}",
            new_cfg.cursor.sensitivity,
            new_cfg.cursor.accel_exponent,
            new_cfg.scroll.sensitivity,
            new_cfg.scroll.natural,
        );
        *reload_cfg.borrow_mut() = new_cfg.clone();
        reload_state
            .borrow_mut()
            .apply_config(cursor_accel(new_cfg), output_config(new_cfg));
    });

    // These UI hooks must not keep the engine alive after run returns.
    let settle_state = Rc::downgrade(&state);
    pause::set_settle_hook(move || {
        if let Some(state) = settle_state.upgrade() {
            state.borrow_mut().cancel_at(time::Timestamp::now());
        }
    });

    let mut recorder = match record {
        Some(path) => {
            log::info!("recording frames to {}", path.display());
            Some(capture::Writer::create(&path)?)
        }
        None => None,
    };
    let mut recorded_device = false;

    let frame_state = Rc::clone(&state);
    let mut known_geometry: Option<(f64, f64)> = None;
    let disconnect_state = Rc::clone(&state);
    manager.run_with_disconnect(
        move |frame, ts| {
            if pause::is_paused() {
                return;
            }

            if let Some(w) = recorder.as_mut() {
                if !recorded_device {
                    recorded_device = true;
                    let summary = hid::device_summary().unwrap_or_else(|| "unknown".into());
                    if let Err(e) = w.device(&summary, hid::device_geometry()) {
                        log::error!("capture header failed: {e:#}");
                    }
                }
                if let Err(e) = w.frame(&frame, ts) {
                    log::error!("capture write failed: {e:#}");
                }
            }
            // Cheap per-frame check rather than a callback from the HID
            // layer: it keeps the gesture engine free of any dependency on
            // how devices are discovered.
            let geometry = hid::device_geometry();
            if geometry != known_geometry {
                known_geometry = geometry;
                frame_state
                    .borrow_mut()
                    .set_pad_geometry(geometry.map(|(w, h)| gesture::PadGeometry {
                        width_mm: w,
                        height_mm: h,
                    }));
            }
            frame_state.borrow_mut().on_frame_at(frame, ts)
        },
        move || {
            disconnect_state
                .borrow_mut()
                .cancel_at(time::Timestamp::now());
        },
    )
}

/// Flatten the TOML config into the emitter's runtime settings. Called
/// at startup and again on every reload.
fn output_config(cfg: &config::Config) -> output::Config {
    output::Config {
        scroll_accel: output::ScrollAccel {
            px_per_mm_at_ref: cfg.scroll.sensitivity,
            exponent: cfg.scroll.accel_exponent,
            ref_mm_per_sec: cfg.scroll.accel_ref,
        },
        natural_scroll: cfg.scroll.natural,
        pinch: enable_to_policy(&cfg.gestures.pinch.enable),
        rotate: enable_to_policy(&cfg.gestures.rotate.enable),
        horizontal_swipe: resolve_swipe(&cfg.gestures.swipe.horizontal),
        vertical_swipe: resolve_swipe(&cfg.gestures.swipe.vertical),
    }
}

fn cursor_accel(cfg: &config::Config) -> gesture::CursorAccel {
    gesture::CursorAccel {
        px_per_mm_at_ref: cfg.cursor.sensitivity,
        exponent: cfg.cursor.accel_exponent,
        ref_mm_per_sec: cfg.cursor.accel_ref,
    }
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
