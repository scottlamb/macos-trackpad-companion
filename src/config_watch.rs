//! Watch the config file and re-apply it without a restart.
//!
//! Implemented as an mtime poll on a `CFRunLoopTimer` rather than
//! FSEvents. A config file changes a handful of times in a session, one
//! `stat` per second is free, and polling the path (rather than a file
//! descriptor) is naturally correct for editors that save by writing a
//! temp file and renaming over the original — the case that silently
//! breaks watchers bound to an inode. The interface here is narrow
//! enough that swapping in FSEvents later touches only this file.
//!
//! A file that fails to parse does not disturb the running daemon: the
//! error is logged and the previous settings stay in force. `config.rs`
//! rejects unknown keys, so a typo is a parse error — fatal at startup
//! by design, but no reason to tear down a daemon that is already
//! working.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::config::Config;
use crate::run_loop_timer::Timer;

/// Owns the polling timer and callback. Dropping this stops watching.
pub struct Watch {
    _timer: Timer,
}

thread_local! {
    /// Where logs are going, if anywhere. Recorded so the menu can
    /// reveal the file without re-reading and re-resolving the config.
    static LOG_FILE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Remember the resolved log path (called from `main` after it expands
/// `~` and creates the directory).
pub fn set_log_file_path(path: Option<PathBuf>) {
    LOG_FILE.with(|p| *p.borrow_mut() = path);
}

pub fn log_file_path() -> Option<PathBuf> {
    LOG_FILE.with(|p| p.borrow().clone())
}

/// How often to stat the config file.
const POLL_SECS: f64 = 1.0;

struct Watcher {
    path: PathBuf,
    last_mtime: Option<SystemTime>,
    /// Settings that are only read during startup. Held so a change to
    /// one can be reported rather than silently ignored — the reload
    /// applies everything else, and staying quiet about the rest would
    /// be worse than saying "restart for this to take effect".
    initial_device: (Option<u16>, Option<u16>),
    initial_log: (String, Option<PathBuf>),
    on_change: Box<dyn FnMut(&Config)>,
}

/// Start watching `path`. The returned timer must be held for as long
/// as watching should continue; dropping it stops the polling.
///
/// `on_change` runs on the main thread, in the same run loop as the HID
/// callbacks, so it can mutate engine state directly.
pub fn start<F>(path: PathBuf, initial: &Config, on_change: F) -> Watch
where
    F: FnMut(&Config) + 'static,
{
    let mut watcher = Watcher {
        last_mtime: mtime(&path),
        path,
        initial_device: (initial.device.vid, initial.device.pid),
        initial_log: (initial.log.level.clone(), initial.log.file.clone()),
        on_change: Box::new(on_change),
    };
    let timer = Timer::new(POLL_SECS, POLL_SECS, move || watcher.tick());
    log::debug!("watching config for changes every {POLL_SECS}s");
    Watch { _timer: timer }
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}

impl Watcher {
    fn tick(&mut self) {
        let watcher = self;
        let current = mtime(&watcher.path);
        if current == watcher.last_mtime {
            return;
        }
        // Record the new mtime even when the parse fails, so a file that
        // stays broken is reported once rather than every second.
        watcher.last_mtime = current;

        if current.is_none() {
            log::warn!(
                "config {} disappeared; keeping current settings",
                watcher.path.display()
            );
            return;
        }

        match crate::config::load(Some(&watcher.path)) {
            Ok((cfg, _)) => {
                if (cfg.device.vid, cfg.device.pid) != watcher.initial_device {
                    log::warn!(
                        "[device] changed in config but is only read at startup — \
                     restart for it to take effect"
                    );
                }
                if (cfg.log.level.clone(), cfg.log.file.clone()) != watcher.initial_log {
                    log::warn!(
                        "[log] changed in config but is only read at startup — \
                     restart for it to take effect"
                    );
                }
                log::info!("config reloaded from {}", watcher.path.display());
                (watcher.on_change)(&cfg);
            }
            Err(e) => {
                log::warn!("config reload failed, keeping previous settings: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    #[test]
    fn dropping_watch_releases_the_engine_callback() {
        let owner = Rc::new(());
        let weak = Rc::downgrade(&owner);
        let watch = start(
            PathBuf::from("/nonexistent/tpc-review-config"),
            &Config::default(),
            move |_| {
                let _keep_alive = &owner;
            },
        );
        assert!(weak.upgrade().is_some());
        drop(watch);
        assert!(weak.upgrade().is_none());
    }
}
