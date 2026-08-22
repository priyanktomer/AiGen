//! Resource identity: deciding whether a replacement URL serves the same bytes.
//!
//! The scenario this exists for: a 10 GB download reaches 6.5 GB, the signed URL expires, and
//! the user obtains a fresh link. Those 6.5 GB must survive — but only if the new link really
//! is the same file. The governing priority is **never silently corrupt a large download**,
//! and an occasional confirmation prompt is an acceptable price for that.
//!
//! # Which signals are actually worth anything
//!
//! | Signal                  | Weight | Reality |
//! |-------------------------|--------|---------|
//! | byte-range comparison   | proof  | the only signal that proves the bytes line up |
//! | server digest           | proof  | rare, but conclusive when present |
//! | `Content-Length`        | gate   | equality proves nothing; **inequality is disproof** |
//! | strong `ETag` equal     | strong | but see below — inequality is *not* disproof |
//! | `Last-Modified` equal   | medium | cheap, often stable across re-signings |
//! | filename, content-type  | weak   | trivially coincidental or forged |
//!
//! # The ETag trap
//!
//! An ETag legitimately changes while the content is identical: the new link may resolve to a
//! different CDN edge (ETags are per-node on many CDNs), S3 multipart ETags depend on part
//! size rather than content, and some servers derive them from inode or mtime. So the rule is
//! asymmetric — **equal is strong evidence, different is no evidence at all** and merely
//! escalates to a content check. Treating ETag mismatch as disproof would reject the majority
//! of legitimate re-signings.

use crate::{
    http::headers::Validator,
    task::probe::{ProbeResult, RangeSupport},
    util::intervals::RangeSet,
};
use sha2::{Digest, Sha256};

/// Bytes compared per verification window.
pub const WINDOW: u64 = 64 * 1024;
/// Windows sampled for a Tier-1 check. Four 64 KB windows is 256 KB — about 0.0025% of a
/// 10 GB file, and under the 1 MB budget the feature promises.
pub const WINDOW_COUNT: usize = 4;

/// The header-level facts about a resource, from either side of a URL swap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ResourceSignals {
    pub total_size: Option<u64>,
    pub etag: Option<Validator>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub range_support: RangeSupport,
}

impl ResourceSignals {
    pub fn from_probe(p: &ProbeResult) -> Self {
        Self {
            total_size: p.total_size,
            etag: p.etag.clone(),
            last_modified: p.last_modified.clone(),
            content_type: p.content_type.clone(),
            filename: Some(p.filename.clone()),
            range_support: p.range_support,
        }
    }
}

/// What the header comparison concluded, before any bytes are fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Preliminary {
    /// Headers agree strongly enough to resume with no prompt and no extra traffic.
    Accept,
    /// Plausible, but needs a content check; on success resume without prompting.
    VerifyThenAccept,
    /// Plausible, but the evidence is weak enough that the user should confirm after the
    /// content check passes.
    VerifyThenConfirm,
    /// Conclusively a different resource. No prompt, no override.
    Reject(RejectReason),
    /// Same file, but the new link cannot resume. Honest about the cost.
    RestartOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RejectReason {
    /// Both sizes known and different — the one genuinely conclusive header.
    SizeMismatch { old: u64, new: u64 },
    /// A verification window did not match what is on disk.
    ContentMismatch { offset: u64 },
    /// The replacement is not an http(s) URL.
    BadScheme,
    /// The replacement needs credentials we do not have.
    AuthRequired,
}

impl RejectReason {
    /// A message for the user that never mentions ETags, ranges, or byte offsets.
    pub fn user_message(&self) -> String {
        match self {
            Self::SizeMismatch { old, new } => format!(
                "This link is a different file — it is {}, but your download is {}.",
                human(*new),
                human(*old)
            ),
            Self::ContentMismatch { .. } =>
                "The data already on disk does not match this link. Either this is a different \
                 file, or the existing partial download is damaged."
                    .to_string(),
            Self::BadScheme => "That is not a valid download link.".to_string(),
            Self::AuthRequired => "This link needs a sign-in that SwiftLoad cannot provide.".to_string(),
        }
    }
}

/// Header-level evidence, shown to the user when confirmation is needed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evidence {
    pub size_matches: bool,
    pub etag_matches: bool,
    pub etag_changed: bool,
    pub last_modified_matches: bool,
    pub filename_matches: bool,
    pub content_type_matches: bool,
    pub windows_verified: usize,
}

impl Evidence {
    /// Plain-language summary for the confirmation card.
    pub fn user_summary(&self) -> String {
        if self.windows_verified > 0 {
            format!(
                "This looks like the same file. The server's version tag changed, so we compared \
                 {} sample sections of what you have already downloaded — all matched.",
                self.windows_verified
            )
        } else if self.etag_matches {
            "Same file confirmed.".to_string()
        } else {
            "This appears to be the same file.".to_string()
        }
    }
}

/// Compare header signals. Fetches nothing.
pub fn compare(old: &ResourceSignals, new: &ResourceSignals) -> (Preliminary, Evidence) {
    let mut ev = Evidence::default();

    // ── The one conclusive gate. Equality proves nothing (a decoy is trivially
    //    size-matched) but inequality is near-proof of a different resource.
    if let (Some(a), Some(b)) = (old.total_size, new.total_size) {
        ev.size_matches = a == b;
        if a != b {
            return (Preliminary::Reject(RejectReason::SizeMismatch { old: a, new: b }), ev);
        }
    }

    ev.filename_matches = match (&old.filename, &new.filename) {
        (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
        _ => false,
    };
    ev.content_type_matches = old.content_type == new.content_type;
    ev.last_modified_matches = match (&old.last_modified, &new.last_modified) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    };

    let (etag_match, etag_differs, both_strong) = match (&old.etag, &new.etag) {
        (Some(a), Some(b)) => (a.raw() == b.raw(), a.raw() != b.raw(), a.is_strong() && b.is_strong()),
        _ => (false, false, false),
    };
    ev.etag_matches = etag_match;
    ev.etag_changed = etag_differs;

    // The replacement cannot serve ranges: resuming is impossible, whatever the headers say.
    if new.range_support != RangeSupport::Supported {
        return (Preliminary::RestartOnly, ev);
    }

    let outcome = if etag_match && both_strong && ev.size_matches {
        // Strong validator plus matching size: as good as headers get.
        Preliminary::Accept
    } else if ev.size_matches && ev.last_modified_matches && old.etag.is_none() && new.etag.is_none() {
        // No ETags anywhere, but the modification time agrees. Cheap to confirm by content.
        Preliminary::VerifyThenAccept
    } else if etag_match && ev.size_matches {
        // Weak validator match: asserts equivalence, not byte identity.
        Preliminary::VerifyThenAccept
    } else if etag_differs {
        // The common, legitimate case. Not disproof — escalate to content, then ask.
        Preliminary::VerifyThenConfirm
    } else {
        // One side has validators and the other does not, or neither has any.
        Preliminary::VerifyThenConfirm
    };

    (outcome, ev)
}

/// Choose the byte windows to compare, all inside already-downloaded data.
///
/// Placement is **deterministic per download** — a retried validation checks the same windows,
/// so results are reproducible — but **unpredictable across downloads**, so a hostile server
/// cannot pre-position matching decoy bytes at guessable offsets.
///
/// The first and last windows are always included: the first catches a wholly different file
/// immediately, and the last sits where a size-preserving difference or an off-by-one in range
/// accounting is most likely to show.
pub fn choose_windows(completed: &RangeSet, download_id: &str, count: usize) -> Vec<(u64, u64)> {
    let total = completed.total();
    if total == 0 {
        return Vec::new();
    }
    let win = WINDOW.min(total);
    let mut picks: Vec<u64> = Vec::new();

    // Logical offset -> absolute offset within the completed set.
    let locate = |mut logical: u64| -> u64 {
        for &(s, l) in completed.spans() {
            if logical < l {
                return s + logical;
            }
            logical -= l;
        }
        completed.spans().last().map_or(0, |&(s, l)| s + l - 1)
    };

    picks.push(locate(0));
    if total > win {
        picks.push(locate(total - win));
    }

    let mut seed = Sha256::digest(download_id.as_bytes());
    while picks.len() < count && total > win {
        let mut v = 0u64;
        for b in seed.iter().take(8) {
            v = (v << 8) | u64::from(*b);
        }
        picks.push(locate(v % (total - win)));
        seed = Sha256::digest(seed);
    }

    picks.sort_unstable();
    picks.dedup();
    picks.into_iter().map(|p| (p, win)).collect()
}

/// Compare two byte buffers, returning the absolute offset of the first difference.
pub fn first_difference(offset: u64, local: &[u8], remote: &[u8]) -> Option<u64> {
    if local.len() != remote.len() {
        return Some(offset + local.len().min(remote.len()) as u64);
    }
    local
        .iter()
        .zip(remote)
        .position(|(a, b)| a != b)
        .map(|i| offset + i as u64)
}

/// The final decision, after any content verification has run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Swap the URL and resume with no prompt.
    Resume(Evidence),
    /// Content matched, but ask before resuming.
    Confirm(Evidence),
    /// Refuse. There is deliberately no "resume anyway" path.
    Reject(RejectReason),
    /// The replacement cannot resume; restarting would re-transfer everything.
    RestartOnly,
}

impl Verdict {
    pub fn is_safe_to_resume(&self) -> bool {
        matches!(self, Self::Resume(_))
    }
    pub fn needs_user(&self) -> bool {
        matches!(self, Self::Confirm(_) | Self::RestartOnly)
    }
}

/// Turn a preliminary plus content-check outcome into a final verdict.
///
/// A content mismatch is a **hard reject**, never a confirmable warning: letting a user click
/// past a proven byte mismatch is exactly how a 10 GB file gets corrupted.
pub fn finalize(pre: Preliminary, mut ev: Evidence, mismatch_at: Option<u64>, windows: usize) -> Verdict {
    if let Some(offset) = mismatch_at {
        return Verdict::Reject(RejectReason::ContentMismatch { offset });
    }
    ev.windows_verified = windows;
    match pre {
        Preliminary::Accept => Verdict::Resume(ev),
        Preliminary::VerifyThenAccept => Verdict::Resume(ev),
        Preliminary::VerifyThenConfirm => Verdict::Confirm(ev),
        Preliminary::Reject(r) => Verdict::Reject(r),
        Preliminary::RestartOnly => Verdict::RestartOnly,
    }
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn sig(size: u64, etag: Option<&str>, lm: Option<&str>) -> ResourceSignals {
        ResourceSignals {
            total_size: Some(size),
            etag: etag.and_then(Validator::parse),
            last_modified: lm.map(str::to_string),
            content_type: Some("application/octet-stream".into()),
            filename: Some("movie.mkv".into()),
            range_support: RangeSupport::Supported,
        }
    }

    #[test]
    fn identical_strong_validators_resume_with_no_traffic() {
        let old = sig(10 * MB, Some("\"abc\""), Some("Wed, 01 Jan 2025 00:00:00 GMT"));
        let (pre, ev) = compare(&old, &old.clone());
        assert_eq!(pre, Preliminary::Accept, "no content check needed");
        assert!(ev.size_matches && ev.etag_matches);
        assert!(finalize(pre, ev, None, 0).is_safe_to_resume());
    }

    #[test]
    fn a_size_mismatch_is_rejected_before_any_fetch() {
        // The one genuinely conclusive header.
        let old = sig(10 * MB, Some("\"abc\""), None);
        let new = sig(12 * MB, Some("\"abc\""), None);
        let (pre, _) = compare(&old, &new);
        assert_eq!(
            pre,
            Preliminary::Reject(RejectReason::SizeMismatch { old: 10 * MB, new: 12 * MB })
        );
    }

    #[test]
    fn size_mismatch_message_avoids_jargon() {
        let r = RejectReason::SizeMismatch { old: 10 * MB, new: 12 * MB };
        let m = r.user_message();
        assert!(m.contains("10.0 MB") && m.contains("12.0 MB"), "{m}");
        for jargon in ["ETag", "Range", "206", "byte offset", "Content-Length"] {
            assert!(!m.contains(jargon), "message leaked jargon {jargon:?}: {m}");
        }
    }

    #[test]
    fn a_changed_etag_escalates_rather_than_rejecting() {
        // The central case. A different CDN edge, an S3 multipart re-upload, or an
        // inode-derived ETag all change this value with byte-identical content.
        let old = sig(10 * MB, Some("\"abc\""), None);
        let new = sig(10 * MB, Some("\"xyz\""), None);
        let (pre, ev) = compare(&old, &new);

        assert_eq!(pre, Preliminary::VerifyThenConfirm, "must not reject on ETag alone");
        assert!(ev.etag_changed);
        assert!(ev.size_matches);

        // With content verified, the user is asked rather than blocked.
        let v = finalize(pre, ev, None, 4);
        assert!(matches!(v, Verdict::Confirm(_)));
        assert!(v.needs_user());
    }

    #[test]
    fn a_content_mismatch_is_a_hard_reject_with_no_override() {
        // Letting a user click past a proven byte mismatch is how a 10 GB file gets corrupted.
        let old = sig(10 * MB, Some("\"abc\""), None);
        let new = sig(10 * MB, Some("\"xyz\""), None);
        let (pre, ev) = compare(&old, &new);

        let v = finalize(pre, ev, Some(4096), 4);
        assert_eq!(v, Verdict::Reject(RejectReason::ContentMismatch { offset: 4096 }));
        assert!(!v.is_safe_to_resume());
        assert!(!v.needs_user(), "a rejection must not offer a confirmation path");
    }

    #[test]
    fn a_content_mismatch_overrides_even_perfect_headers() {
        let old = sig(10 * MB, Some("\"abc\""), None);
        let (pre, ev) = compare(&old, &old.clone());
        assert_eq!(pre, Preliminary::Accept);
        // Content always outranks headers.
        assert!(matches!(finalize(pre, ev, Some(0), 4), Verdict::Reject(_)));
    }

    #[test]
    fn mismatch_message_does_not_assign_blame() {
        // We genuinely cannot tell whether the link is wrong or the local file is damaged.
        let m = RejectReason::ContentMismatch { offset: 999 }.user_message();
        assert!(m.contains("different file"), "{m}");
        assert!(m.contains("damaged"), "must admit the local file could be at fault: {m}");
    }

    #[test]
    fn matching_filename_alone_is_never_sufficient() {
        // A decoy matches name, size and type; only content separates them.
        let old = sig(10 * MB, None, None);
        let new = sig(10 * MB, None, None);
        let (pre, ev) = compare(&old, &new);
        assert!(ev.filename_matches && ev.content_type_matches && ev.size_matches);
        assert_ne!(pre, Preliminary::Accept, "weak signals must not auto-resume");
        assert_eq!(pre, Preliminary::VerifyThenConfirm);
    }

    #[test]
    fn matching_last_modified_without_etags_verifies_then_resumes() {
        let old = sig(10 * MB, None, Some("Wed, 01 Jan 2025 00:00:00 GMT"));
        let new = sig(10 * MB, None, Some("Wed, 01 Jan 2025 00:00:00 GMT"));
        let (pre, _) = compare(&old, &new);
        assert_eq!(pre, Preliminary::VerifyThenAccept);
    }

    #[test]
    fn weak_etags_never_auto_accept_on_headers_alone() {
        let old = sig(10 * MB, Some("W/\"abc\""), None);
        let new = sig(10 * MB, Some("W/\"abc\""), None);
        let (pre, _) = compare(&old, &new);
        assert_eq!(
            pre,
            Preliminary::VerifyThenAccept,
            "a weak validator asserts equivalence, not byte identity"
        );
    }

    #[test]
    fn a_replacement_without_range_support_can_only_restart() {
        let old = sig(10 * MB, Some("\"abc\""), None);
        let mut new = sig(10 * MB, Some("\"abc\""), None);
        new.range_support = RangeSupport::Unsupported;

        let (pre, _) = compare(&old, &new);
        assert_eq!(pre, Preliminary::RestartOnly);
        let v = finalize(pre, Evidence::default(), None, 0);
        assert_eq!(v, Verdict::RestartOnly);
        assert!(v.needs_user(), "the user must choose; we never silently re-download 10 GB");
    }

    #[test]
    fn windows_are_inside_completed_data_only() {
        // Verifying a byte we never downloaded would compare against a hole.
        let completed = RangeSet::from_pairs([(0, 3 * MB), (10 * MB, 3 * MB)]);
        for (off, len) in choose_windows(&completed, "abc123", WINDOW_COUNT) {
            assert!(
                completed.contains_all(off, off + len),
                "window {off}..{} is not fully downloaded",
                off + len
            );
        }
    }

    #[test]
    fn windows_cover_the_first_and_last_downloaded_bytes() {
        let completed = RangeSet::from_pairs([(0, 8 * MB)]);
        let w = choose_windows(&completed, "abc123", WINDOW_COUNT);
        assert_eq!(w.first().unwrap().0, 0, "must check the very start");
        assert_eq!(
            w.last().unwrap().0,
            8 * MB - WINDOW,
            "must check the end, where an off-by-one would surface"
        );
    }

    #[test]
    fn window_placement_is_reproducible_per_download() {
        let completed = RangeSet::from_pairs([(0, 100 * MB)]);
        assert_eq!(
            choose_windows(&completed, "download-a", WINDOW_COUNT),
            choose_windows(&completed, "download-a", WINDOW_COUNT),
            "a retried validation must check the same windows"
        );
    }

    #[test]
    fn window_placement_is_unpredictable_across_downloads() {
        // So a hostile server cannot pre-position matching decoy bytes.
        let completed = RangeSet::from_pairs([(0, 100 * MB)]);
        let a = choose_windows(&completed, "download-a", WINDOW_COUNT);
        let b = choose_windows(&completed, "download-b", WINDOW_COUNT);
        assert_ne!(a, b, "windows must differ between downloads");
    }

    #[test]
    fn verification_cost_stays_within_the_promised_budget() {
        // The feature exists to avoid re-transferring data, so its own traffic must be tiny.
        let completed = RangeSet::from_pairs([(0, 6_500 * MB)]);
        let bytes: u64 = choose_windows(&completed, "big", WINDOW_COUNT).iter().map(|(_, l)| l).sum();
        assert!(bytes <= 1024 * 1024, "verification would transfer {bytes} bytes");
        assert!(
            (bytes as f64 / (6_500.0 * MB as f64)) < 0.0001,
            "verification overhead exceeds 0.01% of the file"
        );
    }

    #[test]
    fn no_windows_when_nothing_has_been_downloaded() {
        assert!(choose_windows(&RangeSet::new(), "x", WINDOW_COUNT).is_empty());
    }

    #[test]
    fn first_difference_locates_the_mismatch() {
        assert_eq!(first_difference(1000, b"hello", b"hello"), None);
        assert_eq!(first_difference(1000, b"hello", b"heLlo"), Some(1002));
        assert_eq!(first_difference(0, b"abc", b"ab"), Some(2), "short read is a mismatch");
    }

    #[test]
    fn evidence_summary_is_plain_language() {
        let ev = Evidence { etag_changed: true, size_matches: true, windows_verified: 4, ..Default::default() };
        let s = ev.user_summary();
        assert!(s.contains("4 sample sections"), "{s}");
        for jargon in ["ETag", "Range", "206", "byte", "offset"] {
            assert!(!s.contains(jargon), "leaked jargon {jargon:?}: {s}");
        }
    }
}

// ─────────────────────────── the verification runner ───────────────────────────

use crate::{
    config::{RequestSpec, Settings},
    fsx,
    http::client,
    task::probe::{probe, ProbeError},
};
use std::path::Path;

/// The full result of validating a replacement URL. Produced without mutating anything, so the
/// UI can show the evidence before the user commits to a swap.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    pub verdict: Verdict,
    /// What the replacement URL says about itself.
    pub new_signals: ResourceSignals,
    pub resolved_url: String,
    pub windows_checked: usize,
    /// Bytes spent proving identity. The feature exists to avoid re-transferring data, so this
    /// is held to a hard budget and reported honestly.
    pub bytes_verified: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum RefreshError {
    #[error("could not reach the replacement link: {0}")]
    Probe(#[from] ProbeError),
    #[error("could not read the existing partial file: {0}")]
    Io(#[from] std::io::Error),
    #[error("client setup failed: {0}")]
    Client(String),
}

/// Validate a replacement URL against an existing partial download.
///
/// Mutates nothing: this is the read-only half of the swap, so the UI can present evidence and
/// let the user decide before any state changes.
pub async fn validate_replacement_url(
    new_url: &str,
    old: &ResourceSignals,
    part_path: &Path,
    completed: &RangeSet,
    download_id: &str,
    settings: &Settings,
    spec: &RequestSpec,
) -> Result<ValidationReport, RefreshError> {
    // A full probe of the replacement. The old capabilities are never carried over: the new
    // link may resolve to a different host with different range support entirely.
    let pr = probe(new_url, settings, spec, false).await?;
    let new_signals = ResourceSignals::from_probe(&pr);

    let (pre, ev) = compare(old, &new_signals);

    // Content is verified whenever there is anything to verify against — even when the headers
    // agree perfectly.
    //
    // Tier 0 (size plus a matching strong ETag) is strong evidence that the *server* is serving
    // the same resource, but it says nothing about the bytes already on *our* disk. A partial
    // file damaged by a bad sector, a truncated write, or an unrelated process would sail
    // straight through a header comparison and be resumed over, producing a right-sized,
    // wrong-content file. Since the check costs about 256 KB — roughly 0.003% of a 10 GB
    // download — there is no case for skipping it.
    //
    // So Tier 0 now means "no *prompt* is needed", not "no verification is needed".
    let verifiable = completed.total() > 0
        && pr.range_support == RangeSupport::Supported
        && !matches!(pre, Preliminary::Reject(_) | Preliminary::RestartOnly);
    if !verifiable {
        return Ok(ValidationReport {
            verdict: finalize(pre, ev, None, 0),
            new_signals,
            resolved_url: pr.final_url.to_string(),
            windows_checked: 0,
            bytes_verified: 0,
        });
    }

    let windows = choose_windows(completed, download_id, WINDOW_COUNT);
    let http = client::build(settings, spec, &pr.final_url).map_err(|e| RefreshError::Client(e.to_string()))?;
    let file = std::fs::File::open(part_path)?;

    let mut bytes_verified = 0u64;
    let mut mismatch = None;

    for (offset, len) in &windows {
        let (off, len) = (*offset, *len);

        // What we already have on disk.
        let mut local = vec![0u8; len as usize];
        let read = fsx::read_at(&file, &mut local, off)?;
        local.truncate(read);

        // The same window from the replacement URL.
        let resp = client::apply_spec(http.get(pr.final_url.clone()), spec)
            .header(reqwest::header::RANGE, format!("bytes={}-{}", off, off + len - 1))
            .send()
            .await;

        let remote = match resp {
            Ok(r) if r.status() == 206 => match r.bytes().await {
                Ok(b) => b,
                Err(e) => return Err(RefreshError::Client(e.to_string())),
            },
            // Anything other than a proper partial response means we cannot confirm the bytes
            // line up, and "cannot confirm" must never become "assume fine".
            Ok(r) => {
                return Ok(ValidationReport {
                    verdict: Verdict::Reject(RejectReason::ContentMismatch { offset: off }),
                    new_signals,
                    resolved_url: pr.final_url.to_string(),
                    windows_checked: bytes_verified as usize,
                    bytes_verified: {
                        let _ = r;
                        bytes_verified
                    },
                });
            }
            Err(e) => return Err(RefreshError::Client(e.to_string())),
        };

        bytes_verified += remote.len() as u64;
        if let Some(at) = first_difference(off, &local, &remote) {
            mismatch = Some(at);
            break; // one mismatch is conclusive; no point spending more traffic
        }
    }

    Ok(ValidationReport {
        verdict: finalize(pre, ev, mismatch, windows.len()),
        new_signals,
        resolved_url: pr.final_url.to_string(),
        windows_checked: windows.len(),
        bytes_verified,
    })
}
