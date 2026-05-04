//! Map device-side `scan_time_100us` (u16, 100µs ticks, wraps every
//! ~6.55 s) to host [`Timestamp`]s on the `CLOCK_UPTIME_RAW` clock.
//!
//! The estimator tracks the offset between scan-time and host-time
//! across a sliding window of recent samples and uses the *minimum*
//! offset in the window to convert each frame's scan_time. The minimum
//! represents the "fastest delivery" path; anything larger is
//! buffering / jitter from USB, the kernel HID queue, or our own
//! callback latency. Per-frame deltas of the aligned timestamps then
//! match the device's scan-time deltas exactly (modulo MCU↔host clock
//! drift), immune to delivery jitter.
//!
//! # Sessions
//!
//! A *session* is a run of consecutive `observe()` calls whose
//! inter-call gap (in host time) stays below [`RESET_GAP`]. The first
//! call ever, and the first call after a > [`RESET_GAP`] gap, starts a
//! new session and discards any prior state — its scan_time becomes
//! the new baseline and its offset is the only sample in the window.
//! Within a session we assume the chip's free-running scan_time
//! counter is monotonic with at most one u16 wrap per inter-frame gap
//! (which holds trivially while frames arrive every few milliseconds).
//! Across a session boundary an unknown number of wraps may have
//! happened on the chip side, so we don't try to span the gap; the
//! new session just rebaselines.
//!
//! In practice a session typically starts when the user touches the
//! pad and ends a few seconds after they lift, but [`ScanTimeClock`]
//! has no notion of touch state — sessions are defined purely by the
//! host-time gap between observe() calls, regardless of what the
//! gesture engine is doing.
//!
//! # Why we want this
//!
//! The gesture engine reads `now − prev_t` to derive velocity. Any
//! delivery jitter — USB stall, kernel HID-queue contention, our own
//! callback latency — that delays a frame's *arrival* relative to its
//! *scan* inflates that frame's dt and skews the velocity computation
//! at the moment of the jitter. A scan-time-derived timestamp gives
//! the gesture engine a dt sequence that reflects the chip's view of
//! reality regardless of how the report was delivered to us. Whether
//! this is a perceptible improvement is unverified — it's a
//! defensive substitution against a known potential noise source, not
//! a fix for a measured bug. It costs nothing functional downstream:
//! CGEvent consumers don't read CGEvent timestamps for any visible
//! decision (see the cgevent timestamp memory).

use crate::time::Timestamp;
use std::collections::VecDeque;
use std::time::Duration;

/// Sliding-window size in samples. ~1 second at the typical 125 Hz
/// frame rate is long enough to span a single bursty USB delivery (the
/// minimum-offset estimator stays anchored on the fastest pre-burst
/// frame) but short enough that MCU↔host clock drift doesn't visibly
/// accumulate within the window.
const WINDOW: usize = 128;

/// Inter-frame host-time gap above which we treat the next observe()
/// call as the start of a new session (see module-level "Sessions").
/// Far below the u16 wrap interval (~6.55 s) so a session-boundary
/// rebaseline never needs to track multiple wraps across the gap. In
/// active use, frame intervals are single-digit ms.
const RESET_GAP: Duration = Duration::from_secs(4);

pub struct ScanTimeClock {
    /// Extended scan time in 100 µs ticks. Increments by the raw u16
    /// delta (mod 65536) each frame within a session; rebaselined to
    /// the raw value at the start of each new session.
    scan_total_ticks: u64,
    last_now: Option<Timestamp>,
    /// `host_ns − scan_ns` samples for recent frames. Minimum is the
    /// fastest-delivery offset; we use it to align scan_time to host.
    offsets: VecDeque<i64>,
}

impl ScanTimeClock {
    pub fn new() -> Self {
        Self {
            scan_total_ticks: 0,
            last_now: None,
            offsets: VecDeque::with_capacity(WINDOW),
        }
    }

    /// Observe a frame at host time `now` whose device-side scan_time
    /// is `raw` (100 µs ticks). Returns the host-aligned [`Timestamp`]
    /// best representing when the chip actually scanned the frame.
    pub fn observe(&mut self, raw: u16, now: Timestamp) -> Timestamp {
        let need_reset = match self.last_now {
            None => true,
            Some(last) => now.saturating_duration_since(last) > RESET_GAP,
        };

        if need_reset {
            self.offsets.clear();
            self.scan_total_ticks = raw as u64;
        } else {
            let prev_raw = (self.scan_total_ticks & 0xFFFF) as u16;
            let delta = raw.wrapping_sub(prev_raw) as u64;
            self.scan_total_ticks = self.scan_total_ticks.wrapping_add(delta);
        }
        self.last_now = Some(now);

        let scan_ns = (self.scan_total_ticks as i64).saturating_mul(100_000);
        let now_ns = now.as_nanos() as i64;
        let offset = now_ns - scan_ns;

        if self.offsets.len() == WINDOW {
            self.offsets.pop_front();
        }
        self.offsets.push_back(offset);
        let min_offset = *self.offsets.iter().min().expect("just pushed");

        Timestamp::from_nanos((scan_ns + min_offset) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Timestamp {
        // Plausible mid-uptime value; arithmetic is what matters.
        Timestamp::from_nanos(1_000_000_000_000)
    }

    #[test]
    fn first_frame_aligns_to_now() {
        let mut c = ScanTimeClock::new();
        let now = t0();
        // First sample: only one offset in the window, so the
        // minimum-offset → aligned-time round-trip recovers `now`.
        let ts = c.observe(1234, now);
        assert_eq!(ts, now);
    }

    #[test]
    fn steady_cadence_preserves_deltas() {
        let mut c = ScanTimeClock::new();
        let _ = c.observe(0, t0());
        let _ = c.observe(80, t0() + Duration::from_millis(8));
        let ts = c.observe(160, t0() + Duration::from_millis(16));
        // Each scan tick = 100 µs; 160 ticks = 16 ms past the baseline.
        assert_eq!(ts, t0() + Duration::from_millis(16));
    }

    #[test]
    fn delayed_frame_doesnt_pull_aligned_forward() {
        // Three frames at steady scan-time cadence (8 ms apart). The
        // third frame's host-time arrival is delayed by 50 ms (USB
        // stall on the lift transition). Aligned timestamp must reflect
        // the *scan* instant (~16 ms past start), not the delivery
        // instant (~66 ms past start).
        let mut c = ScanTimeClock::new();
        let _ = c.observe(0, t0());
        let _ = c.observe(80, t0() + Duration::from_millis(8));
        let ts = c.observe(160, t0() + Duration::from_millis(66));
        let expected = t0() + Duration::from_millis(16);
        let diff_ns = ts.as_nanos() as i64 - expected.as_nanos() as i64;
        assert!(
            diff_ns.abs() < 1000,
            "delayed frame pulled aligned by {diff_ns} ns",
        );
    }

    #[test]
    fn u16_wrap_within_session_is_handled() {
        let mut c = ScanTimeClock::new();
        // Start near the wrap point.
        let _ = c.observe(65_500, t0());
        // 100 ticks later: raw wraps from 65_500 → 64 (= 65_600 mod 65_536).
        let ts = c.observe(64, t0() + Duration::from_millis(10));
        let expected = t0() + Duration::from_millis(10);
        let diff_ns = ts.as_nanos() as i64 - expected.as_nanos() as i64;
        assert!(diff_ns.abs() < 1000, "wrap mishandled: diff {diff_ns} ns");
    }

    #[test]
    fn long_gap_starts_new_session() {
        let mut c = ScanTimeClock::new();
        let _ = c.observe(0, t0());
        let _ = c.observe(80, t0() + Duration::from_millis(8));
        // 5 s gap > RESET_GAP. The next frame's raw scan_time bears no
        // continuous relationship to the last (the chip's u16 counter
        // may have wrapped multiple times), so the estimator treats it
        // as a fresh session: prior offsets are discarded and the new
        // raw value becomes the baseline.
        let ts = c.observe(12_345, t0() + Duration::from_secs(5));
        // First frame of a new session aligns to `now`.
        assert_eq!(ts, t0() + Duration::from_secs(5));
    }
}
