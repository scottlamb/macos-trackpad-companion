//! Single-instance guard via `flock(2)` on a per-user pidfile.
//!
//! Two companions racing the same trackpad is destructive: on graceful
//! shutdown each writes the PTP-control byte back to mouse mode,
//! flipping the firmware out of PTP underneath whichever instance is
//! still running. The second instance also doesn't reliably receive
//! input reports (IOKit delivers each report to one consumer), so even
//! before shutdown it's deadweight that's about to take down the live
//! one.
//!
//! The lock fd is held for the lifetime of the process; the kernel
//! releases the flock on exit (clean, panic, or SIGKILL), so there's no
//! stale-lock recovery path.

use anyhow::{Context, Result};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Another instance already holds the lock.
///
/// Typed rather than a plain message because the caller has to treat it
/// as a *normal* outcome and exit successfully. Under a `KeepAlive`
/// LaunchAgent, exiting non-zero here would have launchd restart the
/// duplicate immediately, which spins into a restart loop — the exact
/// failure the previous hand-rolled agent for this project exhibited.
#[derive(Debug)]
pub struct AlreadyRunning {
    pub pid: String,
    pub path: PathBuf,
}

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "another companion instance is already running (lock {} held by PID {}); \
             running two would clobber each other's PTP input-mode state on the firmware",
            self.path.display(),
            self.pid,
        )
    }
}

impl std::error::Error for AlreadyRunning {}

/// How long to keep trying for the lock before deciding another
/// instance really does hold it.
const ACQUIRE_TIMEOUT: Duration = Duration::from_millis(1500);
const ACQUIRE_RETRY_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub struct InstanceLock {
    // Held purely for its Drop side effect: closing the fd releases the
    // kernel's flock on this inode.
    _file: File,
    pub path: PathBuf,
}

pub fn acquire() -> Result<InstanceLock> {
    acquire_at(&default_lock_path()?)
}

fn acquire_at(path: &Path) -> Result<InstanceLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open lock file {}", path.display()))?;

    // Retry briefly rather than concluding on the first refusal.
    //
    // Two reasons. The practical one: a replacement instance routinely
    // starts while its predecessor is still shutting down — the
    // Relaunch button spawns one after a second, and launchd restarts
    // faster than that — and HID teardown is not instant. Giving up
    // immediately would have the replacement exit as a duplicate,
    // leaving nothing running at all, which is exactly the recovery
    // path start-at-login depends on.
    //
    // The other: flock has been observed returning EWOULDBLOCK
    // transiently right after a release under load. That showed up as a
    // test failing about one run in five, where a lock re-acquired
    // immediately after being dropped in the same process was refused.
    let mut rv;
    let deadline = std::time::Instant::now() + ACQUIRE_TIMEOUT;
    loop {
        rv = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rv == 0 {
            break;
        }
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EWOULDBLOCK) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(ACQUIRE_RETRY_INTERVAL);
    }

    if rv != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let other = read_pid(&mut file).unwrap_or_else(|| "<unknown>".into());
            return Err(anyhow::Error::new(AlreadyRunning {
                pid: other,
                path: path.to_path_buf(),
            }));
        }
        return Err(err).with_context(|| format!("flock {}", path.display()));
    }

    // Truncate-and-rewrite happens after locking so the contents can't
    // race against another acquire.
    file.set_len(0).context("truncate lock file")?;
    file.seek(SeekFrom::Start(0)).context("seek lock file")?;
    writeln!(file, "{}", std::process::id()).context("write PID to lock file")?;

    Ok(InstanceLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

fn read_pid(file: &mut File) -> Option<String> {
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut s = String::new();
    file.read_to_string(&mut s).ok()?;
    let pid = s.trim();
    if pid.is_empty() {
        None
    } else {
        Some(pid.to_string())
    }
}

fn default_lock_path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home)
        .join("Library")
        .join("Caches")
        .join("macos-trackpad-companion")
        .join("instance.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_while_first_held() {
        let dir = tempdir();
        let path = dir.join("instance.lock");

        let first = acquire_at(&path).expect("first acquire");
        // Held throughout, so this waits out ACQUIRE_TIMEOUT and then
        // reports contention — the behaviour a real second instance sees.
        let err = acquire_at(&path).expect_err("second acquire must fail");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("already running"),
            "expected lock-held error, got: {msg}",
        );

        drop(first);
        let _third = acquire_at(&path).expect("acquire after release");
    }

    #[test]
    fn lock_contention_is_a_typed_error() {
        let dir = tempdir();
        let path = dir.join("instance.lock");
        let _first = acquire_at(&path).expect("first acquire");
        let err = acquire_at(&path).expect_err("second acquire must fail");
        assert!(
            err.downcast_ref::<AlreadyRunning>().is_some(),
            "caller must be able to recognise this and exit 0",
        );
    }

    #[test]
    fn pid_is_written() {
        let dir = tempdir();
        let path = dir.join("instance.lock");

        let _lock = acquire_at(&path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.trim(), std::process::id().to_string());
    }

    fn tempdir() -> PathBuf {
        // A counter, not just a timestamp: these tests run in parallel
        // threads and macOS's clock granularity is coarser than a
        // nanosecond, so two of them starting together could land on
        // the same directory and then fight over one lock file. That
        // made `second_acquire_fails_while_first_held` fail about one
        // run in five.
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let base = std::env::temp_dir().join(format!(
            "mtc-instance-lock-test-{}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
