//! Throughput measurement.
//!
//! "Peak speed" is meaningless without a stated window — an undefined peak is just the
//! largest noise spike you happened to sample. So this module defines the three numbers the
//! UI shows, precisely:
//!
//!   * **current** — EWMA over roughly `EWMA_WINDOW`, smooth enough to read
//!   * **average** — total bytes / active seconds (paused time excluded by the caller)
//!   * **peak**    — max over *non-overlapping 1-second windows*
//!
//! All methods take an explicit `now`, so every behaviour here is unit-testable without
//! sleeping.

use std::time::{Duration, Instant};

const EWMA_WINDOW: Duration = Duration::from_secs(3);
const PEAK_WINDOW: Duration = Duration::from_secs(1);

/// Tracks bytes over time and reports current / peak throughput.
#[derive(Debug)]
pub struct SpeedMeter {
    ewma_bps: f64,
    last_update: Option<Instant>,
    /// Accumulator for the in-progress 1-second peak window.
    window_start: Option<Instant>,
    window_bytes: u64,
    peak_bps: u64,
    total_bytes: u64,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl SpeedMeter {
    pub fn new() -> Self {
        Self {
            ewma_bps: 0.0,
            last_update: None,
            window_start: None,
            window_bytes: 0,
            peak_bps: 0,
            total_bytes: 0,
        }
    }

    /// Record `bytes` observed at `now`.
    pub fn record(&mut self, bytes: u64, now: Instant) {
        self.total_bytes += bytes;
        self.window_bytes += bytes;

        // EWMA, with the smoothing factor derived from elapsed time so the constant means
        // the same thing regardless of how often we happen to be called.
        if let Some(last) = self.last_update {
            let dt = now.saturating_duration_since(last).as_secs_f64();
            if dt > 0.0 {
                let inst = bytes as f64 / dt;
                let alpha = 1.0 - (-dt / EWMA_WINDOW.as_secs_f64()).exp();
                self.ewma_bps += alpha * (inst - self.ewma_bps);
            }
        }
        self.last_update = Some(now);

        match self.window_start {
            None => self.window_start = Some(now),
            Some(start) if now.saturating_duration_since(start) >= PEAK_WINDOW => {
                // Close the window and normalize to bytes-per-second, since the window may
                // have overshot 1s slightly.
                let elapsed = now.saturating_duration_since(start).as_secs_f64();
                if elapsed > 0.0 {
                    let bps = (self.window_bytes as f64 / elapsed) as u64;
                    self.peak_bps = self.peak_bps.max(bps);
                }
                self.window_start = Some(now);
                self.window_bytes = 0;
            }
            Some(_) => {}
        }
    }

    /// Decay the EWMA toward zero when no bytes arrive, so a stalled connection reads as
    /// slow rather than frozen at its last good value.
    pub fn tick(&mut self, now: Instant) {
        if let Some(last) = self.last_update {
            let dt = now.saturating_duration_since(last).as_secs_f64();
            if dt > 0.0 {
                let alpha = 1.0 - (-dt / EWMA_WINDOW.as_secs_f64()).exp();
                self.ewma_bps += alpha * (0.0 - self.ewma_bps);
                self.last_update = Some(now);
            }
        }
    }

    pub fn current_bps(&self) -> u64 {
        self.ewma_bps.max(0.0) as u64
    }

    pub fn peak_bps(&self) -> u64 {
        self.peak_bps
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// Average over the supplied active duration (the caller excludes paused/queued time).
    pub fn average_bps(&self, active: Duration) -> u64 {
        let secs = active.as_secs_f64();
        if secs <= 0.0 {
            return 0;
        }
        (self.total_bytes as f64 / secs) as u64
    }

    /// Seconds remaining at the current rate, or `None` when unknown or stalled.
    pub fn eta(&self, remaining_bytes: u64) -> Option<Duration> {
        let bps = self.current_bps();
        if bps == 0 {
            return None;
        }
        Some(Duration::from_secs_f64(remaining_bytes as f64 / bps as f64))
    }
}

/// A token bucket, used today by nothing and tomorrow by the speed limiter.
///
/// The `RateLimiter` seam exists from the start (see `config::RateLimit`) so that adding a
/// bandwidth cap later is a config change rather than a rewrite of the worker read loop.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: u64,
    tokens: f64,
    refill_per_sec: u64,
    last: Option<Instant>,
}

impl TokenBucket {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            capacity: bytes_per_sec.max(1),
            tokens: bytes_per_sec as f64,
            refill_per_sec: bytes_per_sec,
            last: None,
        }
    }

    /// Take up to `want` bytes; returns how many are allowed right now (possibly zero).
    pub fn take(&mut self, want: u64, now: Instant) -> u64 {
        if let Some(last) = self.last {
            let dt = now.saturating_duration_since(last).as_secs_f64();
            self.tokens = (self.tokens + dt * self.refill_per_sec as f64).min(self.capacity as f64);
        }
        self.last = Some(now);
        let granted = want.min(self.tokens as u64);
        self.tokens -= granted as f64;
        granted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn peak_uses_non_overlapping_one_second_windows() {
        let start = t0();
        let mut m = SpeedMeter::new();

        // Second 1: 10 MB. Second 2: 1 MB. Peak must reflect the fast window only.
        m.record(10_000_000, start);
        m.record(0, start + Duration::from_millis(1000));
        let after_first = m.peak_bps();
        assert!(after_first >= 9_000_000, "peak was {after_first}");

        m.record(1_000_000, start + Duration::from_millis(1500));
        m.record(0, start + Duration::from_millis(2000));
        assert_eq!(m.peak_bps(), after_first, "a slower window must not lower the peak");
    }

    #[test]
    fn a_single_burst_does_not_become_the_peak_forever() {
        // The point of windowing: an instantaneous burst is the OS receive buffer, not the
        // network, so it must be amortized across its window rather than reported raw.
        let start = t0();
        let mut m = SpeedMeter::new();
        m.record(1_000_000, start); // 1 MB delivered "instantly"
        m.record(0, start + Duration::from_secs(1));
        assert!(m.peak_bps() <= 1_100_000, "burst inflated peak to {}", m.peak_bps());
    }

    #[test]
    fn average_excludes_time_the_caller_did_not_count() {
        let start = t0();
        let mut m = SpeedMeter::new();
        m.record(100_000_000, start);
        // 100 MB over 10 active seconds = 10 MB/s, regardless of wall-clock pauses.
        assert_eq!(m.average_bps(Duration::from_secs(10)), 10_000_000);
        assert_eq!(m.average_bps(Duration::ZERO), 0);
    }

    #[test]
    fn current_speed_converges_to_the_true_rate() {
        let start = t0();
        let mut m = SpeedMeter::new();
        // Feed a steady 10 MB/s for 20 s in 100 ms slices.
        for i in 1..=200 {
            m.record(1_000_000, start + Duration::from_millis(i * 100));
        }
        let cur = m.current_bps();
        assert!(
            (9_000_000..=11_000_000).contains(&cur),
            "EWMA settled at {cur}, expected ~10 MB/s"
        );
    }

    #[test]
    fn tick_decays_a_stalled_connection_toward_zero() {
        let start = t0();
        let mut m = SpeedMeter::new();
        for i in 1..=50 {
            m.record(1_000_000, start + Duration::from_millis(i * 100));
        }
        let before = m.current_bps();
        assert!(before > 1_000_000);

        m.tick(start + Duration::from_secs(30));
        assert!(m.current_bps() < before / 100, "stalled meter stayed at {}", m.current_bps());
    }

    #[test]
    fn eta_is_none_when_stalled() {
        let m = SpeedMeter::new();
        assert_eq!(m.eta(1_000_000), None);
    }

    #[test]
    fn eta_from_current_rate() {
        let start = t0();
        let mut m = SpeedMeter::new();
        for i in 1..=100 {
            m.record(1_000_000, start + Duration::from_millis(i * 100));
        }
        let eta = m.eta(100_000_000).expect("rate is known");
        // ~10 MB/s over 100 MB remaining ≈ 10 s.
        assert!((8..=13).contains(&eta.as_secs()), "eta was {:?}", eta);
    }

    #[test]
    fn token_bucket_limits_then_refills() {
        let start = t0();
        let mut b = TokenBucket::new(1_000_000); // 1 MB/s
        assert_eq!(b.take(500_000, start), 500_000);
        assert_eq!(b.take(1_000_000, start), 500_000, "only the remaining tokens");
        assert_eq!(b.take(1_000, start), 0, "bucket is empty");

        // Half a second later, half a megabyte has refilled.
        let granted = b.take(1_000_000, start + Duration::from_millis(500));
        assert!((450_000..=550_000).contains(&granted), "granted {granted}");
    }
}
