//! Menu-bar status item — the only UI the companion presents.
//!
//! The icon is a macOS *template* image: pure black on transparency,
//! with `setTemplate(true)` letting AppKit invert it for dark menu
//! bars, selection highlighting, and Reduce Transparency. The PNG is
//! compiled into the binary rather than loaded from the bundle's
//! Resources, so the status item works the same when running the plain
//! CLI binary out of `target/release`.
//!
//! Quit raises `SIGTERM` at itself rather than calling
//! `[NSApp terminate:]`. terminate: calls `exit()`, which skips every
//! Rust destructor — including `hid::DeviceState::drop`, the thing that
//! writes the firmware back to mouse mode. Raising the signal instead
//! lands in the existing sigwait worker, so Quit and Ctrl+C take the
//! exact same shutdown path.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{AnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSAlertThirdButtonReturn, NSImage, NSMenu,
    NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString};

use crate::app_kit;

/// 40x40 @2x template rendered from `assets/icons/bridge-template.svg`.
/// Drawn at 20pt; AppKit picks the right scale from the declared size.
const TEMPLATE_PNG: &[u8] = include_bytes!("../assets/icons/bridge-template-40.png");

/// Menu-bar icons are measured in points, independent of the PNG's
/// pixel dimensions.
const ICON_POINTS: f64 = 20.0;

thread_local! {
    /// The disabled menu line that reports device state. Lives in a
    /// thread-local because `Retained` is `!Send` and only the main
    /// thread ever touches AppKit; [`set_status`] is a no-op on any
    /// other thread, and on the main thread before `install`.
    static STATUS_LINE: RefCell<Option<Retained<NSMenuItem>>> = const { RefCell::new(None) };
}

/// Update the menu's device-state line. Safe to call before the status
/// item exists (the plain CLI path never installs one) and from the
/// HID layer as devices come and go.
pub fn set_status(text: &str) {
    STATUS_LINE.with(|cell| {
        if let Some(item) = cell.borrow().as_ref() {
            item.setTitle(&NSString::from_str(text));
        }
    });
}

define_class!(
    #[unsafe(super(NSObject))]
    #[name = "TrackpadCompanionMenuTarget"]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(togglePause:))]
        fn toggle_pause(&self, sender: Option<&AnyObject>) {
            // Pausing has the same consequence as quitting for a pad
            // that only works while we drive it — except you also need
            // a pointer to un-pause. Ask first.
            if !crate::pause::is_paused()
                && no_pointer_fallback()
                && let Some(mtm) = MainThreadMarker::new()
                && !confirm_pointer_loss(mtm, "Pause and lose the trackpad?", "Pause Anyway")
            {
                return;
            }
            let paused = crate::pause::toggle();
            if let Some(item) = sender.and_then(|s| s.downcast_ref::<NSMenuItem>()) {
                item.setTitle(&NSString::from_str(if paused {
                    "Resume"
                } else {
                    "Pause"
                }));
            }
            set_status(if paused { "Paused" } else { "Running" });
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            if let Some(mtm) = MainThreadMarker::new() {
                crate::settings::show(mtm);
            }
        }

        #[unsafe(method(openScope:))]
        fn open_scope(&self, _sender: Option<&AnyObject>) {
            if let Some(mtm) = MainThreadMarker::new() {
                crate::scope::show(mtm);
            }
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            // Quitting can leave the machine with no pointer at all: a
            // spec-path pad stops responding once nothing is driving
            // it, and macOS may be ignoring the built-in trackpad
            // because that pad is attached. Never do that silently.
            // Only warn when quitting would genuinely leave no pointer:
            // a pad that goes dormant without us, and no usable built-in
            // trackpad to fall back on — either because macOS is
            // ignoring it, or because this Mac hasn't got one.
            if no_pointer_fallback()
                && let Some(mtm) = MainThreadMarker::new()
                && !confirm_pointer_loss(mtm, "Quit and lose the trackpad?", "Quit Anyway")
            {
                log::info!("quit cancelled");
                return;
            }
            log::info!("quit requested from menu");
            // Straight into the sigwait worker installed by
            // `hid::Manager::run`, which stops the event loop and lets
            // `main` unwind so the firmware gets reverted.
            crate::hid::request_shutdown();
        }
    }
);

/// Whether stopping now would leave the machine with no usable pointer:
/// a pad that only responds while we drive it, and no built-in trackpad
/// to fall back on — either because macOS is ignoring it, or because
/// this Mac hasn't got one.
fn no_pointer_fallback() -> bool {
    crate::hid::quit_would_strand_device()
        && (crate::system_prefs::builtin_trackpad_ignored()
            || !crate::hid::builtin_trackpad_present())
}

/// Warn before an action that would leave no working pointer.
///
/// Returns true if the user chose to go ahead.
fn confirm_pointer_loss(mtm: MainThreadMarker, title: &str, proceed: &str) -> bool {
    // Bring the app forward so the alert isn't stranded behind other
    // windows — an accessory app's modal can otherwise be easy to miss.
    app_kit::activate_for_window(mtm);

    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Warning);
    alert.setMessageText(&NSString::from_str(title));
    alert.setInformativeText(&NSString::from_str(concat!(
        "This trackpad stops responding while Trackpad Companion isn't running, ",
        "and macOS may be ignoring your built-in trackpad because an external ",
        "pointing device is attached — so quitting can leave you with no pointer ",
        "at all.\n\n",
        "It works again as soon as the companion is driving it again; unplugging ",
        "is not required. Control-F2 focuses the menu bar if you need to reach ",
        "this menu without a pointer.\n\n",
        "To keep the built-in trackpad available, turn off “Ignore built-in ",
        "trackpad when mouse or wireless trackpad is present” in Accessibility > ",
        "Pointer Control.",
    )));
    alert.addButtonWithTitle(&NSString::from_str(proceed));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    alert.addButtonWithTitle(&NSString::from_str("Open Accessibility Settings…"));

    let response = alert.runModal();
    app_kit::settle_activation(mtm);

    if response == NSAlertThirdButtonReturn {
        crate::permissions::open_pointer_control_settings();
        return false;
    }
    response == NSAlertFirstButtonReturn
}

impl MenuTarget {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let _ = mtm;
        unsafe { msg_send![Self::alloc(), init] }
    }
}

/// Live status item. Dropping it removes the icon from the menu bar, so
/// `main` holds it for the lifetime of the daemon.
pub struct StatusItem {
    item: Retained<NSStatusItem>,
    /// The menu's `target` is an unretained reference on the AppKit
    /// side, so the target has to outlive the menu.
    _target: Retained<MenuTarget>,
}

impl StatusItem {
    /// Install the status item. Must run on the main thread, before the
    /// event loop starts.
    pub fn install(mtm: MainThreadMarker) -> Self {
        app_kit::ensure_app(mtm);

        let status_bar = NSStatusBar::systemStatusBar();
        let item = status_bar.statusItemWithLength(NSVariableStatusItemLength);

        if let Some(button) = item.button(mtm) {
            let data = NSData::with_bytes(TEMPLATE_PNG);
            if let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) {
                image.setSize(NSSize::new(ICON_POINTS, ICON_POINTS));
                image.setTemplate(true);
                button.setImage(Some(&image));
            } else {
                log::warn!("status item: embedded template PNG failed to decode");
            }
        } else {
            log::warn!("status item has no button; icon not shown");
        }

        let target = MenuTarget::new(mtm);
        let menu = Self::build_menu(mtm, &target);
        item.setMenu(Some(&menu));

        log::debug!("status item installed");
        Self {
            item,
            _target: target,
        }
    }

    fn build_menu(mtm: MainThreadMarker, target: &Retained<MenuTarget>) -> Retained<NSMenu> {
        let menu = NSMenu::initWithTitle(mtm.alloc::<NSMenu>(), &NSString::from_str(""));

        // Disabled header, so the menu says what it belongs to.
        let header = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str(concat!("Trackpad Companion ", env!("CARGO_PKG_VERSION"))),
                None,
                &NSString::from_str(""),
            )
        };
        header.setEnabled(false);
        menu.addItem(&header);

        // Device state. Updated by the HID layer via `set_status`; the
        // initial text is what shows until the first open attempt
        // resolves.
        let status = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Starting…"),
                None,
                &NSString::from_str(""),
            )
        };
        status.setEnabled(false);
        menu.addItem(&status);
        STATUS_LINE.with(|cell| *cell.borrow_mut() = Some(status));

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        // Reopening matters: the setup window auto-opens once at launch,
        // and without this there is no way back to it.
        let pause_item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str(if crate::pause::is_paused() {
                    "Resume"
                } else {
                    "Pause"
                }),
                Some(sel!(togglePause:)),
                &NSString::from_str(""),
            )
        };
        unsafe { pause_item.setTarget(Some(target.as_ref() as &AnyObject)) };
        menu.addItem(&pause_item);

        let settings = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Settings…"),
                Some(sel!(openSettings:)),
                &NSString::from_str(","),
            )
        };
        unsafe { settings.setTarget(Some(target.as_ref() as &AnyObject)) };
        menu.addItem(&settings);

        // The live view of what the recognizer is thinking. Off until
        // asked for: the engine skips building a snapshot while nothing
        // is watching.
        let scope = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Gesture Scope…"),
                Some(sel!(openScope:)),
                &NSString::from_str("g"),
            )
        };
        unsafe { scope.setTarget(Some(target.as_ref() as &AnyObject)) };
        menu.addItem(&scope);

        menu.addItem(&NSMenuItem::separatorItem(mtm));

        let quit = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc::<NSMenuItem>(),
                &NSString::from_str("Quit"),
                Some(sel!(quit:)),
                &NSString::from_str("q"),
            )
        };
        unsafe { quit.setTarget(Some(target.as_ref() as &AnyObject)) };
        menu.addItem(&quit);

        menu
    }
}

impl Drop for StatusItem {
    fn drop(&mut self) {
        STATUS_LINE.with(|cell| *cell.borrow_mut() = None);
        let status_bar = NSStatusBar::systemStatusBar();
        status_bar.removeStatusItem(&self.item);
    }
}
