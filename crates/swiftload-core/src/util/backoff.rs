//! Exponential backoff with **full jitter**.
//!
//! Full jitter (`sleep = rand(0, min(cap, base * 2^n))`) rather than plain exponential is
//! load-bearing here: when a server drops all `k` connections at once, plain exponential
//! makes every worker retry in lockstep forever, re-creating the thundering herd on every
//! round. Full jitter spreads them out.

use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub base: Duration,
    pub cap: Duration,
    pub max_attempts: u32,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_millis(500),
            cap: Duration::from_secs(30),
            max_attempts: 8,
        }
    }
}

impl Backoff {
    /// Upper bound of the delay window for `attempt` (0-based), before jitter.
    pub fn ceiling_for(&self, attempt: u32) -> Duration {
        let shift = attempt.min(32);
        let scaled = self
            .base
            .checked_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .unwrap_or(self.cap);
        scaled.min(self.cap)
    }

    /// Delay for `attempt`, drawn uniformly from `[0, ceiling]`.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let ceiling = self.ceiling_for(attempt);
        let millis = ceiling.as_millis() as u64;
        if millis == 0 {
            return Duration::ZERO;
        }
        Duration::from_millis(rand::random_range(0..=millis))
    }

    pub fn exhausted(&self, attempt: u32) -> bool {
        attempt >= self.max_attempts
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ceiling_doubles_then_saturates_at_cap() {
        let b = Backoff::default();
        assert_eq!(b.ceiling_for(0), Duration::from_millis(500));
        assert_eq!(b.ceiling_for(1), Duration::from_millis(1000));
        assert_eq!(b.ceiling_for(2), Duration::from_millis(2000));
        assert_eq!(b.ceiling_for(6), Duration::from_secs(30)); // capped
        assert_eq!(b.ceiling_for(1000), Duration::from_secs(30)); // no overflow panic
    }

    #[test]
    fn delay_stays_within_the_window() {
        let b = Backoff::default();
        for attempt in 0..12 {
            for _ in 0..200 {
                assert!(b.delay_for(attempt) <= b.ceiling_for(attempt));
            }
        }
    }

    #[test]
    fn jitter_actually_spreads_retries() {
        // The whole point: repeated draws must not be identical, or the herd stays together.
        let b = Backoff::default();
        let draws: std::collections::HashSet<_> =
            (0..100).map(|_| b.delay_for(5).as_millis()).collect();
        assert!(
            draws.len() > 50,
            "insufficient jitter spread: {} distinct",
            draws.len()
        );
    }

    #[test]
    fn exhaustion() {
        let b = Backoff {
            max_attempts: 3,
            ..Default::default()
        };
        assert!(!b.exhausted(2));
        assert!(b.exhausted(3));
    }
}
