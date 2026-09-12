//! First-run setup window: the two privacy grants, their live state, and
//! a button per row that does the right thing for that state.
//!
//! Opens automatically when either grant is missing, because a first-run
//! user has no reason to go looking in a menu — and without Accessibility
//! the companion fails silently, which is indistinguishable from being
//! broken.
//!
//! Layout is absolute rather than `NSStackView`: the window is a fixed
//! size with six controls, and hand-placed frames avoid a pile of
//! auto-layout FFI for no visible gain.
//!
//! Window close is detected by polling `isVisible` on the refresh timer
//! rather than by an `NSWindowDelegate`. Same effect, one less
//! Objective-C protocol to conform to.

use std::cell::RefCell;
use std::ffi::c_void;

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
    NSBackingStoreType, NSButton, NSFont, NSTextField, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use crate::app_kit;
use crate::permissions::{self, Access};

const WINDOW_W: f64 = 500.0;
const WINDOW_H: f64 = 268.0;
/// How often the window re-reads permission state. The user grants in
/// System Settings, in another app — there is no notification to
/// observe, so polling is the mechanism.
const REFRESH_SECS: f64 = 1.0;

thread_local! {
    static SETUP: RefCell<Option<Setup>> = const { RefCell::new(None) };
}

struct Setup {
    window: Retained<NSWindow>,
    im_status: Retained<NSTextField>,
    im_button: Retained<NSButton>,
    ax_status: Retained<NSTextField>,
    ax_button: Retained<NSButton>,
    note: Retained<NSTextField>,
    relaunch: Retained<NSButton>,
    /// Input Monitoring state when the process started. A grant made
    /// after launch doesn't apply until restart, so this is what tells
    /// us whether to offer Relaunch.
    initial_im: Access,
    timer: Option<CFRunLoopTimer>,
    _actions: Retained<Actions>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TrackpadCompanionSetupActions"]
    struct Actions;

    impl Actions {
        #[unsafe(method(grantInput:))]
        fn grant_input(&self, _sender: Option<&AnyObject>) {
            // Always ask first, even when the state reads Denied.
            // The request is what registers the app with TCC — without
            // it the app never appears in the Input Monitoring list,
            // and "Open Settings" lands on a pane with nothing to
            // toggle. Only fall back to the pane if asking didn't
            // resolve it.
            log::info!(
                "requesting Input Monitoring (current: {:?})",
                permissions::input_monitoring()
            );
            if !permissions::request_input_monitoring() {
                permissions::open_input_monitoring_settings();
            }
        }

        #[unsafe(method(grantAccessibility:))]
        fn grant_accessibility(&self, _sender: Option<&AnyObject>) {
            if permissions::accessibility() {
                return;
            }
            // The prompting variant shows a dialog with its own
            // "Open System Settings" button — but macOS suppresses that
            // dialog if the user dismissed it before, which would leave
            // this button doing nothing. Fall back to the pane.
            log::info!("requesting Accessibility");
            if !permissions::request_accessibility() {
                permissions::open_accessibility_settings();
            }
        }

        #[unsafe(method(relaunchApp:))]
        fn relaunch_app(&self, _sender: Option<&AnyObject>) {
            relaunch();
        }
    }
);

impl Actions {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let _ = mtm;
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// Open the window if either grant is missing. Called at startup.
pub fn show_if_needed(mtm: MainThreadMarker) {
    if permissions::State::current().all_granted() {
        return;
    }
    show(mtm);
}

/// Open the window unconditionally — the menu's "Setup…" item.
pub fn show(mtm: MainThreadMarker) {
    SETUP.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(Setup::build(mtm));
        }
        if let Some(setup) = slot.as_mut() {
            setup.present(mtm);
        }
    });
}

fn relaunch() {
    let Ok(exe) = std::env::current_exe() else {
        log::error!("relaunch: can't resolve current executable");
        return;
    };
    // .../companion.app/Contents/MacOS/companion -> .../companion.app
    let bundle = exe
        .ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(|p| p.to_path_buf());

    let command = match &bundle {
        Some(app) => format!("sleep 1; open -n '{}'", app.display()),
        None => format!("sleep 1; '{}'", exe.display()),
    };
    log::info!("relaunching: {command}");
    // Detach, wait for this process to release the instance lock, then
    // start the replacement.
    let spawned = std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .spawn();
    if let Err(e) = spawned {
        log::error!("relaunch failed to spawn: {e}");
        return;
    }
    // Same shutdown path as Ctrl+C and the Quit menu item.
    crate::hid::request_shutdown();
}

impl Setup {
    fn build(mtm: MainThreadMarker) -> Self {
        app_kit::ensure_app(mtm);

        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WINDOW_W, WINDOW_H));
        let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
        let window: Retained<NSWindow> = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc::<NSWindow>(),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        window.setTitle(&NSString::from_str("Trackpad Companion Setup"));
        // CRITICAL: programmatically created NSWindows default to
        // releasedWhenClosed = true, so clicking the close button
        // releases the window out from under the `Retained` held here.
        // The refresh timer then messages a freed object and segfaults
        // (EXC_BAD_ACCESS in NSWindow::isVisible). Keeping ownership
        // here means close just hides it, and `present` can show the
        // same window again.
        unsafe { window.setReleasedWhenClosed(false) };

        app_kit::register_window(window.clone());

        let actions = Actions::new(mtm);
        let content = window
            .contentView()
            .expect("NSWindow auto-creates a contentView");

        let title = label(
            mtm,
            "Two permissions are required",
            24.0,
            216.0,
            452.0,
            22.0,
        );
        title.setFont(Some(&NSFont::boldSystemFontOfSize(15.0)));
        content.addSubview(&title);

        let blurb = label(
            mtm,
            "macOS gates reading the trackpad and moving the cursor separately.",
            24.0,
            192.0,
            452.0,
            18.0,
        );
        blurb.setFont(Some(&NSFont::systemFontOfSize(12.0)));
        content.addSubview(&blurb);

        // --- Input Monitoring row ---
        let im_label = label(mtm, "Input Monitoring", 24.0, 150.0, 150.0, 20.0);
        im_label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        content.addSubview(&im_label);
        let im_why = label(
            mtm,
            "Read touch data from the trackpad",
            24.0,
            132.0,
            300.0,
            16.0,
        );
        im_why.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        content.addSubview(&im_why);
        let im_status = label(mtm, "", 180.0, 150.0, 150.0, 20.0);
        content.addSubview(&im_status);
        let im_button = button(mtm, "Grant…", &actions, sel!(grantInput:), 344.0, 144.0);
        content.addSubview(&im_button);

        // --- Accessibility row ---
        let ax_label = label(mtm, "Accessibility", 24.0, 96.0, 150.0, 20.0);
        ax_label.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
        content.addSubview(&ax_label);
        let ax_why = label(
            mtm,
            "Move the cursor and post gestures",
            24.0,
            78.0,
            300.0,
            16.0,
        );
        ax_why.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        content.addSubview(&ax_why);
        let ax_status = label(mtm, "", 180.0, 96.0, 150.0, 20.0);
        content.addSubview(&ax_status);
        let ax_button = button(
            mtm,
            "Grant…",
            &actions,
            sel!(grantAccessibility:),
            344.0,
            90.0,
        );
        content.addSubview(&ax_button);

        // --- footer ---
        let note = label(mtm, "", 24.0, 16.0, 310.0, 40.0);
        note.setFont(Some(&NSFont::systemFontOfSize(11.0)));
        content.addSubview(&note);

        let relaunch = button(mtm, "Relaunch", &actions, sel!(relaunchApp:), 344.0, 18.0);
        relaunch.setHidden(true);
        content.addSubview(&relaunch);

        Self {
            window,
            im_status,
            im_button,
            ax_status,
            ax_button,
            note,
            relaunch,
            initial_im: permissions::input_monitoring(),
            timer: None,
            _actions: actions,
        }
    }

    fn present(&mut self, mtm: MainThreadMarker) {
        app_kit::activate_for_window(mtm);

        self.window.center();
        self.window.makeKeyAndOrderFront(None);

        // Ask once on open so the app registers with TCC and shows up
        // in the Input Monitoring list. A no-op when already granted,
        // and silent (no prompt) when previously denied.
        if !permissions::input_monitoring().is_granted() {
            permissions::request_input_monitoring();
        }

        self.refresh();

        if self.timer.is_none() {
            let mut ctx = CFRunLoopTimerContext {
                version: 0,
                info: std::ptr::null_mut(),
                retain: None,
                release: None,
                copyDescription: None,
            };
            let fire_at = unsafe { CFAbsoluteTimeGetCurrent() } + REFRESH_SECS;
            let timer = CFRunLoopTimer::new(fire_at, REFRESH_SECS, 0, 0, on_tick, &mut ctx);
            CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopCommonModes });
            self.timer = Some(timer);
        }
    }

    /// Re-read both grants and restate the UI around them.
    fn refresh(&mut self) {
        let state = permissions::State::current();

        let (im_text, im_action, im_enabled) = match state.input_monitoring {
            Access::Granted => ("Granted", "Settings…", false),
            Access::Denied => ("Denied", "Open Settings…", true),
            Access::Unknown => ("Not requested", "Grant…", true),
        };
        self.im_status.setStringValue(&NSString::from_str(im_text));
        self.im_button.setTitle(&NSString::from_str(im_action));
        self.im_button.setEnabled(im_enabled);

        let (ax_text, ax_enabled) = if state.accessibility {
            ("Granted", false)
        } else {
            ("Not granted", true)
        };
        self.ax_status.setStringValue(&NSString::from_str(ax_text));
        self.ax_button.setEnabled(ax_enabled);

        // A grant made while running doesn't reach this process: the
        // HID access check is made when the manager opens.
        let needs_relaunch = state.input_monitoring.is_granted() && !self.initial_im.is_granted();
        self.relaunch.setHidden(!needs_relaunch);
        // Input Monitoring gets its own instruction. macOS often
        // declines to show the prompt for a background agent and
        // records a denial instead, leaving the app absent from the
        // list — so "Open Settings" lands on a pane with nothing to
        // toggle. Adding it by hand is the supported way out.
        self.note.setStringValue(&NSString::from_str(if needs_relaunch {
            "Input Monitoring was granted after launch. Relaunch to use it."
        } else if state.all_granted() {
            "All set — you can close this window."
        } else if !state.input_monitoring.is_granted() {
            "If Trackpad Companion isn't listed under Input Monitoring,\nclick + in that list and add it from ~/Applications."
        } else {
            "Granting opens System Settings; return here when done."
        }));
    }

    fn teardown(&mut self, mtm: MainThreadMarker) {
        if let Some(timer) = self.timer.take() {
            unsafe { CFRunLoopTimerInvalidate(timer.as_concrete_TypeRef()) };
        }
        // Back to a menu-bar-only agent — unless the settings window
        // is still open.
        app_kit::settle_activation(mtm);
    }
}

extern "C" fn on_tick(_timer: CFRunLoopTimerRef, _info: *mut c_void) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    SETUP.with(|cell| {
        if let Some(setup) = cell.borrow_mut().as_mut() {
            if setup.window.isVisible() {
                setup.refresh();
            } else {
                setup.teardown(mtm);
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
    let field = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    field.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(w, h)));
    field
}

fn button(
    mtm: MainThreadMarker,
    title: &str,
    target: &Retained<Actions>,
    action: objc2::runtime::Sel,
    x: f64,
    y: f64,
) -> Retained<NSButton> {
    let b = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target.as_ref() as &AnyObject),
            Some(action),
            mtm,
        )
    };
    b.setFrame(NSRect::new(NSPoint::new(x, y), NSSize::new(132.0, 28.0)));
    b
}
