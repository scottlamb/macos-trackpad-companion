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

use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};

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

    let rv = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rv != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let other = read_pid(&mut file).unwrap_or_else(|| "<unknown>".into());
            bail!(
                "another companion instance is already running (lock {} held by PID {}); \
                 running two would clobber each other's PTP input-mode state on the firmware",
                path.display(),
                other,
            );
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
    fn pid_is_written() {
        let dir = tempdir();
        let path = dir.join("instance.lock");

        let _lock = acquire_at(&path).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents.trim(), std::process::id().to_string());
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "mtc-instance-lock-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
