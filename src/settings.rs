//! Settings window.
//!
//! Every control writes to the config file; nothing here touches the
//! gesture engine. [`crate::config_watch`] notices the file change and
//! applies it, so the file stays the single source of truth and the UI
//! can't drift from what's on disk. Writes go through
//! [`crate::config_edit`], which patches individual keys and preserves
//! comments.
//!
//! Sliders are continuous so the value label tracks the drag, but the
//! write is debounced: a drag schedules a flush [`FLUSH_DELAY_SECS`]
//! after the last movement, and the flush writes all three slider
//! values in one edit. Without it a single drag rewrites the file
//! dozens of times — churn on a file that may well be in git, and a
//! wider window for a concurrent hand edit to be lost.
//!
//! Gestures whose `enable` holds an app list (`{ only = [...] }`) get a
//! disabled checkbox. A checkbox cannot represent a list, and writing
//! "on" over one would throw the user's list away.

use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;

use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{
    CFRunLoop, CFRunLoopTimer, CFRunLoopTimerContext, kCFRunLoopCommonModes,
};
use core_foundation_sys::runloop::{CFRunLoopTimerInvalidate, CFRunLoopTimerRef};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSBackingStoreType, NSButton, NSControlStateValueOff, NSControlStateValueOn, NSFont,
    NSPasteboard, NSPasteboardTypeString, NSSlider, NSTextField, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::app_kit;
use crate::config::{self, Config, GestureEnable};
use crate::config_edit::ConfigFile;

const WINDOW_W: f64 = 520.0;
const WINDOW_H: f64 = 656.0;
const POLL_SECS: f64 = 1.0;
/// Quiet period after the last slider movement before the file is
/// written. Long enough to coalesce a drag, short enough that letting
/// go feels immediate.
const FLUSH_DELAY_SECS: f64 = 0.25;

// Slider ranges. `pub(crate)` because the gesture scope carries the
// same sliders and must not invent its own bounds — two windows
// editing one key through different ranges is a bug waiting to happen.
pub(crate) const CURSOR_MIN: f64 = 5.0;
pub(crate) const CURSOR_MAX: f64 = 80.0;
pub(crate) const EXPONENT_MIN: f64 = 0.5;
pub(crate) const EXPONENT_MAX: f64 = 2.0;
pub(crate) const SCROLL_MIN: f64 = 5.0;
pub(crate) const SCROLL_MAX: f64 = 80.0;
/// Velocity at which `sensitivity` is the plain linear feel, in mm/s.
pub(crate) const ACCEL_REF_MIN: f64 = 20.0;
pub(crate) const ACCEL_REF_MAX: f64 = 200.0;

thread_local! {
    static SETTINGS: RefCell<Option<Window>> = const { RefCell::new(None) };
    /// Set once from `main`; the window has no other way to know which
    /// file it is editing.
    static CONFIG_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Tell the settings window which config file to edit.
pub fn set_config_path(path: PathBuf) {
    CONFIG_PATH.with(|p| *p.borrow_mut() = Some(path));
}

/// The config file being edited, for diagnostics output.
pub fn config_path_display() -> String {
    config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "unknown".into())
}

/// The config file both this window and the gesture scope edit.
pub(crate) fn config_path() -> Option<PathBuf> {
    CONFIG_PATH.with(|p| p.borrow().clone())
}

/// Apply an edit to the config file. Reloads the document each time so
/// hand edits made since the window opened are never clobbered.
///
/// Shared with the gesture scope, whose sliders write the same keys —
/// one implementation of "patch the file without eating the comments".
/// Returns whether the file actually took the edit. The settings
/// window can ignore that — its controls were only ever a view of the
/// file, so a failed write leaves nothing inconsistent. The gesture
/// scope cannot: it has already handed the value to the engine, and a
/// failure there means the engine is running on something the file
/// never accepted.
pub(crate) fn edit(f: impl FnOnce(&mut ConfigFile) -> anyhow::Result<()>) -> bool {
    let Some(path) = config_path() else {
        log::error!("settings: no config path registered");
        return false;
    };
    let result = ConfigFile::load(&path).and_then(|mut doc| {
        f(&mut doc)?;
        doc.save()
    });
    match result {
        Ok(()) => {
            log::debug!("settings written to {}", path.display());
            true
        }
        Err(e) => {
            log::error!("settings write failed: {e:#}");
            false
        }
    }
}

struct Window {
    window: Retained<NSWindow>,
    cursor_sensitivity: Retained<NSSlider>,
    cursor_sensitivity_value: Retained<NSTextField>,
    cursor_exponent: Retained<NSSlider>,
    cursor_exponent_value: Retained<NSTextField>,
    cursor_accel_ref: Retained<NSSlider>,
    cursor_accel_ref_value: Retained<NSTextField>,
    scroll_sensitivity: Retained<NSSlider>,
    scroll_sensitivity_value: Retained<NSTextField>,
    scroll_exponent: Retained<NSSlider>,
    scroll_exponent_value: Retained<NSTextField>,
    scroll_accel_ref: Retained<NSSlider>,
    scroll_accel_ref_value: Retained<NSTextField>,
    natural: Retained<NSButton>,
    pinch: Retained<NSButton>,
    rotate: Retained<NSButton>,
    swipe_h: Retained<NSButton>,
    swipe_v: Retained<NSButton>,
    overlay: Retained<NSButton>,
    login: Retained<NSButton>,
    timer: Option<CFRunLoopTimer>,
    /// Pending debounced write, replaced on each slider movement.
    flush_timer: Option<CFRunLoopTimer>,
    /// Config mtime as of the last populate, so an edit made in a text
    /// editor while this window is open is picked up.
    seen_mtime: Option<std::time::SystemTime>,
    _actions: Retained<Actions>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TrackpadCompanionSettingsActions"]
    struct Actions;

    impl Actions {
        #[unsafe(method(cursorSensitivity:))]
        fn cursor_sensitivity(&self, _s: Option<&AnyObject>) {
            with_window_mut(|w| {
                let v = round1(w.cursor_sensitivity.doubleValue());
                w.cursor_sensitivity_value.setStringValue(&fmt(v));
                w.schedule_flush();
            });
        }

        #[unsafe(method(cursorExponent:))]
        fn cursor_exponent(&self, _s: Option<&AnyObject>) {
            with_window_mut(|w| {
                let v = round2(w.cursor_exponent.doubleValue());
                w.cursor_exponent_value.setStringValue(&fmt(v));
                w.schedule_flush();
            });
        }

        #[unsafe(method(cursorAccelRef:))]
        fn cursor_accel_ref(&self, _s: Option<&AnyObject>) {
            with_window_mut(|w| {
                let v = round1(w.cursor_accel_ref.doubleValue());
                w.cursor_accel_ref_value.setStringValue(&fmt(v));
                w.schedule_flush();
            });
        }

        #[unsafe(method(scrollSensitivity:))]
        fn scroll_sensitivity(&self, _s: Option<&AnyObject>) {
            with_window_mut(|w| {
                let v = round1(w.scroll_sensitivity.doubleValue());
                w.scroll_sensitivity_value.setStringValue(&fmt(v));
                w.schedule_flush();
            });
        }

        #[unsafe(method(scrollExponent:))]
        fn scroll_exponent(&self, _s: Option<&AnyObject>) {
            with_window_mut(|w| {
                let v = round2(w.scroll_exponent.doubleValue());
                w.scroll_exponent_value.setStringValue(&fmt(v));
                w.schedule_flush();
            });
        }

        #[unsafe(method(scrollAccelRef:))]
        fn scroll_accel_ref(&self, _s: Option<&AnyObject>) {
            with_window_mut(|w| {
                let v = round1(w.scroll_accel_ref.doubleValue());
                w.scroll_accel_ref_value.setStringValue(&fmt(v));
                w.schedule_flush();
            });
        }

        #[unsafe(method(naturalScroll:))]
        fn natural_scroll(&self, _s: Option<&AnyObject>) {
            with_window(|w| {
                let on = checked(&w.natural);
                let _ = edit(|c| c.set_bool(&["scroll"], "natural", on));
            });
        }

        #[unsafe(method(togglePinch:))]
        fn toggle_pinch(&self, _s: Option<&AnyObject>) {
            with_window(|w| {
                let on = checked(&w.pinch);
                let _ = edit(|c| c.set_str(&["gestures", "pinch"], "enable", on_off(on)));
            });
        }

        #[unsafe(method(toggleRotate:))]
        fn toggle_rotate(&self, _s: Option<&AnyObject>) {
            with_window(|w| {
                let on = checked(&w.rotate);
                let _ = edit(|c| c.set_str(&["gestures", "rotate"], "enable", on_off(on)));
            });
        }

        #[unsafe(method(toggleSwipeH:))]
        fn toggle_swipe_h(&self, _s: Option<&AnyObject>) {
            with_window(|w| {
                let on = checked(&w.swipe_h);
                let _ = edit(|c| {
                    c.set_str(&["gestures", "swipe", "horizontal"], "enable", on_off(on))
                });
            });
        }

        #[unsafe(method(toggleSwipeV:))]
        fn toggle_swipe_v(&self, _s: Option<&AnyObject>) {
            with_window(|w| {
                let on = checked(&w.swipe_v);
                let _ = edit(|c| c.set_str(&["gestures", "swipe", "vertical"], "enable", on_off(on)));
            });
        }

        #[unsafe(method(toggleOverlay:))]
        fn toggle_overlay(&self, _s: Option<&AnyObject>) {
            with_window(|w| {
                let on = checked(&w.overlay);
                let _ = edit(|c| c.set_bool(&["overlay"], "enable", on));
            });
        }

        #[unsafe(method(resetDefaults:))]
        fn reset_defaults(&self, _s: Option<&AnyObject>) {
            with_window(|w| w.reset_to_defaults());
        }

        #[unsafe(method(toggleLoginItem:))]
        fn toggle_login_item(&self, _s: Option<&AnyObject>) {
            with_window(|w| {
                let enabling = !crate::launch_agent::is_enabled();
                let result = if enabling {
                    crate::launch_agent::enable()
                } else {
                    crate::launch_agent::disable()
                };
                match result {
                    Ok(()) => set_checked(&w.login, enabling),
                    Err(e) => {
                        log::error!("start-at-login toggle failed: {e:#}");
                        // Put the checkbox back where reality is.
                        set_checked(&w.login, crate::launch_agent::is_enabled());
                    }
                }
            });
        }

        #[unsafe(method(openPermissions:))]
        fn open_permissions(&self, _s: Option<&AnyObject>) {
            if let Some(mtm) = MainThreadMarker::new() {
                crate::onboarding::show(mtm);
            }
        }

        #[unsafe(method(copyDiagnostics:))]
        fn copy_diagnostics(&self, _s: Option<&AnyObject>) {
            let pb = NSPasteboard::generalPasteboard();
            pb.clearContents();
            let ok = unsafe {
                pb.setString_forType(
                    &NSString::from_str(&crate::diagnostics::text()),
                    NSPasteboardTypeString,
                )
            };
            if ok {
                log::info!("diagnostics copied to the clipboard");
            } else {
                log::error!("failed to write diagnostics to the clipboard");
            }
        }

        #[unsafe(method(revealLog:))]
        fn reveal_log(&self, _s: Option<&AnyObject>) {
            match crate::config_watch::log_file_path() {
                Some(path) if path.exists() => {
                    let _ = std::process::Command::new("/usr/bin/open")
                        .arg("-R")
                        .arg(&path)
                        .spawn();
                }
                Some(path) => log::info!("no log file at {} yet", path.display()),
                None => log::info!("logging to stderr; set [log].file to get a file"),
            }
        }

        #[unsafe(method(revealConfig:))]
        fn reveal_config(&self, _s: Option<&AnyObject>) {
            let Some(path) = config_path() else { return };
            // -R reveals in Finder rather than opening the .toml in
            // whatever happens to claim that extension.
            let _ = std::process::Command::new("/usr/bin/open")
                .arg("-R")
                .arg(&path)
                .spawn();
        }
    }
);

impl Actions {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let _ = mtm;
        unsafe { msg_send![Self::alloc(), init] }
    }
}

fn with_window_mut(f: impl FnOnce(&mut Window)) {
    SETTINGS.with(|cell| {
        if let Some(w) = cell.borrow_mut().as_mut() {
            f(w);
        }
    });
}

fn with_window(f: impl FnOnce(&Window)) {
    SETTINGS.with(|cell| {
        if let Some(w) = cell.borrow().as_ref() {
            f(w);
        }
    });
}

fn checked(b: &Retained<NSButton>) -> bool {
    b.state() == NSControlStateValueOn
}

fn on_off(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

// Shared with the gesture scope's sliders: both windows write the same
// same keys, and a value that rounded differently depending on which
// window you dragged would show up as a phantom file change.
pub(crate) fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

pub(crate) fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

fn fmt(v: f64) -> Retained<NSString> {
    NSString::from_str(&format!("{v:.2}"))
}

/// Open the settings window, creating it on first use.
pub fn show(mtm: MainThreadMarker) {
    SETTINGS.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Window::build(mtm));
        }
        if let Some(w) = slot.as_mut() {
            w.present(mtm);
        }
    });
}

impl Window {
    fn build(mtm: MainThreadMarker) -> Self {
        app_kit::ensure_app(mtm);

        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WINDOW_W, WINDOW_H));
        let window: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect,
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("Trackpad Companion Settings"));
        // Same trap as the setup window: the default frees the window on
        // close, dangling the Retained held here.
        unsafe { window.setReleasedWhenClosed(false) };
        app_kit::register_window(window.clone());

        let actions = Actions::new(mtm);
        let content = window
            .contentView()
            .expect("NSWindow auto-creates a contentView");

        let title = label(mtm, "Settings", 24.0, 614.0, 300.0, 22.0);
        title.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
        content.addSubview(&title);
        let blurb = label(
            mtm,
            "Saved to the config file and applied within a second.",
            24.0,
            592.0,
            472.0,
            18.0,
        );
        blurb.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        content.addSubview(&blurb);

        content.addSubview(&section(mtm, "Cursor", 560.0));
        content.addSubview(&label(mtm, "Sensitivity", 24.0, 532.0, 120.0, 18.0));
        let cursor_sensitivity = slider(
            mtm,
            25.0,
            CURSOR_MIN,
            CURSOR_MAX,
            &actions,
            sel!(cursorSensitivity:),
            150.0,
            528.0,
        );
        content.addSubview(&cursor_sensitivity);
        let cursor_sensitivity_value = label(mtm, "", 400.0, 532.0, 90.0, 18.0);
        content.addSubview(&cursor_sensitivity_value);

        content.addSubview(&label(mtm, "Acceleration", 24.0, 502.0, 120.0, 18.0));
        let cursor_exponent = slider(
            mtm,
            1.0,
            EXPONENT_MIN,
            EXPONENT_MAX,
            &actions,
            sel!(cursorExponent:),
            150.0,
            498.0,
        );
        content.addSubview(&cursor_exponent);
        let cursor_exponent_value = label(mtm, "", 400.0, 502.0, 90.0, 18.0);
        content.addSubview(&cursor_exponent_value);

        content.addSubview(&label(mtm, "Accel reference", 24.0, 472.0, 120.0, 18.0));
        let cursor_accel_ref = slider(
            mtm,
            80.0,
            ACCEL_REF_MIN,
            ACCEL_REF_MAX,
            &actions,
            sel!(cursorAccelRef:),
            150.0,
            468.0,
        );
        content.addSubview(&cursor_accel_ref);
        let cursor_accel_ref_value = label(mtm, "", 400.0, 472.0, 90.0, 18.0);
        content.addSubview(&cursor_accel_ref_value);
        let accel_note = label(
            mtm,
            "mm/s at which sensitivity is the plain linear feel",
            150.0,
            450.0,
            340.0,
            14.0,
        );
        accel_note.setFont(Some(&NSFont::systemFontOfSize(10.0)));
        content.addSubview(&accel_note);

        content.addSubview(&section(mtm, "Scroll", 418.0));
        content.addSubview(&label(mtm, "Sensitivity", 24.0, 390.0, 120.0, 18.0));
        let scroll_sensitivity = slider(
            mtm,
            20.0,
            SCROLL_MIN,
            SCROLL_MAX,
            &actions,
            sel!(scrollSensitivity:),
            150.0,
            386.0,
        );
        content.addSubview(&scroll_sensitivity);
        let scroll_sensitivity_value = label(mtm, "", 400.0, 390.0, 90.0, 18.0);
        content.addSubview(&scroll_sensitivity_value);

        content.addSubview(&label(mtm, "Acceleration", 24.0, 360.0, 120.0, 18.0));
        let scroll_exponent = slider(
            mtm,
            1.3,
            EXPONENT_MIN,
            EXPONENT_MAX,
            &actions,
            sel!(scrollExponent:),
            150.0,
            356.0,
        );
        content.addSubview(&scroll_exponent);
        let scroll_exponent_value = label(mtm, "", 400.0, 360.0, 90.0, 18.0);
        content.addSubview(&scroll_exponent_value);

        content.addSubview(&label(mtm, "Accel reference", 24.0, 330.0, 120.0, 18.0));
        let scroll_accel_ref = slider(
            mtm,
            60.0,
            ACCEL_REF_MIN,
            ACCEL_REF_MAX,
            &actions,
            sel!(scrollAccelRef:),
            150.0,
            326.0,
        );
        content.addSubview(&scroll_accel_ref);
        let scroll_accel_ref_value = label(mtm, "", 400.0, 330.0, 90.0, 18.0);
        content.addSubview(&scroll_accel_ref_value);

        let natural = checkbox(
            mtm,
            "Natural scrolling",
            &actions,
            sel!(naturalScroll:),
            150.0,
            300.0,
            240.0,
        );
        content.addSubview(&natural);

        content.addSubview(&section(mtm, "Gestures", 266.0));
        let pinch = checkbox(
            mtm,
            "Pinch",
            &actions,
            sel!(togglePinch:),
            24.0,
            238.0,
            220.0,
        );
        content.addSubview(&pinch);
        let rotate = checkbox(
            mtm,
            "Rotate",
            &actions,
            sel!(toggleRotate:),
            262.0,
            238.0,
            220.0,
        );
        content.addSubview(&rotate);
        let swipe_h = checkbox(
            mtm,
            "Swipe — horizontal",
            &actions,
            sel!(toggleSwipeH:),
            24.0,
            214.0,
            220.0,
        );
        content.addSubview(&swipe_h);
        let swipe_v = checkbox(
            mtm,
            "Swipe — vertical",
            &actions,
            sel!(toggleSwipeV:),
            262.0,
            214.0,
            220.0,
        );
        content.addSubview(&swipe_v);
        let overlay = checkbox(
            mtm,
            "Show gesture overlay",
            &actions,
            sel!(toggleOverlay:),
            24.0,
            190.0,
            300.0,
        );
        content.addSubview(&overlay);

        content.addSubview(&section(mtm, "General", 156.0));
        let login = checkbox(
            mtm,
            "Start at login",
            &actions,
            sel!(toggleLoginItem:),
            24.0,
            128.0,
            300.0,
        );
        content.addSubview(&login);
        let login_note = label(
            mtm,
            "Restarts the companion after a crash, not after you quit.",
            44.0,
            108.0,
            452.0,
            16.0,
        );
        login_note.setFont(Some(&NSFont::systemFontOfSize(10.0)));
        content.addSubview(&login_note);

        content.addSubview(&section(mtm, "Troubleshooting", 74.0));
        let perms_btn = push(
            mtm,
            "Permissions…",
            &actions,
            sel!(openPermissions:),
            24.0,
            56.0,
            140.0,
        );
        content.addSubview(&perms_btn);
        let diag_btn = push(
            mtm,
            "Copy Diagnostics",
            &actions,
            sel!(copyDiagnostics:),
            172.0,
            56.0,
            160.0,
        );
        content.addSubview(&diag_btn);
        let log_btn = push(
            mtm,
            "Reveal Log…",
            &actions,
            sel!(revealLog:),
            340.0,
            56.0,
            140.0,
        );
        content.addSubview(&log_btn);
        let reveal = push(
            mtm,
            "Reveal Config File…",
            &actions,
            sel!(revealConfig:),
            24.0,
            20.0,
            180.0,
        );
        content.addSubview(&reveal);
        let reset = push(
            mtm,
            "Reset to Defaults",
            &actions,
            sel!(resetDefaults:),
            212.0,
            20.0,
            170.0,
        );
        content.addSubview(&reset);

        Self {
            window,
            cursor_sensitivity,
            cursor_sensitivity_value,
            cursor_exponent,
            cursor_exponent_value,
            cursor_accel_ref,
            cursor_accel_ref_value,
            scroll_sensitivity,
            scroll_sensitivity_value,
            scroll_exponent,
            scroll_exponent_value,
            scroll_accel_ref,
            scroll_accel_ref_value,
            natural,
            pinch,
            rotate,
            swipe_h,
            swipe_v,
            overlay,
            login,
            timer: None,
            flush_timer: None,
            seen_mtime: None,
            _actions: actions,
        }
    }

    fn present(&mut self, mtm: MainThreadMarker) {
        self.populate();
        self.seen_mtime = Self::config_mtime();
        app_kit::activate_for_window(mtm);
        self.window.center();
        self.window.makeKeyAndOrderFront(None);

        if self.timer.is_none() {
            let mut ctx = CFRunLoopTimerContext {
                version: 0,
                info: std::ptr::null_mut(),
                retain: None,
                release: None,
                copyDescription: None,
            };
            let fire_at = unsafe { CFAbsoluteTimeGetCurrent() } + POLL_SECS;
            let timer = CFRunLoopTimer::new(fire_at, POLL_SECS, 0, 0, on_tick, &mut ctx);
            CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopCommonModes });
            self.timer = Some(timer);
        }
    }

    /// Current mtime of the config file, if it has one.
    fn config_mtime() -> Option<std::time::SystemTime> {
        config_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok())
    }

    /// Load current values from the file.
    fn populate(&self) {
        let cfg = config_path()
            .and_then(|p| config::load(Some(&p)).ok())
            .map(|(c, _)| c)
            .unwrap_or_else(|| {
                log::warn!("settings: config unreadable, showing defaults");
                Config::default()
            });

        self.cursor_sensitivity
            .setDoubleValue(cfg.cursor.sensitivity);
        self.cursor_sensitivity_value
            .setStringValue(&fmt(cfg.cursor.sensitivity));
        self.cursor_exponent
            .setDoubleValue(cfg.cursor.accel_exponent);
        self.cursor_exponent_value
            .setStringValue(&fmt(cfg.cursor.accel_exponent));
        self.cursor_accel_ref.setDoubleValue(cfg.cursor.accel_ref);
        self.cursor_accel_ref_value
            .setStringValue(&fmt(cfg.cursor.accel_ref));
        self.scroll_sensitivity
            .setDoubleValue(cfg.scroll.sensitivity);
        self.scroll_sensitivity_value
            .setStringValue(&fmt(cfg.scroll.sensitivity));
        self.scroll_exponent
            .setDoubleValue(cfg.scroll.accel_exponent);
        self.scroll_exponent_value
            .setStringValue(&fmt(cfg.scroll.accel_exponent));
        self.scroll_accel_ref.setDoubleValue(cfg.scroll.accel_ref);
        self.scroll_accel_ref_value
            .setStringValue(&fmt(cfg.scroll.accel_ref));
        set_checked(&self.natural, cfg.scroll.natural);
        set_checked(&self.overlay, cfg.overlay.enable);
        // Not a config value — read the actual agent state each time.
        set_checked(&self.login, crate::launch_agent::is_enabled());

        apply_policy(&self.pinch, "Pinch", &cfg.gestures.pinch.enable);
        apply_policy(&self.rotate, "Rotate", &cfg.gestures.rotate.enable);
        apply_policy(
            &self.swipe_h,
            "Swipe — horizontal",
            &cfg.gestures.swipe.horizontal.enable,
        );
        apply_policy(
            &self.swipe_v,
            "Swipe — vertical",
            &cfg.gestures.swipe.vertical.enable,
        );
    }

    /// (Re)arm the debounced write. Each movement pushes the deadline
    /// out, so only the settled value reaches the file.
    fn schedule_flush(&mut self) {
        if let Some(timer) = self.flush_timer.take() {
            unsafe { CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef()) };
        }
        let mut ctx = CFRunLoopTimerContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            copyDescription: None,
        };
        let fire_at = unsafe { CFAbsoluteTimeGetCurrent() } + FLUSH_DELAY_SECS;
        // Non-repeating: one shot per quiet period.
        let timer = CFRunLoopTimer::new(fire_at, 0.0, 0, 0, on_flush, &mut ctx);
        CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopCommonModes });
        self.flush_timer = Some(timer);
    }

    /// Write every slider value in a single edit.
    fn flush(&mut self) {
        self.flush_timer = None;
        let cursor = round1(self.cursor_sensitivity.doubleValue());
        let exponent = round2(self.cursor_exponent.doubleValue());
        let accel_ref = round1(self.cursor_accel_ref.doubleValue());
        let scroll = round1(self.scroll_sensitivity.doubleValue());
        let scroll_exponent = round2(self.scroll_exponent.doubleValue());
        let scroll_accel_ref = round1(self.scroll_accel_ref.doubleValue());
        let _ = edit(|c| {
            c.set_f64(&["cursor"], "sensitivity", cursor)?;
            c.set_f64(&["cursor"], "accel_exponent", exponent)?;
            c.set_f64(&["cursor"], "accel_ref", accel_ref)?;
            c.set_f64(&["scroll"], "sensitivity", scroll)?;
            c.set_f64(&["scroll"], "accel_exponent", scroll_exponent)?;
            c.set_f64(&["scroll"], "accel_ref", scroll_accel_ref)?;
            Ok(())
        });
    }

    /// Restore the defaults for everything this window displays.
    ///
    /// Deliberately narrow: settings the window doesn't show (`[log]`,
    /// `[device]`) are left alone, and a gesture whose policy is
    /// an app list is skipped for the same reason its checkbox is
    /// disabled — a reset shouldn't quietly delete a curated list.
    fn reset_to_defaults(&self) {
        let defaults = Config::default();
        let current = config_path()
            .and_then(|p| config::load(Some(&p)).ok())
            .map(|(c, _)| c)
            .unwrap_or_default();

        let simple = |e: &GestureEnable| matches!(e, GestureEnable::On | GestureEnable::Off);
        let pinch_simple = simple(&current.gestures.pinch.enable);
        let rotate_simple = simple(&current.gestures.rotate.enable);
        let h_simple = simple(&current.gestures.swipe.horizontal.enable);
        let v_simple = simple(&current.gestures.swipe.vertical.enable);
        if !(pinch_simple && rotate_simple && h_simple && v_simple) {
            log::info!("reset: leaving app-list gesture policies untouched");
        }

        let _ = edit(|c| {
            c.set_f64(&["cursor"], "sensitivity", defaults.cursor.sensitivity)?;
            c.set_f64(
                &["cursor"],
                "accel_exponent",
                defaults.cursor.accel_exponent,
            )?;
            c.set_f64(&["cursor"], "accel_ref", defaults.cursor.accel_ref)?;
            c.set_f64(&["scroll"], "sensitivity", defaults.scroll.sensitivity)?;
            c.set_f64(
                &["scroll"],
                "accel_exponent",
                defaults.scroll.accel_exponent,
            )?;
            c.set_f64(&["scroll"], "accel_ref", defaults.scroll.accel_ref)?;
            c.set_bool(&["scroll"], "natural", defaults.scroll.natural)?;
            c.set_bool(&["overlay"], "enable", defaults.overlay.enable)?;
            if pinch_simple {
                c.set_str(&["gestures", "pinch"], "enable", "on")?;
            }
            if rotate_simple {
                c.set_str(&["gestures", "rotate"], "enable", "on")?;
            }
            if h_simple {
                c.set_str(&["gestures", "swipe", "horizontal"], "enable", "on")?;
            }
            if v_simple {
                c.set_str(&["gestures", "swipe", "vertical"], "enable", "on")?;
            }
            Ok(())
        });
        self.populate();
    }

    /// Re-read the file if it changed underneath us — but never while a
    /// write of our own is pending, which is the case where the user is
    /// mid-drag and would have the slider yanked out from under them.
    fn refresh_if_file_changed(&mut self) {
        if self.flush_timer.is_some() {
            return;
        }
        let now = Self::config_mtime();
        if now != self.seen_mtime {
            self.seen_mtime = now;
            self.populate();
        }
    }

    fn teardown(&mut self, mtm: MainThreadMarker) {
        if let Some(timer) = self.timer.take() {
            unsafe { CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef()) };
        }
        // A pending write must still land — closing the window mid-drag
        // shouldn't discard the change.
        if self.flush_timer.is_some() {
            self.flush();
        }
        app_kit::settle_activation(mtm);
    }
}

/// A gesture whose policy is an app list can't be represented by a
/// checkbox. Show it as on, disable it, and say why — writing "on"
/// over `{ only = [...] }` would silently discard the list.
fn apply_policy(button: &Retained<NSButton>, name: &str, enable: &GestureEnable) {
    match enable {
        GestureEnable::On => {
            set_checked(button, true);
            button.setEnabled(true);
            button.setTitle(&NSString::from_str(name));
        }
        GestureEnable::Off => {
            set_checked(button, false);
            button.setEnabled(true);
            button.setTitle(&NSString::from_str(name));
        }
        GestureEnable::Only(_) | GestureEnable::Except(_) => {
            set_checked(button, true);
            button.setEnabled(false);
            button.setTitle(&NSString::from_str(&format!(
                "{name} (app list — edit file)"
            )));
        }
    }
}

fn set_checked(b: &Retained<NSButton>, on: bool) {
    b.setState(if on {
        NSControlStateValueOn
    } else {
        NSControlStateValueOff
    });
}

extern "C" fn on_flush(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    with_window_mut(|w| w.flush());
}

extern "C" fn on_tick(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    SETTINGS.with(|cell| {
        if let Some(w) = cell.borrow_mut().as_mut() {
            if w.window.isVisible() {
                w.refresh_if_file_changed();
            } else {
                w.teardown(mtm);
            }
        }
    });
}

fn label(
    mtm: MainThreadMarker,
    text: &str,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Retained<NSTextField> {
    let f = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    f.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
    f
}

fn section(mtm: MainThreadMarker, text: &str, y: f64) -> Retained<NSTextField> {
    let f = label(mtm, text, 24.0, y, 200.0, 18.0);
    f.setFont(Some(&NSFont::boldSystemFontOfSize(12.0)));
    f
}

#[allow(clippy::too_many_arguments)]
fn slider(
    mtm: MainThreadMarker,
    value: f64,
    min: f64,
    max: f64,
    target: &Retained<Actions>,
    action: objc2::runtime::Sel,
    x: f64,
    y: f64,
) -> Retained<NSSlider> {
    let s = unsafe {
        NSSlider::sliderWithValue_minValue_maxValue_target_action(
            value,
            min,
            max,
            Some(target.as_ref() as &AnyObject),
            Some(action),
            mtm,
        )
    };
    s.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(240.0, 24.0)));
    s
}

fn push(
    mtm: MainThreadMarker,
    title: &str,
    target: &Retained<Actions>,
    action: objc2::runtime::Sel,
    x: f64,
    y: f64,
    w: f64,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target.as_ref() as &AnyObject),
            Some(action),
            mtm,
        )
    };
    b.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, 28.0)));
    b
}

fn checkbox(
    mtm: MainThreadMarker,
    title: &str,
    target: &Retained<Actions>,
    action: objc2::runtime::Sel,
    x: f64,
    y: f64,
    w: f64,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::checkboxWithTitle_target_action(
            &NSString::from_str(title),
            Some(target.as_ref() as &AnyObject),
            Some(action),
            mtm,
        )
    };
    b.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, 20.0)));
    b
}
