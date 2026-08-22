//! Adaptive concurrency.
//!
//! # The idea
//!
//! Naive downloaders open a fixed number of connections and hope. The trouble is that a
//! throughput plateau has several possible causes that demand *opposite* responses, and
//! aggregate throughput alone cannot tell them apart:
//!
//! | Observation on k -> 2k         | Cause                      | Correct response      |
//! |--------------------------------|----------------------------|-----------------------|
//! | `T(2k) ~ 2*T(k)`               | per-connection rate cap    | keep ramping          |
//! | `T(2k) ~ T(k)`                 | link or per-IP cap         | stop, record it       |
//! | `T(2k) < T(k)`                 | server penalises fan-out   | fall back below k     |
//! | 429/503 appear                 | rate limiting              | halve, honour the wait|
//! | write channel persistently full| disk-bound                 | cap; more won't help  |
//!
//! So the governor tracks **both** aggregate throughput and per-connection throughput, and
//! uses their ratio as the discriminator. That is the whole trick.
//!
//! # Why it is a pure state machine
//!
//! `observe()` takes a measurement and returns a decision. No I/O, no clock of its own, no
//! sockets. That makes every regime above testable against a synthetic trace, which is the
//! difference between believing the adaptive logic works and having tests that prove what it
//! does in each case.

use crate::config::{GovernorConfig, MIN_SEGMENT};
use std::time::{Duration, Instant};

/// One observation, assembled by the download task.
#[derive(Debug, Clone)]
pub struct Sample {
    pub now: Instant,
    /// Cumulative bytes for the whole download. Throughput is derived from the delta over a
    /// measurement window, rather than from a lagging EWMA.
    pub bytes_total: u64,
    /// Connections actually running.
    pub conns: usize,
    pub errors_since_last: u32,
    /// `Some(retry_after)` when the server asked us to slow down.
    pub rate_limited: Option<Option<Duration>>,
    /// The writer's channel has been full, meaning the disk cannot keep up.
    pub disk_backpressure: bool,
    pub remaining_bytes: u64,
    /// Only one download in the whole app may ramp at a time; without this, concurrent ramps
    /// read each other's growth as their own plateau and oscillate forever.
    pub probe_token: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Stay as we are.
    Hold,
    /// Grow to this many connections.
    SpawnTo(usize),
    /// Shrink to this many. Workers finish their current claim first; nothing is killed
    /// mid-range.
    RetireTo(usize),
    /// Back off after a rate limit, and wait before touching the server again.
    BackOff { conns: usize, wait: Duration },
    /// Ramping finished; another download may take the probe token.
    ReleaseToken(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Measuring the starting level.
    Baseline,
    /// Actively testing a higher level. Holds the probe token.
    Ramp,
    /// Settled. Re-probes occasionally, with an increasing interval.
    Hold,
    /// Backing off after a rate limit or error burst.
    BackOff,
}

/// Why the governor stopped growing — surfaced in diagnostics and learned per host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    StillRamping,
    /// Extra connections stopped paying: the pipe, not the server, is the limit.
    Saturated,
    /// Adding connections actively hurt.
    ServerPenalised,
    RateLimited,
    DiskBound,
    /// Nothing left to split.
    NotEnoughWork,
    ReachedCeiling,
}

struct Window {
    started: Instant,
    start_bytes: u64,
}

pub struct Governor {
    cfg: GovernorConfig,
    phase: Phase,
    /// Target connection count.
    k: usize,
    /// Hard ceiling learned at runtime (rate limits, disk, penalties).
    ceiling: usize,
    /// Best (level, throughput) measured so far.
    best: Option<(usize, f64)>,
    /// Throughput at the level we are comparing against.
    baseline: Option<(usize, f64)>,
    /// Set once the warmup after a change has elapsed.
    window: Option<Window>,
    changed_at: Instant,
    /// Midpoint retry after a failed doubling, so we do not discard 6 because 8 failed.
    mid_candidate: Option<usize>,
    last_probe: Instant,
    reprobe_interval: Duration,
    consecutive_backpressure: u32,
    /// Backpressure seen in the most recent sample. Distinct from the sustained counter:
    /// this blocks *growth* immediately, while the counter is what permanently caps.
    recent_backpressure: bool,
    pub stop_reason: StopReason,
    pub saturation_detected: bool,
}

impl Governor {
    pub fn new(cfg: GovernorConfig, k0: usize, now: Instant) -> Self {
        let reprobe = cfg.reprobe_interval;
        Self {
            ceiling: cfg.max_conns,
            cfg,
            phase: Phase::Baseline,
            k: k0.max(1),
            best: None,
            baseline: None,
            window: None,
            changed_at: now,
            mid_candidate: None,
            last_probe: now,
            reprobe_interval: reprobe,
            consecutive_backpressure: 0,
            recent_backpressure: false,
            stop_reason: StopReason::StillRamping,
            saturation_detected: false,
        }
    }

    pub fn target_conns(&self) -> usize {
        self.k
    }
    pub fn phase(&self) -> Phase {
        self.phase
    }
    pub fn ceiling(&self) -> usize {
        self.ceiling
    }
    /// True while this governor needs the app-wide probe token.
    pub fn wants_probe_token(&self) -> bool {
        matches!(self.phase, Phase::Baseline | Phase::Ramp)
    }

    /// Largest useful connection count given how much work is left. There is no point
    /// spawning a connection that would get less than a minimum segment to fetch.
    fn max_by_work(&self, remaining: u64) -> usize {
        ((remaining / MIN_SEGMENT) as usize).max(1)
    }

    fn change_to(&mut self, k: usize, now: Instant) {
        self.k = k;
        self.changed_at = now;
        self.window = None;
    }

    /// Feed one observation and get the next action.
    pub fn observe(&mut self, s: &Sample) -> Decision {
        // ── Pre-emptive back-off. Rate limiting outranks everything: never answer a limit
        //    with more connections, which is both abusive and slower.
        if let Some(retry_after) = s.rate_limited {
            let k = (self.k / 2).max(1);
            self.ceiling = k;
            self.saturation_detected = true;
            self.stop_reason = StopReason::RateLimited;
            self.phase = Phase::BackOff;
            self.change_to(k, s.now);
            return Decision::BackOff { conns: k, wait: retry_after.unwrap_or(Duration::from_secs(1)) };
        }

        // ── Disk-bound. More connections cannot help if the bytes cannot be written.
        self.recent_backpressure = s.disk_backpressure;
        if s.disk_backpressure {
            self.consecutive_backpressure += 1;
            if self.consecutive_backpressure >= 6 {
                self.ceiling = self.k;
                self.stop_reason = StopReason::DiskBound;
                self.phase = Phase::Hold;
                return Decision::Hold;
            }
        } else {
            self.consecutive_backpressure = 0;
        }

        // ── Error burst: shed one connection rather than hammering.
        if s.errors_since_last >= 2 && self.k > 1 {
            let k = self.k - 1;
            self.ceiling = self.ceiling.min(self.k);
            self.phase = Phase::Hold;
            self.change_to(k, s.now);
            return Decision::RetireTo(k);
        }

        match self.phase {
            Phase::BackOff => {
                // Stay put until something else moves us; the wait was already returned.
                self.phase = Phase::Hold;
                Decision::Hold
            }
            Phase::Baseline | Phase::Ramp => self.measure(s),
            Phase::Hold => self.maybe_reprobe(s),
        }
    }

    /// Accumulate a measurement window, then decide once it is complete.
    fn measure(&mut self, s: &Sample) -> Decision {
        // Discard the warmup: TCP slow-start and TLS make the first moments unrepresentative.
        if s.now.saturating_duration_since(self.changed_at) < self.cfg.warmup {
            return Decision::Hold;
        }
        let w = self.window.get_or_insert(Window { started: s.now, start_bytes: s.bytes_total });

        let elapsed = s.now.saturating_duration_since(w.started);
        if elapsed < self.cfg.dwell {
            return Decision::Hold;
        }

        let bps = (s.bytes_total.saturating_sub(w.start_bytes)) as f64 / elapsed.as_secs_f64();
        let level = self.k;
        self.window = None;
        self.changed_at = s.now;

        if self.best.is_none_or(|(_, b)| bps > b) {
            self.best = Some((level, bps));
        }

        match self.phase {
            Phase::Baseline => {
                self.baseline = Some((level, bps));
                self.phase = Phase::Ramp;
                self.try_grow(s)
            }
            Phase::Ramp => self.judge(level, bps, s),
            _ => Decision::Hold,
        }
    }

    /// Decide whether the level we just measured earned its extra connections.
    fn judge(&mut self, level: usize, bps: f64, s: &Sample) -> Decision {
        let Some((prev_k, prev_bps)) = self.baseline else {
            self.baseline = Some((level, bps));
            return self.try_grow(s);
        };

        let gain = if prev_bps > 0.0 { bps / prev_bps - 1.0 } else { 1.0 };
        // Per-connection throughput ratio: the discriminator. Near 1.0 means each new
        // connection is as productive as the old ones (a per-connection cap). Near 0.5 on a
        // doubling means we are just resharing a fixed pipe.
        let per_conn = if prev_bps > 0.0 && level > 0 {
            (bps / level as f64) / (prev_bps / prev_k as f64)
        } else {
            1.0
        };

        if gain >= self.cfg.gain_threshold {
            // It paid. Adopt this level and try for more.
            self.baseline = Some((level, bps));
            self.mid_candidate = None;
            return self.try_grow(s);
        }

        if per_conn <= self.cfg.saturation_ratio {
            self.saturation_detected = true;
            self.stop_reason = if gain < -0.05 { StopReason::ServerPenalised } else { StopReason::Saturated };
        } else {
            self.stop_reason = StopReason::Saturated;
        }

        // A doubling that did not pay does not prove the midpoint would not have. Try it once
        // before writing off the whole step.
        if let Some(mid) = self.mid_candidate.take() {
            // We were measuring the midpoint; keep whichever level measured best.
            let winner = if bps > prev_bps { mid } else { prev_k };
            let settle = winner.min(self.ceiling).max(1);
            self.phase = Phase::Hold;
            self.last_probe = s.now;
            self.ceiling = self.ceiling.min(level.saturating_sub(1).max(settle));
            self.change_to(settle, s.now);
            return Decision::ReleaseToken(settle);
        }

        let mid = (prev_k + level) / 2;
        if mid > prev_k && mid < level {
            self.mid_candidate = Some(mid);
            self.change_to(mid, s.now);
            return Decision::RetireTo(mid);
        }

        // No midpoint to try: settle back at the last level that paid.
        let settle = prev_k.min(self.ceiling).max(1);
        self.ceiling = self.ceiling.min(level.saturating_sub(1).max(settle));
        self.phase = Phase::Hold;
        self.last_probe = s.now;
        self.change_to(settle, s.now);
        Decision::ReleaseToken(settle)
    }

    /// Attempt the next level up, subject to every limit.
    fn try_grow(&mut self, s: &Sample) -> Decision {
        if !s.probe_token {
            // Someone else is ramping. Hold rather than measure through their interference.
            self.phase = Phase::Hold;
            return Decision::Hold;
        }
        if self.recent_backpressure {
            // The disk is already behind. Adding connections cannot move more bytes; it only
            // deepens the queue. Refuse to grow *now*, without waiting for the sustained
            // counter to confirm what this sample already shows.
            //
            // Deliberately does NOT lower the ceiling: a brief stall (an antivirus scan, a
            // competing write) must not permanently pin the download at its starting
            // concurrency. Only sustained backpressure, handled in `observe`, caps for good.
            // Releasing the token lets another download make progress, and the Hold phase
            // will re-probe once conditions settle.
            self.stop_reason = StopReason::DiskBound;
            self.phase = Phase::Hold;
            self.last_probe = s.now;
            return Decision::ReleaseToken(self.k);
        }
        let by_work = self.max_by_work(s.remaining_bytes);
        let limit = self.ceiling.min(self.cfg.max_conns).min(by_work);
        let next = (self.k * 2).min(limit);

        if next <= self.k {
            self.stop_reason = if by_work <= self.k { StopReason::NotEnoughWork } else { StopReason::ReachedCeiling };
            self.phase = Phase::Hold;
            self.last_probe = s.now;
            return Decision::ReleaseToken(self.k);
        }
        self.phase = Phase::Ramp;
        self.change_to(next, s.now);
        Decision::SpawnTo(next)
    }

    /// In steady state, occasionally check whether conditions improved — but back off the
    /// probing itself, so a stable download stops nagging the server.
    fn maybe_reprobe(&mut self, s: &Sample) -> Decision {
        if self.saturation_detected || self.stop_reason == StopReason::RateLimited {
            return Decision::Hold;
        }
        if !s.probe_token || s.errors_since_last > 0 {
            return Decision::Hold;
        }
        if s.now.saturating_duration_since(self.last_probe) < self.reprobe_interval {
            return Decision::Hold;
        }
        // Never re-probe near the end: the endgame wants fewer connections, not more.
        if s.remaining_bytes < MIN_SEGMENT * 4 {
            return Decision::Hold;
        }
        let limit = self.ceiling.min(self.cfg.max_conns).min(self.max_by_work(s.remaining_bytes));
        let next = (self.k + 2).min(limit);
        if next <= self.k {
            return Decision::Hold;
        }
        // Each unsuccessful probe doubles the interval, so a settled download quiets down.
        self.reprobe_interval *= 2;
        self.baseline = self.best;
        self.phase = Phase::Ramp;
        self.change_to(next, s.now);
        Decision::SpawnTo(next)
    }

    /// Endgame: with little work left, a swarm of connections just fights over the last few
    /// hundred KB and adds tail latency.
    pub fn endgame_target(&self, remaining: u64) -> Option<usize> {
        if remaining < MIN_SEGMENT {
            Some(1)
        } else if remaining < MIN_SEGMENT * 2 {
            Some(2.min(self.k))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drives a governor through a synthetic trace.
    ///
    /// `throughput` models the network: given a connection count it returns aggregate bytes
    /// per second. That is what lets us reproduce each regime exactly, with no network.
    struct Sim {
        gov: Governor,
        now: Instant,
        bytes: u64,
        conns: usize,
        remaining: u64,
    }

    impl Sim {
        fn new(cfg: GovernorConfig, k0: usize) -> Self {
            let now = Instant::now();
            Self {
                gov: Governor::new(cfg, k0, now),
                now,
                bytes: 0,
                conns: k0,
                remaining: 10_000_000_000,
            }
        }

        /// Advance time, accruing bytes at the rate the model says this connection count earns.
        fn run(&mut self, steps: usize, throughput: impl Fn(usize) -> f64) -> Vec<Decision> {
            let mut out = Vec::new();
            for _ in 0..steps {
                self.now += Duration::from_millis(200);
                let bps = throughput(self.conns);
                self.bytes += (bps * 0.2) as u64;
                self.remaining = self.remaining.saturating_sub((bps * 0.2) as u64);

                let s = Sample {
                    now: self.now,
                    bytes_total: self.bytes,
                    conns: self.conns,
                    errors_since_last: 0,
                    rate_limited: None,
                    disk_backpressure: false,
                    remaining_bytes: self.remaining,
                    probe_token: true,
                };
                let d = self.gov.observe(&s);
                match d {
                    Decision::SpawnTo(k) | Decision::RetireTo(k) | Decision::ReleaseToken(k) => {
                        self.conns = k;
                    }
                    Decision::BackOff { conns, .. } => self.conns = conns,
                    Decision::Hold => {}
                }
                if !matches!(d, Decision::Hold) {
                    out.push(d);
                }
            }
            out
        }
    }

    fn cfg() -> GovernorConfig {
        GovernorConfig { max_conns: 32, ..Default::default() }
    }

    #[test]
    fn per_connection_cap_ramps_up() {
        // Each connection gets 2 MB/s regardless of how many there are: parallelism pays
        // linearly, so the governor should keep going until it hits a limit.
        let mut sim = Sim::new(cfg(), 4);
        sim.run(200, |k| k as f64 * 2_000_000.0);

        assert!(
            sim.gov.target_conns() >= 16,
            "should have ramped up under a per-connection cap, settled at {}",
            sim.gov.target_conns()
        );
        assert!(!sim.gov.saturation_detected, "a per-connection cap is not saturation");
    }

    #[test]
    fn saturated_pipe_stops_and_is_recorded() {
        // A fixed 20 MB/s no matter how many connections: extra connections buy nothing.
        let mut sim = Sim::new(cfg(), 4);
        sim.run(200, |_| 20_000_000.0);

        assert!(
            sim.gov.target_conns() <= 8,
            "should not have ramped on a saturated pipe, got {}",
            sim.gov.target_conns()
        );
        assert!(sim.gov.saturation_detected, "saturation must be detected and remembered");
        assert_eq!(sim.gov.phase(), Phase::Hold);
    }

    #[test]
    fn server_that_penalises_fan_out_causes_a_retreat() {
        // Throughput actively degrades past 8 connections, as an overloaded or shaping server
        // would behave.
        let mut sim = Sim::new(cfg(), 4);
        sim.run(240, |k| if k <= 8 { k as f64 * 2_000_000.0 } else { 16_000_000.0 / (k as f64 / 8.0) });

        assert!(
            sim.gov.target_conns() <= 12,
            "should have retreated from a penalising server, got {}",
            sim.gov.target_conns()
        );
        assert!(sim.gov.saturation_detected);
    }

    #[test]
    fn rate_limiting_halves_concurrency_and_never_increases_it() {
        let mut sim = Sim::new(cfg(), 16);
        sim.conns = 16;
        sim.now += Duration::from_secs(2);
        let s = Sample {
            now: sim.now,
            bytes_total: 1_000_000,
            conns: 16,
            errors_since_last: 0,
            rate_limited: Some(Some(Duration::from_secs(5))),
            disk_backpressure: false,
            remaining_bytes: 1_000_000_000,
            probe_token: true,
        };
        let d = sim.gov.observe(&s);
        assert_eq!(d, Decision::BackOff { conns: 8, wait: Duration::from_secs(5) });
        assert_eq!(sim.gov.ceiling(), 8, "the ceiling must come down and stay down");
        assert_eq!(sim.gov.stop_reason, StopReason::RateLimited);

        // And it must never climb back on its own after a rate limit.
        let mut sim2 = Sim::new(cfg(), 8);
        sim2.gov = sim.gov;
        sim2.run(200, |k| k as f64 * 5_000_000.0);
        assert!(sim2.gov.target_conns() <= 8, "climbed back to {} after a 429", sim2.gov.target_conns());
    }

    #[test]
    fn disk_backpressure_prevents_growth_and_caps_the_ceiling() {
        // A disk that cannot keep up is not fixed by opening more sockets. Crucially the
        // governor must refuse to grow on the *first* sign of backpressure, not after the
        // sustained-confirmation window, or it will have already spawned connections the disk
        // cannot feed.
        let mut sim = Sim::new(cfg(), 8);
        for _ in 0..20 {
            sim.now += Duration::from_millis(500);
            sim.bytes += 1_000_000;
            let s = Sample {
                now: sim.now,
                bytes_total: sim.bytes,
                conns: sim.conns,
                errors_since_last: 0,
                rate_limited: None,
                disk_backpressure: true,
                remaining_bytes: 5_000_000_000,
                probe_token: true,
            };
            let d = sim.gov.observe(&s);
            assert!(!matches!(d, Decision::SpawnTo(_)), "grew while disk-bound: {d:?}");
        }
        assert_eq!(sim.gov.stop_reason, StopReason::DiskBound);
        assert_eq!(sim.gov.target_conns(), 8, "must stay where it started");
        assert!(sim.gov.ceiling() <= 8, "disk-bound must cap the ceiling");
    }

    #[test]
    fn a_brief_stall_does_not_permanently_pin_concurrency() {
        // Backpressure is often transient — an antivirus scan, a competing write. A blip must
        // block growth for that moment without capping the download for its whole lifetime.
        let mut sim = Sim::new(cfg(), 4);
        let ceiling_before = sim.gov.ceiling();

        // Three samples spaced widely enough to complete a measurement window, but well short
        // of the sustained-backpressure threshold.
        for _ in 0..3 {
            sim.now += Duration::from_millis(700);
            sim.bytes += 1_400_000;
            let s = Sample {
                now: sim.now,
                bytes_total: sim.bytes,
                conns: sim.conns,
                errors_since_last: 0,
                rate_limited: None,
                disk_backpressure: true,
                remaining_bytes: 5_000_000_000,
                probe_token: true,
            };
            let d = sim.gov.observe(&s);
            assert!(!matches!(d, Decision::SpawnTo(_)), "grew during a stall: {d:?}");
        }

        assert_eq!(
            sim.gov.ceiling(),
            ceiling_before,
            "a transient stall must not lower the ceiling — only sustained backpressure does"
        );
    }

    #[test]
    fn error_bursts_shed_a_connection() {
        let mut sim = Sim::new(cfg(), 8);
        sim.now += Duration::from_secs(3);
        let s = Sample {
            now: sim.now,
            bytes_total: 5_000_000,
            conns: 8,
            errors_since_last: 3,
            rate_limited: None,
            disk_backpressure: false,
            remaining_bytes: 1_000_000_000,
            probe_token: true,
        };
        assert_eq!(sim.gov.observe(&s), Decision::RetireTo(7));
    }

    #[test]
    fn does_not_ramp_without_the_probe_token() {
        // Two downloads ramping at once would each read the other's growth as their own
        // plateau, so only one may hold the token.
        let cfgv = cfg();
        let now = Instant::now();
        let mut g = Governor::new(cfgv, 4, now);
        let mut bytes = 0u64;
        let mut t = now;
        for _ in 0..40 {
            t += Duration::from_millis(200);
            bytes += 2_000_000;
            let s = Sample {
                now: t,
                bytes_total: bytes,
                conns: 4,
                errors_since_last: 0,
                rate_limited: None,
                disk_backpressure: false,
                remaining_bytes: 5_000_000_000,
                probe_token: false,
            };
            let d = g.observe(&s);
            assert!(!matches!(d, Decision::SpawnTo(_)), "ramped without the token: {d:?}");
        }
        assert_eq!(g.target_conns(), 4);
    }

    #[test]
    fn does_not_split_beyond_the_work_available() {
        // 6 MB left cannot usefully occupy 32 connections at a 2 MB minimum segment.
        let mut sim = Sim::new(cfg(), 4);
        sim.remaining = 6 * 1024 * 1024;
        sim.run(60, |k| k as f64 * 10_000_000.0);
        assert!(sim.gov.target_conns() <= 4, "over-split tiny remainder to {}", sim.gov.target_conns());
    }

    #[test]
    fn endgame_reduces_connections() {
        let g = Governor::new(cfg(), 16, Instant::now());
        assert_eq!(g.endgame_target(1024), Some(1), "one connection for the last KB");
        assert_eq!(g.endgame_target(3 * 1024 * 1024), Some(2));
        assert_eq!(g.endgame_target(500 * 1024 * 1024), None, "plenty of work left");
    }

    #[test]
    fn settles_rather_than_oscillating() {
        // The failure mode that makes adaptive concurrency worse than a fixed guess: the
        // level must stop changing once conditions are stable.
        let mut sim = Sim::new(cfg(), 4);
        sim.run(120, |_| 20_000_000.0);
        let settled = sim.gov.target_conns();

        let changes = sim.run(400, |_| 20_000_000.0);
        assert!(
            changes.len() <= 2,
            "governor kept changing its mind {} times after settling: {:?}",
            changes.len(),
            changes
        );
        assert_eq!(sim.gov.target_conns(), settled);
    }

    #[test]
    fn adaptive_lands_near_the_best_fixed_level() {
        // The real test of the whole idea: adaptive must never be much worse than a good
        // fixed choice. Here throughput scales to 8 connections and then flattens, so the
        // best fixed level is 8.
        let mut sim = Sim::new(cfg(), 4);
        sim.run(300, |k| (k.min(8) as f64) * 2_000_000.0);

        let settled = sim.gov.target_conns();
        assert!(
            (6..=16).contains(&settled),
            "settled at {settled}, which is far from the optimum of 8"
        );
    }
}
