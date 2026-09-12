//! Main-thread/run-loop-local timer ownership. A CF release does not
//! unschedule a timer; the guard must invalidate it before dropping.

use std::cell::RefCell;
use std::ffi::c_void;
use std::marker::PhantomData;
use std::rc::Rc;

use core_foundation::base::TCFType;
use core_foundation::date::CFAbsoluteTimeGetCurrent;
use core_foundation::runloop::{CFRunLoop, CFRunLoopTimer, kCFRunLoopCommonModes};
use core_foundation_sys::runloop::{
    CFRunLoopTimerContext, CFRunLoopTimerInvalidate, CFRunLoopTimerRef,
};

type Callback = RefCell<Box<dyn FnMut(CFRunLoopTimerRef)>>;

pub(crate) struct Timer {
    timer: CFRunLoopTimer,
    // The callback can contain Rc, RefCell and AppKit objects. Creation,
    // firing and invalidation must all happen on the scheduling thread.
    _local: PhantomData<Rc<()>>,
}

impl Timer {
    pub(crate) fn new(delay: f64, interval: f64, mut f: impl FnMut() + 'static) -> Self {
        Self::with_callback(delay, interval, move |_| f())
    }

    pub(crate) fn with_callback(
        delay: f64,
        interval: f64,
        f: impl FnMut(CFRunLoopTimerRef) + 'static,
    ) -> Self {
        let callback: Rc<Callback> = Rc::new(RefCell::new(Box::new(f)));
        let mut context = CFRunLoopTimerContext {
            version: 0,
            info: Rc::as_ptr(&callback) as *mut c_void,
            retain: Some(retain_callback),
            release: Some(release_callback),
            copyDescription: None,
        };
        let timer = CFRunLoopTimer::new(
            unsafe { CFAbsoluteTimeGetCurrent() } + delay,
            interval,
            0,
            0,
            tick,
            &mut context,
        );
        CFRunLoop::get_current().add_timer(&timer, unsafe { kCFRunLoopCommonModes });
        Self {
            timer,
            _local: PhantomData,
        }
    }
}

impl Drop for Timer {
    fn drop(&mut self) {
        unsafe { CFRunLoopTimerInvalidate(self.timer.as_concrete_TypeRef()) };
    }
}

extern "C" fn retain_callback(info: *const c_void) -> *const c_void {
    unsafe { Rc::increment_strong_count(info as *const Callback) };
    info
}

extern "C" fn release_callback(info: *const c_void) {
    drop(unsafe { Rc::from_raw(info as *const Callback) });
}

extern "C" fn tick(timer: CFRunLoopTimerRef, info: *mut c_void) {
    // A callback may invalidate its own timer. Hold a temporary owner
    // until it returns, even if CF releases the context in the meantime.
    let callback = unsafe {
        Rc::increment_strong_count(info as *const Callback);
        Rc::from_raw(info as *const Callback)
    };
    // A nested run loop must not enter the same FnMut concurrently.
    if let Ok(mut f) = callback.try_borrow_mut() {
        f(timer);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_foundation::runloop::kCFRunLoopDefaultMode;
    use core_foundation_sys::runloop::{CFRunLoopTimerIsValid, CFRunLoopTimerSetNextFireDate};
    use std::cell::Cell;
    use std::time::Duration;

    #[test]
    fn dropping_timer_invalidates_it_and_releases_captured_state() {
        let owner = Rc::new(());
        let weak = Rc::downgrade(&owner);
        let calls = Rc::new(Cell::new(0));
        let seen = calls.clone();
        let timer = Timer::new(60.0, 60.0, move || {
            let _keep = &owner;
            seen.set(seen.get() + 1);
        });
        let cf = timer.timer.clone();
        drop(timer);
        assert!(weak.upgrade().is_none());
        assert_eq!(
            unsafe { CFRunLoopTimerIsValid(cf.as_concrete_TypeRef()) },
            0
        );
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(1),
            false,
        );
        assert_eq!(calls.get(), 0);
    }

    #[test]
    fn callback_can_drop_its_own_timer() {
        let slot = Rc::new(RefCell::new(None::<Timer>));
        let weak_slot = Rc::downgrade(&slot);
        let owner = Rc::new(());
        let weak_owner = Rc::downgrade(&owner);
        let timer = Timer::new(60.0, 60.0, move || {
            weak_slot.upgrade().unwrap().borrow_mut().take();
            assert_eq!(Rc::strong_count(&owner), 1);
        });
        let cf = timer.timer.clone();
        *slot.borrow_mut() = Some(timer);
        unsafe { CFRunLoopTimerSetNextFireDate(cf.as_concrete_TypeRef(), 0.0) };
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(10),
            false,
        );
        assert!(slot.borrow().is_none());
        assert!(weak_owner.upgrade().is_none());
    }

    #[test]
    fn a_fired_one_shot_releases_its_capture_while_the_guard_still_exists() {
        let owner = Rc::new(());
        let weak = Rc::downgrade(&owner);
        let timer = Timer::new(60.0, 0.0, move || {
            let _keep_alive = &owner;
        });
        unsafe { CFRunLoopTimerSetNextFireDate(timer.timer.as_concrete_TypeRef(), 0.0) };
        CFRunLoop::run_in_mode(
            unsafe { kCFRunLoopDefaultMode },
            Duration::from_millis(10),
            false,
        );
        assert!(weak.upgrade().is_none());
        drop(timer);
    }
}
