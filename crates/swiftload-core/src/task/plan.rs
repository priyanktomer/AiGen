//! Work allocation: claims, shrinking, and stealing.
//!
//! # Why not a fixed N-way split
//!
//! The classic approach cuts the file into N equal segments, one per connection. It is
//! straggler-bound: the slowest connection decides when the download finishes, and on a
//! heterogeneous path that is a large penalty paid on every download.
//!
//! # Why not a chunk queue
//!
//! Handing out fixed 4 MB chunks load-balances beautifully, but costs one request round-trip
//! per chunk. At 4 MB / 10 MB/s / 30 ms RTT that is ~7% overhead, and it gets worse on
//! high-latency paths — exactly the paths where parallelism is most valuable.
//!
//! # Claims
//!
//! Each worker owns a mutable `[cursor, end)` claim and streams it in **one** long-lived
//! request. When a worker runs out of work, the coordinator lowers another worker's `end` and
//! hands the freed tail over. The shrunk worker notices at its next chunk boundary and stops
//! cleanly. No request is wasted except on an actual steal, and the straggler penalty is gone
//! because a slow worker simply keeps less of its range.

use crate::{config::MIN_SEGMENT, util::intervals::RangeSet};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};

/// Steals are rounded to this boundary, so a split never lands mid-chunk.
const SPLIT_ALIGN: u64 = 1024 * 1024;

/// A worker's mutable range. `end` may be lowered by the coordinator at any time.
#[derive(Debug)]
pub struct ClaimSlot {
    cursor: AtomicU64,
    end: AtomicU64,
    active: AtomicBool,
}

impl ClaimSlot {
    fn new(start: u64, end: u64) -> Arc<Self> {
        Arc::new(Self {
            cursor: AtomicU64::new(start),
            end: AtomicU64::new(end),
            active: AtomicBool::new(true),
        })
    }

    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    pub fn end(&self) -> u64 {
        self.end.load(Ordering::Acquire)
    }

    /// Bytes still to fetch. Zero once the worker has passed a lowered `end`.
    pub fn remaining(&self) -> u64 {
        self.end().saturating_sub(self.cursor())
    }

    pub fn is_done(&self) -> bool {
        self.cursor() >= self.end()
    }

    /// Record progress. Called by the worker after each chunk is handed to the writer.
    pub fn advance(&self, n: u64) {
        self.cursor.fetch_add(n, Ordering::AcqRel);
    }

    fn deactivate(&self) {
        self.active.store(false, Ordering::Release);
    }

    fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

/// Outcome of asking for work.
#[derive(Debug)]
pub enum Grant {
    /// A range to fetch.
    Claim(Arc<ClaimSlot>),
    /// Nothing left to hand out; the worker should retire.
    Exhausted,
}

pub struct Plan {
    total: Option<u64>,
    inner: Mutex<Inner>,
}

struct Inner {
    /// Unclaimed gaps, as `(start, len)`.
    free: Vec<(u64, u64)>,
    /// Live claims, including finished ones not yet released.
    slots: Vec<Arc<ClaimSlot>>,
}

impl Plan {
    /// Build a plan over the **gaps** in `completed`, never over the whole file.
    ///
    /// This is what makes resume work identically for a fresh start, a pause, a crash, and a
    /// URL swap: in every case the planner is simply told what is already on disk.
    pub fn new(total: Option<u64>, completed: &RangeSet) -> Self {
        let free = match total {
            Some(t) => completed.gaps_in(0, t),
            // Unknown size: one open-ended claim from where we left off.
            None => vec![(completed.end(), u64::MAX - completed.end())],
        };
        Self { total, inner: Mutex::new(Inner { free, slots: Vec::new() }) }
    }

    /// Split the outstanding work into up to `n` initial claims.
    pub fn seed(&self, n: usize) {
        let mut g = self.inner.lock().unwrap();
        if n <= 1 || g.free.len() >= n {
            return;
        }
        // Split the largest gaps until we have enough pieces to occupy `n` workers, never
        // producing a piece smaller than a minimum segment.
        while g.free.len() < n {
            let Some((idx, &(start, len))) = g
                .free
                .iter()
                .enumerate()
                .max_by_key(|(_, &(_, len))| len)
            else {
                break;
            };
            if len < MIN_SEGMENT * 2 {
                break;
            }
            let half = align_up(len / 2);
            if half == 0 || half >= len {
                break;
            }
            g.free[idx] = (start, half);
            g.free.push((start + half, len - half));
            g.free.sort_unstable_by_key(|&(s, _)| s);
        }
    }

    pub fn total(&self) -> Option<u64> {
        self.total
    }

    /// Work not yet claimed by any worker.
    pub fn unclaimed(&self) -> u64 {
        self.inner.lock().unwrap().free.iter().map(|&(_, l)| l).sum()
    }

    /// Everything still outstanding: unclaimed plus what live workers hold.
    pub fn outstanding(&self) -> u64 {
        let g = self.inner.lock().unwrap();
        let free: u64 = g.free.iter().map(|&(_, l)| l).sum();
        let held: u64 = g.slots.iter().filter(|s| s.is_active()).map(|s| s.remaining()).sum();
        free + held
    }

    /// Hand out work: an unclaimed gap if one exists, otherwise steal half of the largest
    /// live claim.
    pub fn request(&self) -> Grant {
        let mut g = self.inner.lock().unwrap();

        // Prefer unclaimed work; take the largest so early claims are chunky.
        if let Some((idx, _)) = g.free.iter().enumerate().max_by_key(|(_, &(_, l))| l) {
            let (start, len) = g.free.remove(idx);
            let slot = ClaimSlot::new(start, start + len);
            g.slots.push(slot.clone());
            return Grant::Claim(slot);
        }

        // Nothing free: steal from whoever holds the most.
        let victim = g
            .slots
            .iter()
            .filter(|s| s.is_active())
            .max_by_key(|s| s.remaining())
            .cloned();

        let Some(victim) = victim else {
            return Grant::Exhausted;
        };

        let remaining = victim.remaining();
        // Not worth splitting: the victim would be left with a scrap, and we would pay a
        // fresh request to save almost nothing.
        if remaining < MIN_SEGMENT * 2 {
            return Grant::Exhausted;
        }

        let cursor = victim.cursor();
        let split_at = align_up(cursor + remaining / 2);
        let old_end = victim.end();
        if split_at <= cursor || split_at >= old_end {
            return Grant::Exhausted;
        }

        // Lower the victim's end. It notices at its next chunk boundary and stops cleanly —
        // no connection is torn down and no bytes are wasted.
        victim.end.store(split_at, Ordering::Release);

        let slot = ClaimSlot::new(split_at, old_end);
        g.slots.push(slot.clone());
        Grant::Claim(slot)
    }

    /// Give back a claim. Any unfetched tail returns to the free list.
    pub fn release(&self, slot: &Arc<ClaimSlot>) {
        slot.deactivate();
        let (cursor, end) = (slot.cursor(), slot.end());
        if cursor < end {
            let mut g = self.inner.lock().unwrap();
            g.free.push((cursor, end - cursor));
            g.free.sort_unstable_by_key(|&(s, _)| s);
            merge_free(&mut g.free);
        }
        self.inner.lock().unwrap().slots.retain(|s| !Arc::ptr_eq(s, slot) || s.is_active());
    }

    /// Return a range to the pool after a failure, so another worker can retry it.
    pub fn return_range(&self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let mut g = self.inner.lock().unwrap();
        g.free.push((start, len));
        g.free.sort_unstable_by_key(|&(s, _)| s);
        merge_free(&mut g.free);
    }

    /// Largest number of workers that could usefully be running right now.
    pub fn useful_workers(&self) -> usize {
        let out = self.outstanding();
        ((out / MIN_SEGMENT) as usize).max(1)
    }
}

fn align_up(v: u64) -> u64 {
    v.div_ceil(SPLIT_ALIGN) * SPLIT_ALIGN
}

fn merge_free(free: &mut Vec<(u64, u64)>) {
    let mut out: Vec<(u64, u64)> = Vec::with_capacity(free.len());
    for &(s, l) in free.iter() {
        match out.last_mut() {
            Some((ps, pl)) if *ps + *pl >= s => {
                let end = (*ps + *pl).max(s + l);
                *pl = end - *ps;
            }
            _ => out.push((s, l)),
        }
    }
    *free = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn take(plan: &Plan) -> Arc<ClaimSlot> {
        match plan.request() {
            Grant::Claim(c) => c,
            Grant::Exhausted => panic!("expected work to be available"),
        }
    }

    #[test]
    fn a_fresh_download_plans_over_the_whole_file() {
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        assert_eq!(p.unclaimed(), 100 * MB);
        let c = take(&p);
        assert_eq!((c.cursor(), c.end()), (0, 100 * MB));
    }

    #[test]
    fn a_resumed_download_plans_only_over_the_gaps() {
        // The property that makes resume work: already-downloaded bytes are never re-fetched.
        let done = RangeSet::from_pairs([(0, 30 * MB), (60 * MB, 40 * MB)]);
        let p = Plan::new(Some(100 * MB), &done);
        assert_eq!(p.unclaimed(), 30 * MB, "only the hole from 30 MB to 60 MB is outstanding");

        let c = take(&p);
        assert_eq!((c.cursor(), c.end()), (30 * MB, 60 * MB));
    }

    #[test]
    fn seeding_splits_work_across_workers() {
        let p = Plan::new(Some(64 * MB), &RangeSet::new());
        p.seed(4);
        let claims: Vec<_> = (0..4).map(|_| take(&p)).collect();

        assert_eq!(claims.iter().map(|c| c.remaining()).sum::<u64>(), 64 * MB);
        // Contiguous and non-overlapping when sorted.
        let mut sorted: Vec<_> = claims.iter().map(|c| (c.cursor(), c.end())).collect();
        sorted.sort();
        for w in sorted.windows(2) {
            assert_eq!(w[0].1, w[1].0, "claims must tile the file: {sorted:?}");
        }
        assert_eq!(sorted.first().unwrap().0, 0);
        assert_eq!(sorted.last().unwrap().1, 64 * MB);
    }

    #[test]
    fn seeding_refuses_to_produce_useless_slivers() {
        // 3 MB cannot usefully occupy 8 workers at a 2 MB minimum segment.
        let p = Plan::new(Some(3 * MB), &RangeSet::new());
        p.seed(8);
        assert!(p.unclaimed() == 3 * MB);
        let first = take(&p);
        assert!(first.remaining() >= MIN_SEGMENT, "produced a sliver of {}", first.remaining());
    }

    #[test]
    fn an_idle_worker_steals_half_of_the_largest_claim() {
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        let a = take(&p);
        assert_eq!(a.remaining(), 100 * MB);

        // Second worker arrives with nothing free: it must steal.
        let b = take(&p);

        assert!(a.end() < 100 * MB, "victim's end should have been lowered");
        assert_eq!(a.end(), b.cursor(), "the split must be seamless");
        assert_eq!(b.end(), 100 * MB);
        assert_eq!(a.remaining() + b.remaining(), 100 * MB, "no bytes lost or duplicated");
    }

    #[test]
    fn stealing_accounts_for_progress_already_made() {
        // A worker that has fetched most of its range should give away less.
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        let a = take(&p);
        a.advance(80 * MB);

        let b = take(&p);
        assert!(b.cursor() >= 80 * MB, "stole work that was already fetched: {}", b.cursor());
        assert_eq!(a.end(), b.cursor());
        assert_eq!(a.remaining() + b.remaining(), 20 * MB);
    }

    #[test]
    fn a_shrunk_worker_stops_at_its_new_end() {
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        let a = take(&p);
        let _b = take(&p); // steals, lowering a's end
        let new_end = a.end();

        // The worker keeps streaming and simply notices at the next chunk boundary.
        a.advance(new_end);
        assert!(a.is_done(), "worker must stop once it passes its lowered end");
        assert_eq!(a.remaining(), 0);
    }

    #[test]
    fn no_steal_when_the_remainder_is_too_small_to_split() {
        // Splitting 3 MB in two leaves both workers with less than a minimum segment, and
        // costs a fresh request to save nothing.
        let p = Plan::new(Some(3 * MB), &RangeSet::new());
        let _a = take(&p);
        assert!(matches!(p.request(), Grant::Exhausted));
    }

    #[test]
    fn releasing_returns_unfetched_work_to_the_pool() {
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        let a = take(&p);
        a.advance(10 * MB);
        assert_eq!(p.unclaimed(), 0);

        p.release(&a);
        assert_eq!(p.unclaimed(), 90 * MB, "the unfetched tail must come back");

        let b = take(&p);
        assert_eq!(b.cursor(), 10 * MB, "and be handed to the next worker");
    }

    #[test]
    fn releasing_a_finished_claim_adds_nothing() {
        let p = Plan::new(Some(10 * MB), &RangeSet::new());
        let a = take(&p);
        a.advance(10 * MB);
        p.release(&a);
        assert_eq!(p.unclaimed(), 0);
        assert!(matches!(p.request(), Grant::Exhausted));
    }

    #[test]
    fn returned_ranges_are_merged() {
        // A failed segment goes back for another worker to retry.
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        let a = take(&p);
        p.release(&a);

        p.return_range(0, 10 * MB);
        p.return_range(10 * MB, 10 * MB);
        assert_eq!(p.unclaimed(), 100 * MB, "overlapping returns must not double-count");
    }

    #[test]
    fn outstanding_counts_both_free_and_held_work() {
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        assert_eq!(p.outstanding(), 100 * MB);

        let a = take(&p);
        assert_eq!(p.unclaimed(), 0);
        assert_eq!(p.outstanding(), 100 * MB, "held work still counts");

        a.advance(40 * MB);
        assert_eq!(p.outstanding(), 60 * MB);
    }

    #[test]
    fn useful_worker_count_tracks_remaining_work() {
        let p = Plan::new(Some(100 * MB), &RangeSet::new());
        assert!(p.useful_workers() >= 32);

        let a = take(&p);
        a.advance(99 * MB);
        assert_eq!(p.useful_workers(), 1, "1 MB left cannot occupy several connections");
    }

    #[test]
    fn unknown_size_yields_one_open_ended_claim() {
        let p = Plan::new(None, &RangeSet::new());
        let c = take(&p);
        assert_eq!(c.cursor(), 0);
        assert!(c.remaining() > 0);
        // Without a size there is nothing to split, so no second worker is spawned.
        assert_eq!(p.total(), None);
    }

    #[test]
    fn repeated_steals_tile_the_file_without_gaps_or_overlap() {
        // The invariant that matters most: however work is subdivided, every byte is claimed
        // exactly once.
        let p = Plan::new(Some(256 * MB), &RangeSet::new());
        let mut claims = Vec::new();
        for _ in 0..16 {
            match p.request() {
                Grant::Claim(c) => claims.push(c),
                Grant::Exhausted => break,
            }
        }
        assert!(claims.len() >= 8, "only produced {} claims", claims.len());

        let mut ranges: Vec<(u64, u64)> = claims.iter().map(|c| (c.cursor(), c.end())).collect();
        ranges.sort();
        assert_eq!(ranges[0].0, 0);
        assert_eq!(ranges.last().unwrap().1, 256 * MB);
        for w in ranges.windows(2) {
            assert_eq!(w[0].1, w[1].0, "gap or overlap between claims: {ranges:?}");
        }
        assert_eq!(
            ranges.iter().map(|(s, e)| e - s).sum::<u64>(),
            256 * MB,
            "claims must sum to the file size"
        );
    }
}
