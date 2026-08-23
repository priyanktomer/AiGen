//! Scheduling across downloads.
//!
//! One download's concurrency is the Governor's problem. This module handles the level above:
//! how many downloads run at once, in what order, and how the connection budget is divided
//! between them.
//!
//! # Why this is a pure state machine
//!
//! Like [`crate::task::governor`], `Scheduler` performs no I/O and owns no clock. It holds the
//! queue and the running set, and [`Scheduler::poll`] answers one question — *which downloads
//! should start now?* — from that state alone. The caller does the spawning. That keeps every
//! admission rule below testable without a network, a disk, or a timing window.
//!
//! # The three limits, and why they are not the same limit
//!
//! | Limit | Protects | Symptom when missing |
//! |---|---|---|
//! | `max_concurrent_downloads` | the user's attention and disk head | ten half-finished files, none soon |
//! | `max_conns_per_host` | the *server*, and our standing with it | 429s, then a tarpit or a ban |
//! | `max_total_conns` | the local link and file handles | every download slow, none identifiably at fault |
//!
//! A per-host cap cannot substitute for a global one: three downloads from three hosts at
//! eight connections each is twenty-four sockets, which no single host limit would catch.
//!
//! # Fair share, and the honest limitation in it
//!
//! When a download is admitted its connection ceiling is set to its fair share of the host
//! budget — `max_conns_per_host / (downloads already on that host + 1)`. Two downloads from
//! one host therefore get four connections each rather than the first one taking all eight.
//!
//! The share is computed **at admission and not revised afterwards**, because
//! [`crate::task::download`] takes its ceiling once, at the start. So when the first of two
//! downloads finishes, the second keeps the four it was given instead of growing back to
//! eight. That costs some throughput at the tail and never costs correctness; raising a
//! running download's ceiling needs a control channel into the task, which is roadmap
//! (`docs/PLAN.md` §L) rather than something to fake here.

use crate::task::ProbeToken;
use std::collections::HashMap;

pub use crate::store::models::Priority;

/// The limits a [`Scheduler`] enforces. Mirrors the user-facing settings of the same names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_concurrent_downloads: usize,
    pub max_conns_per_download: usize,
    pub max_conns_per_host: usize,
    pub max_total_conns: usize,
}

impl Limits {
    pub fn from_settings(s: &crate::config::Settings) -> Self {
        Self {
            max_concurrent_downloads: s.max_concurrent_downloads.max(1),
            max_conns_per_download: s.max_conns_per_download.max(1),
            max_conns_per_host: s.max_conns_per_host.max(1),
            max_total_conns: s.max_total_conns.max(1),
        }
    }
}

/// A download cleared to start, with the connection ceiling it was granted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub id: String,
    /// Pass to `DownloadRequest::max_conns`. Already clamped by every limit in force.
    pub max_conns: usize,
}

#[derive(Debug, Clone)]
struct QueuedItem {
    id: String,
    host: String,
    priority: Priority,
    /// Tie-break within a priority. Ascending, so ordinary enqueues are FIFO; `start_now`
    /// assigns descending negatives to jump the whole queue without inventing a priority
    /// above `High`.
    seq: i64,
}

#[derive(Debug, Clone)]
struct Running {
    host: String,
    /// Connections currently open. Starts at one and tracks reality as progress is reported;
    /// a stale-low value can only ever admit *more* work, never overshoot a hard cap, because
    /// admission also checks the download count.
    conns: usize,
}

pub struct Scheduler {
    queued: Vec<QueuedItem>,
    running: HashMap<String, Running>,
    limits: Limits,
    probe: ProbeToken,
    next_seq: i64,
    next_front_seq: i64,
}

impl Scheduler {
    pub fn new(limits: Limits) -> Self {
        Self {
            queued: Vec::new(),
            running: HashMap::new(),
            limits,
            probe: ProbeToken::new(),
            next_seq: 0,
            next_front_seq: 0,
        }
    }

    /// The app-wide ramp permit. Clone it onto every `DownloadRequest` the manager builds:
    /// that is what stops two downloads from ramping into each other.
    pub fn probe_token(&self) -> ProbeToken {
        self.probe.clone()
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Apply changed settings. Lowering a limit never stops a running download — killing a
    /// transfer that is already moving to satisfy a preference the user just expressed would
    /// destroy work to enforce a number. The new limit binds the next admission instead.
    pub fn set_limits(&mut self, limits: Limits) {
        self.limits = limits;
    }

    pub fn enqueue(&mut self, id: impl Into<String>, host: impl Into<String>, priority: Priority) {
        let id = id.into();
        self.queued.retain(|q| q.id != id);
        self.next_seq += 1;
        self.queued.push(QueuedItem {
            id,
            host: host.into(),
            priority,
            seq: self.next_seq,
        });
    }

    /// Record a download the caller started outside `poll` — a resume the user asked for
    /// directly, for instance. Keeps the budget honest about what is actually running.
    pub fn mark_running(&mut self, id: impl Into<String>, host: impl Into<String>) {
        let id = id.into();
        self.queued.retain(|q| q.id != id);
        self.running.insert(
            id,
            Running {
                host: host.into(),
                conns: 1,
            },
        );
    }

    pub fn set_priority(&mut self, id: &str, priority: Priority) -> bool {
        match self.queued.iter_mut().find(|q| q.id == id) {
            Some(q) => {
                q.priority = priority;
                true
            }
            None => false,
        }
    }

    /// Move a queued download to the head of the queue.
    ///
    /// Deliberately does not pre-empt anything already running: stopping a transfer mid-flight
    /// to start another one throws away an open connection and a warm ramp, and the user asked
    /// for this download sooner, not for that one to be punished.
    pub fn start_now(&mut self, id: &str) -> bool {
        self.next_front_seq -= 1;
        let seq = self.next_front_seq;
        match self.queued.iter_mut().find(|q| q.id == id) {
            Some(q) => {
                q.priority = Priority::High;
                q.seq = seq;
                true
            }
            None => false,
        }
    }

    /// Drop a download from both the queue and the running set.
    pub fn remove(&mut self, id: &str) {
        self.queued.retain(|q| q.id != id);
        self.running.remove(id);
    }

    /// A running download stopped, for any reason. Frees its budget.
    pub fn finished(&mut self, id: &str) {
        self.running.remove(id);
    }

    /// Report a running download's live connection count, so the global budget reflects what
    /// is actually open rather than what was granted.
    pub fn update_conns(&mut self, id: &str, conns: usize) {
        if let Some(r) = self.running.get_mut(id) {
            r.conns = conns;
        }
    }

    pub fn is_running(&self, id: &str) -> bool {
        self.running.contains_key(id)
    }

    pub fn running_count(&self) -> usize {
        self.running.len()
    }

    pub fn queued_count(&self) -> usize {
        self.queued.len()
    }

    /// Ids in the order they will be admitted.
    pub fn queue_order(&self) -> Vec<String> {
        let mut q: Vec<&QueuedItem> = self.queued.iter().collect();
        q.sort_by(|a, b| b.priority.cmp(&a.priority).then(a.seq.cmp(&b.seq)));
        q.into_iter().map(|q| q.id.clone()).collect()
    }

    /// Zero-based position in the queue, for the Queue view.
    pub fn queue_position(&self, id: &str) -> Option<usize> {
        self.queue_order().iter().position(|q| q == id)
    }

    fn conns_on_host(&self, host: &str) -> usize {
        self.running
            .values()
            .filter(|r| r.host == host)
            .map(|r| r.conns)
            .sum()
    }

    fn downloads_on_host(&self, host: &str) -> usize {
        self.running.values().filter(|r| r.host == host).count()
    }

    fn total_conns(&self) -> usize {
        self.running.values().map(|r| r.conns).sum()
    }

    /// Everything that may start right now, best first.
    ///
    /// Admits repeatedly until a limit binds, so one call after a download finishes fills
    /// every slot that freed rather than one per tick.
    pub fn poll(&mut self) -> Vec<Admission> {
        let mut admitted = Vec::new();

        loop {
            if self.running.len() >= self.limits.max_concurrent_downloads {
                break;
            }
            if self.total_conns() >= self.limits.max_total_conns {
                break;
            }

            // Best-first, but skip past any candidate whose host is already at its cap: one
            // busy host must not block the queue for every other host behind it.
            let order = self.queue_order();
            let Some(next) = order.into_iter().find(|id| {
                self.queued
                    .iter()
                    .find(|q| &q.id == id)
                    .is_some_and(|q| self.conns_on_host(&q.host) < self.limits.max_conns_per_host)
            }) else {
                break;
            };

            let pos = self.queued.iter().position(|q| q.id == next).unwrap();
            let item = self.queued.remove(pos);

            let share =
                (self.limits.max_conns_per_host / (self.downloads_on_host(&item.host) + 1)).max(1);
            let free_total = self
                .limits
                .max_total_conns
                .saturating_sub(self.total_conns())
                .max(1);
            let max_conns = share
                .min(free_total)
                .min(self.limits.max_conns_per_download)
                .max(1);

            self.running.insert(
                item.id.clone(),
                Running {
                    host: item.host,
                    conns: 1,
                },
            );
            admitted.push(Admission {
                id: item.id,
                max_conns,
            });
        }

        admitted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> Limits {
        Limits {
            max_concurrent_downloads: 2,
            max_conns_per_download: 8,
            max_conns_per_host: 8,
            max_total_conns: 24,
        }
    }

    fn ids(a: &[Admission]) -> Vec<&str> {
        a.iter().map(|x| x.id.as_str()).collect()
    }

    #[test]
    fn admits_up_to_the_concurrency_limit() {
        let mut s = Scheduler::new(limits());
        for i in 0..5 {
            s.enqueue(format!("d{i}"), format!("h{i}.example"), Priority::Normal);
        }
        assert_eq!(ids(&s.poll()), ["d0", "d1"]);
        // Full: a second poll must not admit more.
        assert!(s.poll().is_empty());
        assert_eq!(s.queued_count(), 3);
    }

    #[test]
    fn finishing_one_admits_exactly_one_more() {
        let mut s = Scheduler::new(limits());
        for i in 0..4 {
            s.enqueue(format!("d{i}"), format!("h{i}.example"), Priority::Normal);
        }
        s.poll();
        s.finished("d0");
        assert_eq!(ids(&s.poll()), ["d2"]);
    }

    #[test]
    fn a_freed_slot_is_filled_in_one_poll_not_one_per_tick() {
        let mut s = Scheduler::new(Limits {
            max_concurrent_downloads: 3,
            ..limits()
        });
        for i in 0..6 {
            s.enqueue(format!("d{i}"), format!("h{i}.example"), Priority::Normal);
        }
        s.poll();
        s.finished("d0");
        s.finished("d1");
        assert_eq!(ids(&s.poll()), ["d3", "d4"]);
    }

    #[test]
    fn priority_beats_arrival_order_and_ties_stay_fifo() {
        let mut s = Scheduler::new(Limits {
            max_concurrent_downloads: 1,
            ..limits()
        });
        s.enqueue("first", "a.example", Priority::Normal);
        s.enqueue("second", "b.example", Priority::Normal);
        s.enqueue("urgent", "c.example", Priority::High);
        s.enqueue("later", "d.example", Priority::Low);
        assert_eq!(s.queue_order(), ["urgent", "first", "second", "later"]);
        assert_eq!(ids(&s.poll()), ["urgent"]);
    }

    #[test]
    fn start_now_jumps_ahead_of_other_high_priority_items() {
        let mut s = Scheduler::new(limits());
        s.enqueue("a", "a.example", Priority::High);
        s.enqueue("b", "b.example", Priority::High);
        s.enqueue("c", "c.example", Priority::Normal);
        assert!(s.start_now("c"));
        assert_eq!(s.queue_order(), ["c", "a", "b"]);
    }

    #[test]
    fn start_now_does_not_preempt_a_running_download() {
        let mut s = Scheduler::new(Limits {
            max_concurrent_downloads: 1,
            ..limits()
        });
        s.enqueue("running", "a.example", Priority::Normal);
        s.poll();
        s.enqueue("wanted", "b.example", Priority::Normal);
        assert!(s.start_now("wanted"));
        // Nothing starts until the running one is out of the way.
        assert!(s.poll().is_empty());
        assert!(s.is_running("running"));
        s.finished("running");
        assert_eq!(ids(&s.poll()), ["wanted"]);
    }

    #[test]
    fn two_downloads_from_one_host_split_that_host_budget() {
        let mut s = Scheduler::new(limits());
        s.enqueue("a", "same.example", Priority::Normal);
        s.enqueue("b", "same.example", Priority::Normal);
        let got = s.poll();
        assert_eq!(got[0].max_conns, 8, "first has the host to itself");
        assert_eq!(got[1].max_conns, 4, "second takes a half share");
    }

    #[test]
    fn a_host_at_its_cap_does_not_block_other_hosts_behind_it() {
        let mut s = Scheduler::new(Limits {
            max_concurrent_downloads: 4,
            max_conns_per_host: 4,
            ..limits()
        });
        s.enqueue("busy", "same.example", Priority::High);
        s.poll();
        s.update_conns("busy", 4); // host budget now fully spent

        s.enqueue("also-busy", "same.example", Priority::High);
        s.enqueue("elsewhere", "other.example", Priority::Normal);
        // Higher priority, but its host has nothing left; the lower-priority item on a free
        // host goes instead of the queue stalling.
        assert_eq!(ids(&s.poll()), ["elsewhere"]);
        assert_eq!(s.queue_order(), ["also-busy"]);
    }

    #[test]
    fn the_global_connection_budget_binds_across_hosts() {
        let mut s = Scheduler::new(Limits {
            max_concurrent_downloads: 8,
            max_conns_per_download: 8,
            max_conns_per_host: 8,
            max_total_conns: 10,
        });
        s.enqueue("a", "a.example", Priority::Normal);
        s.enqueue("b", "b.example", Priority::Normal);
        s.poll();
        s.update_conns("a", 6);
        s.update_conns("b", 4); // 10 of 10 spent

        s.enqueue("c", "c.example", Priority::Normal);
        assert!(
            s.poll().is_empty(),
            "no host is at its cap, but the link is"
        );
    }

    #[test]
    fn an_admission_never_exceeds_the_remaining_global_budget() {
        let mut s = Scheduler::new(Limits {
            max_concurrent_downloads: 4,
            max_conns_per_download: 8,
            max_conns_per_host: 8,
            max_total_conns: 9,
        });
        s.enqueue("a", "a.example", Priority::Normal);
        s.poll();
        s.update_conns("a", 7);
        s.enqueue("b", "b.example", Priority::Normal);
        let got = s.poll();
        assert_eq!(got[0].max_conns, 2, "only two connections left in the link");
    }

    #[test]
    fn lowering_a_limit_does_not_stop_running_work() {
        let mut s = Scheduler::new(limits());
        s.enqueue("a", "a.example", Priority::Normal);
        s.enqueue("b", "b.example", Priority::Normal);
        s.poll();
        assert_eq!(s.running_count(), 2);
        s.set_limits(Limits {
            max_concurrent_downloads: 1,
            ..limits()
        });
        assert_eq!(s.running_count(), 2, "already-moving transfers are kept");
        s.enqueue("c", "c.example", Priority::Normal);
        assert!(s.poll().is_empty(), "but the new limit binds admissions");
    }

    #[test]
    fn removing_a_queued_download_takes_it_out_of_the_order() {
        let mut s = Scheduler::new(Limits {
            max_concurrent_downloads: 1,
            ..limits()
        });
        s.enqueue("a", "a.example", Priority::Normal);
        s.enqueue("b", "b.example", Priority::Normal);
        s.enqueue("c", "c.example", Priority::Normal);
        s.remove("b");
        assert_eq!(s.queue_order(), ["a", "c"]);
        assert_eq!(s.queue_position("c"), Some(1));
    }

    #[test]
    fn enqueueing_the_same_id_twice_does_not_duplicate_it() {
        let mut s = Scheduler::new(limits());
        s.enqueue("a", "a.example", Priority::Normal);
        s.enqueue("a", "a.example", Priority::High);
        assert_eq!(s.queued_count(), 1);
        assert_eq!(s.queue_order(), ["a"]);
    }

    #[test]
    fn only_one_download_holds_the_probe_token() {
        let s = Scheduler::new(limits());
        let token = s.probe_token();
        assert!(!token.is_taken());
        let held = token.clone();
        // Two clones are the same token, not two tokens.
        let permit = held.try_take();
        assert!(permit.is_some());
        assert!(token.is_taken());
        drop(permit);
        assert!(!token.is_taken());
    }
}
