//! `RangeSet`: the authoritative record of which byte ranges are durably on disk.
//!
//! This is the single most correctness-critical structure in the engine. A download's
//! resume state is exactly one of these, persisted as a compact BLOB in one row, which
//! makes a checkpoint atomic by construction (see `store`).
//!
//! Invariants, upheld by every operation and asserted by the property tests:
//!   * spans are sorted by start
//!   * spans are disjoint
//!   * spans are non-adjacent (touching spans are merged, so the representation is canonical)
//!   * no span has zero length

use std::fmt;

/// A canonical set of byte ranges, stored as sorted `(start, len)` pairs.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct RangeSet {
    spans: Vec<(u64, u64)>,
}

impl RangeSet {
    pub fn new() -> Self {
        Self { spans: Vec::new() }
    }

    /// Build from `(start, len)` pairs in any order. Overlapping and adjacent input is merged.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (u64, u64)>) -> Self {
        let mut s = Self::new();
        for (start, len) in pairs {
            s.insert(start, len);
        }
        s
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    pub fn spans(&self) -> &[(u64, u64)] {
        &self.spans
    }

    /// Total number of bytes covered.
    pub fn total(&self) -> u64 {
        self.spans.iter().map(|&(_, len)| len).sum()
    }

    /// One past the highest covered byte, or 0 when empty.
    pub fn end(&self) -> u64 {
        self.spans.last().map_or(0, |&(s, l)| s + l)
    }

    /// Insert `[start, start+len)`, merging with any overlapping or adjacent spans.
    ///
    /// Re-inserting bytes already present is a no-op on the set — workers legitimately
    /// re-fetch after a crash rewind, so this must be idempotent rather than an error.
    pub fn insert(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let end = start.saturating_add(len);

        // First span that could touch or follow us: the first whose own end >= our start.
        // (`>=` rather than `>` so an exactly-adjacent span merges.)
        let lo = self.spans.partition_point(|&(s, l)| s + l < start);
        // First span strictly after our end (cannot touch): its start > our end.
        let hi = self.spans.partition_point(|&(s, _)| s <= end);

        if lo >= hi {
            // Nothing to merge with; plain insertion keeps the vec sorted.
            self.spans.insert(lo, (start, len));
            return;
        }

        let merged_start = start.min(self.spans[lo].0);
        let merged_end = {
            let (ls, ll) = self.spans[hi - 1];
            end.max(ls + ll)
        };
        self.spans
            .splice(lo..hi, [(merged_start, merged_end - merged_start)]);
    }

    /// True when every byte in `[start, end)` is present. An empty range is trivially contained.
    pub fn contains_all(&self, start: u64, end: u64) -> bool {
        if start >= end {
            return true;
        }
        // A single span must cover the whole query, because the set is canonical:
        // adjacent spans are always merged, so a gap-free region is exactly one span.
        self.spans
            .iter()
            .any(|&(s, l)| s <= start && start + (end - start) <= s + l)
    }

    /// The first missing byte at or after `pos`, with the gap's exclusive end.
    ///
    /// `None` for the end means "open-ended": no covered span begins after the gap, so the
    /// gap runs to the end of the file (whose size this structure deliberately does not know).
    pub fn first_gap_at_or_after(&self, pos: u64) -> Option<(u64, Option<u64>)> {
        let mut cursor = pos;
        for &(s, l) in &self.spans {
            if s + l <= cursor {
                continue; // span is entirely behind the cursor
            }
            if s <= cursor {
                cursor = s + l; // cursor is inside this span; step past it
                continue;
            }
            return Some((cursor, Some(s))); // gap runs from cursor up to this span's start
        }
        Some((cursor, None))
    }

    /// All gaps within `[start, end)`, in ascending order.
    ///
    /// This is what the planner uses on resume: work is planned over the holes, never over
    /// the whole file.
    pub fn gaps_in(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        let mut out = Vec::new();
        if start >= end {
            return out;
        }
        let mut cursor = start;
        for &(s, l) in &self.spans {
            let (ss, se) = (s, s + l);
            if se <= cursor {
                continue;
            }
            if ss >= end {
                break;
            }
            if ss > cursor {
                out.push((cursor, ss.min(end) - cursor));
            }
            cursor = cursor.max(se);
            if cursor >= end {
                return out;
            }
        }
        if cursor < end {
            out.push((cursor, end - cursor));
        }
        out
    }

    /// Remove `[start, start+len)` from the set, splitting spans as needed.
    pub fn remove(&mut self, start: u64, len: u64) {
        if len == 0 {
            return;
        }
        let end = start + len;
        let mut out = Vec::with_capacity(self.spans.len() + 1);
        for &(s, l) in &self.spans {
            let (ss, se) = (s, s + l);
            if se <= start || ss >= end {
                out.push((s, l)); // untouched
                continue;
            }
            if ss < start {
                out.push((ss, start - ss)); // keep the head
            }
            if se > end {
                out.push((end, se - end)); // keep the tail
            }
        }
        self.spans = out;
    }

    /// Drop everything at or beyond `limit`. Used when the on-disk file turns out to be
    /// shorter than the checkpoint claimed (truncated `.slpart`).
    pub fn truncate_to(&mut self, limit: u64) {
        self.spans.retain_mut(|(s, l)| {
            if *s >= limit {
                return false;
            }
            if *s + *l > limit {
                *l = limit - *s;
            }
            true
        });
    }

    /// Paranoid crash recovery: give back the last `margin` bytes of every span.
    ///
    /// `fsync` is honoured by essentially all modern drives, but consumer SSDs with volatile
    /// write caches and some virtualised storage have been observed to lie. Re-fetching at
    /// most `margin` bytes per fragment is trivial insurance against a corruption class we
    /// could otherwise never detect. Spans shorter than the margin are dropped entirely.
    pub fn rewind_tails(&mut self, margin: u64) {
        if margin == 0 {
            return;
        }
        self.spans.retain_mut(|(_, l)| {
            if *l <= margin {
                return false;
            }
            *l -= margin;
            true
        });
    }

    /// Compact varint encoding of `(gap_from_previous_end, len)` deltas.
    ///
    /// Even a heavily fragmented multi-GB download encodes to a few dozen bytes, which is
    /// what makes a single-row checkpoint cheap enough to write every few seconds.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.spans.len() * 4);
        let mut prev_end = 0u64;
        for &(s, l) in &self.spans {
            put_varint(&mut out, s - prev_end);
            put_varint(&mut out, l);
            prev_end = s + l;
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Self, DecodeError> {
        let mut spans = Vec::new();
        let mut i = 0usize;
        let mut prev_end = 0u64;
        while i < bytes.len() {
            let gap = get_varint(bytes, &mut i)?;
            let len = get_varint(bytes, &mut i)?;
            if len == 0 {
                return Err(DecodeError::ZeroLength);
            }
            let start = prev_end.checked_add(gap).ok_or(DecodeError::Overflow)?;
            let end = start.checked_add(len).ok_or(DecodeError::Overflow)?;
            spans.push((start, len));
            prev_end = end;
        }
        Ok(Self { spans })
    }
}

impl fmt::Debug for RangeSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RangeSet[")?;
        for (i, &(s, l)) in self.spans.iter().enumerate() {
            if i > 0 {
                write!(f, ", ")?;
            }
            write!(f, "{}..{}", s, s + l)?;
        }
        write!(f, "]")
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DecodeError {
    #[error("truncated varint in range set")]
    Truncated,
    #[error("varint overflow in range set")]
    Overflow,
    #[error("zero-length span in range set")]
    ZeroLength,
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn get_varint(bytes: &[u8], i: &mut usize) -> Result<u64, DecodeError> {
    let mut v = 0u64;
    let mut shift = 0u32;
    loop {
        let b = *bytes.get(*i).ok_or(DecodeError::Truncated)?;
        *i += 1;
        if shift >= 64 {
            return Err(DecodeError::Overflow);
        }
        v |= u64::from(b & 0x7f)
            .checked_shl(shift)
            .ok_or(DecodeError::Overflow)?;
        if b & 0x80 == 0 {
            return Ok(v);
        }
        shift += 7;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert the canonical-form invariants. Every test calls this.
    fn check_invariants(rs: &RangeSet) {
        let mut prev_end = None::<u64>;
        for &(s, l) in rs.spans() {
            assert!(l > 0, "zero-length span in {rs:?}");
            if let Some(pe) = prev_end {
                assert!(
                    s > pe,
                    "spans must be sorted, disjoint AND non-adjacent: {rs:?}"
                );
            }
            prev_end = Some(s + l);
        }
    }

    #[test]
    fn insert_merges_adjacent_and_overlapping() {
        let mut rs = RangeSet::new();
        rs.insert(0, 100);
        rs.insert(100, 100); // exactly adjacent -> must merge
        check_invariants(&rs);
        assert_eq!(rs.spans(), &[(0, 200)]);

        rs.insert(50, 100); // fully contained -> no change
        check_invariants(&rs);
        assert_eq!(rs.spans(), &[(0, 200)]);

        rs.insert(300, 50); // disjoint
        rs.insert(200, 100); // bridges the two -> all merge
        check_invariants(&rs);
        assert_eq!(rs.spans(), &[(0, 350)]);
    }

    #[test]
    fn insert_is_order_independent() {
        let pairs = [
            (500u64, 100u64),
            (0, 100),
            (200, 100),
            (100, 100),
            (300, 100),
        ];
        let a = RangeSet::from_pairs(pairs);
        let mut reversed: Vec<_> = pairs.to_vec();
        reversed.reverse();
        let b = RangeSet::from_pairs(reversed);
        check_invariants(&a);
        assert_eq!(a, b);
        assert_eq!(a.spans(), &[(0, 400), (500, 100)]);
    }

    #[test]
    fn insert_is_idempotent() {
        // Workers legitimately re-fetch bytes after a crash rewind.
        let mut rs = RangeSet::from_pairs([(0, 100), (200, 100)]);
        let before = rs.clone();
        rs.insert(0, 100);
        rs.insert(250, 20);
        assert_eq!(rs, before);
        assert_eq!(rs.total(), 200);
    }

    #[test]
    fn total_and_end() {
        let rs = RangeSet::from_pairs([(0, 10), (100, 5)]);
        assert_eq!(rs.total(), 15);
        assert_eq!(rs.end(), 105);
        assert_eq!(RangeSet::new().end(), 0);
    }

    #[test]
    fn contains_all_respects_gaps() {
        let rs = RangeSet::from_pairs([(0, 100), (200, 100)]);
        assert!(rs.contains_all(0, 100));
        assert!(rs.contains_all(10, 90));
        assert!(rs.contains_all(5, 5)); // empty query
        assert!(!rs.contains_all(0, 101)); // crosses the gap
        assert!(!rs.contains_all(150, 160)); // wholly in the gap
        assert!(!rs.contains_all(100, 200));
    }

    #[test]
    fn gaps_in_finds_holes() {
        let rs = RangeSet::from_pairs([(100, 100), (400, 100)]);
        assert_eq!(rs.gaps_in(0, 600), vec![(0, 100), (200, 200), (500, 100)]);
        assert_eq!(rs.gaps_in(0, 100), vec![(0, 100)]);
        assert_eq!(rs.gaps_in(100, 200), vec![]); // fully covered
        assert_eq!(rs.gaps_in(150, 450), vec![(200, 200)]);
        assert_eq!(rs.gaps_in(500, 500), vec![]); // empty window
    }

    #[test]
    fn gaps_over_empty_set_is_the_whole_window() {
        let rs = RangeSet::new();
        assert_eq!(rs.gaps_in(0, 1000), vec![(0, 1000)]);
    }

    #[test]
    fn first_gap_reports_open_ended_tail() {
        let rs = RangeSet::from_pairs([(0, 100), (200, 100)]);
        assert_eq!(rs.first_gap_at_or_after(0), Some((100, Some(200))));
        assert_eq!(rs.first_gap_at_or_after(150), Some((150, Some(200))));
        // Past the last span the gap is open-ended: the set does not know the file size.
        assert_eq!(rs.first_gap_at_or_after(300), Some((300, None)));
    }

    #[test]
    fn remove_splits_spans() {
        let mut rs = RangeSet::from_pairs([(0, 1000)]);
        rs.remove(400, 200);
        check_invariants(&rs);
        assert_eq!(rs.spans(), &[(0, 400), (600, 400)]);

        rs.remove(0, 10_000); // remove everything
        assert!(rs.is_empty());
    }

    #[test]
    fn truncate_drops_beyond_real_file_length() {
        let mut rs = RangeSet::from_pairs([(0, 100), (200, 100)]);
        rs.truncate_to(250);
        check_invariants(&rs);
        assert_eq!(rs.spans(), &[(0, 100), (200, 50)]);

        rs.truncate_to(50);
        assert_eq!(rs.spans(), &[(0, 50)]);
    }

    #[test]
    fn rewind_tails_gives_back_the_margin() {
        let mut rs = RangeSet::from_pairs([(0, 100), (200, 100)]);
        rs.rewind_tails(10);
        check_invariants(&rs);
        assert_eq!(rs.spans(), &[(0, 90), (200, 90)]);
        assert_eq!(rs.total(), 180);
    }

    #[test]
    fn rewind_tails_drops_spans_shorter_than_margin() {
        let mut rs = RangeSet::from_pairs([(0, 5), (200, 100)]);
        rs.rewind_tails(10);
        check_invariants(&rs);
        assert_eq!(rs.spans(), &[(200, 90)]);
    }

    #[test]
    fn rewind_never_grows_the_set() {
        let mut rs = RangeSet::from_pairs([(0, 100), (500, 100)]);
        let before = rs.total();
        rs.rewind_tails(1024 * 1024);
        assert!(rs.total() <= before);
        assert!(rs.is_empty());
    }

    #[test]
    fn encode_decode_roundtrip() {
        for rs in [
            RangeSet::new(),
            RangeSet::from_pairs([(0, 1)]),
            RangeSet::from_pairs([(0, 100), (200, 100), (10_000_000_000, 5_000_000_000)]),
        ] {
            let decoded = RangeSet::decode(&rs.encode()).expect("roundtrip");
            assert_eq!(rs, decoded, "roundtrip failed for {rs:?}");
            check_invariants(&decoded);
        }
    }

    #[test]
    fn encoding_of_a_fragmented_multi_gb_set_stays_tiny() {
        // 200 fragments scattered across 10 GB must still fit in a checkpoint we are happy
        // to write every few seconds.
        let rs = RangeSet::from_pairs((0..200).map(|i| (i * 50_000_000u64, 1_000_000u64)));
        assert_eq!(rs.spans().len(), 200);
        // ~7 bytes per fragment: a 4-byte varint gap plus a 3-byte varint length.
        assert!(
            rs.encode().len() < 2000,
            "encoded {} bytes",
            rs.encode().len()
        );
    }

    #[test]
    fn decode_rejects_corrupt_input() {
        assert_eq!(RangeSet::decode(&[0x80]), Err(DecodeError::Truncated));
        assert_eq!(
            RangeSet::decode(&[0x00, 0x00]),
            Err(DecodeError::ZeroLength)
        );
    }

    proptest::proptest! {
        #[test]
        fn prop_invariants_hold_under_arbitrary_inserts(
            raw in proptest::collection::vec((0u64..2000, 1u64..300), 0..60)
        ) {
            let rs = RangeSet::from_pairs(raw.iter().copied());

            let mut prev_end = None::<u64>;
            for &(s, l) in rs.spans() {
                proptest::prop_assert!(l > 0);
                if let Some(pe) = prev_end {
                    proptest::prop_assert!(s > pe, "not canonical: {:?}", rs);
                }
                prev_end = Some(s + l);
            }

            // The set must agree with a naive bitmap oracle.
            let mut oracle = vec![false; 2400];
            for &(s, l) in &raw {
                for b in s..(s + l) {
                    oracle[b as usize] = true;
                }
            }
            let expected: u64 = oracle.iter().filter(|&&b| b).count() as u64;
            proptest::prop_assert_eq!(rs.total(), expected);

            for (b, &covered) in oracle.iter().enumerate() {
                proptest::prop_assert_eq!(
                    rs.contains_all(b as u64, b as u64 + 1), covered, "byte {}", b
                );
            }
        }

        #[test]
        fn prop_order_independence(
            raw in proptest::collection::vec((0u64..2000, 1u64..300), 0..40)
        ) {
            let forward = RangeSet::from_pairs(raw.iter().copied());
            let backward = RangeSet::from_pairs(raw.iter().rev().copied());
            proptest::prop_assert_eq!(forward, backward);
        }

        #[test]
        fn prop_encode_decode_roundtrip(
            raw in proptest::collection::vec((0u64..100_000, 1u64..5000), 0..40)
        ) {
            let rs = RangeSet::from_pairs(raw);
            proptest::prop_assert_eq!(RangeSet::decode(&rs.encode()).unwrap(), rs);
        }

        #[test]
        fn prop_gaps_are_exactly_the_complement(
            raw in proptest::collection::vec((0u64..1000, 1u64..200), 0..30)
        ) {
            let rs = RangeSet::from_pairs(raw);
            let total = 1200u64;
            let gaps = rs.gaps_in(0, total);

            // Covered + gaps must tile [0, total) exactly, with no overlap.
            let mut seen = vec![0u8; total as usize];
            for &(s, l) in rs.spans() {
                for b in s..(s + l).min(total) { seen[b as usize] += 1; }
            }
            for &(s, l) in &gaps {
                for b in s..(s + l) { seen[b as usize] += 1; }
            }
            proptest::prop_assert!(seen.iter().all(|&c| c == 1), "gaps do not tile the range");
        }
    }
}
