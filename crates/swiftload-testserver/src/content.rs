//! Deterministic, seekable synthetic content.
//!
//! The byte at any offset is a pure function of `(seed, offset)`, so the server can serve a
//! nominal 10 GB "file" without touching disk, and a client can verify **every byte** it
//! assembled — out of order, across segments, across resumes — by recomputing the expectation.
//!
//! Two different seeds produce content that differs almost everywhere, which is what makes
//! the "decoy" scenario (same name, same size, different bytes) a genuine test of content
//! verification rather than of header comparison.

/// SplitMix64 — small, fast, and good enough that a byte-level comparison is meaningful.
#[inline]
fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

/// Hash a human-readable seed name into the numeric seed used for generation.
pub fn seed_of(name: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in name.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h | 1
}

/// The single byte at `offset` for `seed`.
#[inline]
pub fn byte_at(seed: u64, offset: u64) -> u8 {
    let block = offset / 8;
    let v = splitmix64(seed ^ block.wrapping_mul(0x9E3779B97F4A7C15));
    v.to_le_bytes()[(offset % 8) as usize]
}

/// Fill `buf` with the content starting at `start`.
pub fn fill(seed: u64, start: u64, buf: &mut [u8]) {
    let mut off = start;
    let mut i = 0usize;

    // Unaligned head.
    while i < buf.len() && off % 8 != 0 {
        buf[i] = byte_at(seed, off);
        i += 1;
        off += 1;
    }
    // Aligned body: one PRNG call per 8 bytes.
    while i + 8 <= buf.len() {
        let block = off / 8;
        let v = splitmix64(seed ^ block.wrapping_mul(0x9E3779B97F4A7C15));
        buf[i..i + 8].copy_from_slice(&v.to_le_bytes());
        i += 8;
        off += 8;
    }
    // Unaligned tail.
    while i < buf.len() {
        buf[i] = byte_at(seed, off);
        i += 1;
        off += 1;
    }
}

/// Allocate and fill a chunk.
pub fn chunk(seed: u64, start: u64, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    fill(seed, start, &mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fill_matches_byte_at_across_alignments() {
        let seed = seed_of("alpha");
        for start in [0u64, 1, 7, 8, 9, 1023, 4096, 1_000_000_007] {
            let mut buf = vec![0u8; 37];
            fill(seed, start, &mut buf);
            for (i, &b) in buf.iter().enumerate() {
                assert_eq!(b, byte_at(seed, start + i as u64), "start={start} i={i}");
            }
        }
    }

    #[test]
    fn content_is_stable_across_calls() {
        let seed = seed_of("beta");
        assert_eq!(chunk(seed, 12_345, 64), chunk(seed, 12_345, 64));
    }

    #[test]
    fn out_of_order_assembly_reconstructs_the_same_stream() {
        // This is exactly what the engine does: independent segments, assembled by offset.
        let seed = seed_of("gamma");
        let whole = chunk(seed, 0, 4096);
        let mut assembled = vec![0u8; 4096];
        for &(start, len) in &[(2048usize, 1024usize), (0, 512), (3072, 1024), (512, 1536)] {
            assembled[start..start + len].copy_from_slice(&chunk(seed, start as u64, len));
        }
        assert_eq!(assembled, whole);
    }

    #[test]
    fn different_seeds_differ_almost_everywhere() {
        // The decoy scenario depends on this: same size, same name, different bytes.
        let a = chunk(seed_of("real"), 0, 4096);
        let b = chunk(seed_of("decoy"), 0, 4096);
        let same = a.iter().zip(&b).filter(|(x, y)| x == y).count();
        assert!(same < 100, "{same}/4096 bytes collided — seeds are too close");
    }

    #[test]
    fn content_is_not_trivially_compressible_or_constant() {
        let c = chunk(seed_of("delta"), 0, 8192);
        let distinct: std::collections::HashSet<_> = c.iter().collect();
        assert!(distinct.len() > 200, "only {} distinct byte values", distinct.len());
    }
}
