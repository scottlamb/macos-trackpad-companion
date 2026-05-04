//! Monotonic timestamp shared by the gesture engine and the CGEvent
//! synthesizer. Backed by `CLOCK_UPTIME_RAW` (== `mach_absolute_time` on
//! Apple): nanoseconds since boot, paused while the system sleeps. Same
//! time base IOHID stamps on real-trackpad events, so values can be
//! passed directly to `CGEventSetTimestamp` — no calibration step. We
//! use this instead of `std::time::Instant` because Instant exposes only
//! relative arithmetic, but `CGEventSetTimestamp` and the embedded IOHID
//! payload structs in the gesture synthesizer (`output::synthesize_gesture_event`)
//! all want a raw `u64` of nanoseconds-since-boot. Rust's `Instant` is
//! itself implemented on top of `CLOCK_UPTIME_RAW` on Apple platforms,
//! so this is the same clock with a more useful API surface.

#![allow(non_upper_case_globals)]

use std::ops::{Add, Sub};
use std::time::Duration;

unsafe extern "C" {
    fn clock_gettime_nsec_np(clock_id: u32) -> u64;
}

/// `CLOCK_UPTIME_RAW` from `<time.h>`. Identical to `mach_absolute_time()`
/// after timebase conversion; doesn't advance during sleep.
const CLOCK_UPTIME_RAW: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(u64);

impl Timestamp {
    pub fn now() -> Self {
        Self(unsafe { clock_gettime_nsec_np(CLOCK_UPTIME_RAW) })
    }

    /// Construct from a raw nanosecond-since-boot value. Use when you
    /// already have a value measured against `CLOCK_UPTIME_RAW` (e.g.
    /// from a captured timeline) or when synthesizing one for the
    /// timestamp experiment.
    #[allow(dead_code)]
    pub fn from_nanos(ns: u64) -> Self {
        Self(ns)
    }

    /// Raw nanosecond-since-boot value, ready for `CGEventSetTimestamp`.
    pub fn as_nanos(self) -> u64 {
        self.0
    }

    /// Like `self - earlier` but clamps to `Duration::ZERO` if `earlier`
    /// is in the future. Mirrors `Instant::saturating_duration_since`.
    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

impl Sub for Timestamp {
    type Output = Duration;
    fn sub(self, rhs: Self) -> Duration {
        Duration::from_nanos(
            self.0
                .checked_sub(rhs.0)
                .expect("Timestamp - Timestamp went negative"),
        )
    }
}

impl Add<Duration> for Timestamp {
    type Output = Timestamp;
    fn add(self, rhs: Duration) -> Timestamp {
        Timestamp(self.0 + rhs.as_nanos() as u64)
    }
}

impl Sub<Duration> for Timestamp {
    type Output = Timestamp;
    fn sub(self, rhs: Duration) -> Timestamp {
        Timestamp(self.0 - rhs.as_nanos() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_is_monotonic() {
        let a = Timestamp::now();
        let b = Timestamp::now();
        assert!(b >= a);
    }

    #[test]
    fn add_then_sub_round_trips() {
        let t0 = Timestamp::from_nanos(1_000_000_000);
        let t1 = t0 + Duration::from_millis(50);
        assert_eq!(t1 - t0, Duration::from_millis(50));
        assert_eq!(t1 - Duration::from_millis(50), t0);
    }

    #[test]
    fn saturating_clamps_at_zero() {
        let t0 = Timestamp::from_nanos(100);
        let t1 = Timestamp::from_nanos(50);
        assert_eq!(t1.saturating_duration_since(t0), Duration::ZERO);
    }
}
