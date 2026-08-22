//! Persistence behaviour, especially the parts crash recovery depends on.

use swiftload_core::{
    store::{models::*, Store},
    util::intervals::RangeSet,
};

fn rec() -> DownloadRecord {
    let mut d = DownloadRecord::new(
        "https://cdn.example.com/movie.mkv?token=SECRET&expires=123",
        "movie.mkv",
        "/downloads",
        "/downloads/movie.mkv.slpart",
    );
    d.total_size = Some(10_000_000_000);
    d.identity_hint = "hint-abc".into();
    d
}

#[test]
fn round_trips_a_download() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    let got = s.get(&d.id).unwrap().expect("record should exist");
    assert_eq!(got.filename, "movie.mkv");
    assert_eq!(got.total_size, Some(10_000_000_000));
    assert_eq!(got.status, DownloadStatus::Queued);
    assert_eq!(got.url_refresh_count, 0);
}

#[test]
fn checkpoints_survive_and_stay_exact() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    let ranges = RangeSet::from_pairs([(0, 1_000_000), (5_000_000, 2_000_000)]);
    s.checkpoint(&d.id, &ranges, ranges.total()).unwrap();

    let got = s.get(&d.id).unwrap().unwrap();
    assert_eq!(got.completed_ranges, ranges, "the fragmented set must round-trip exactly");
    assert_eq!(got.bytes_done, 3_000_000);
}

#[test]
fn a_checkpoint_for_an_unknown_download_is_an_error_not_a_silent_no_op() {
    let s = Store::open_in_memory().unwrap();
    assert!(s.checkpoint("nope", &RangeSet::new(), 0).is_err());
}

#[test]
fn an_unclean_shutdown_triggers_the_paranoid_rewind() {
    // fsync is honoured by essentially every modern drive, but some consumer SSDs and
    // virtualised storage have been observed to lie. Giving back a megabyte per fragment is
    // trivial insurance against a corruption class we could not otherwise detect.
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    let ranges = RangeSet::from_pairs([(0, 50 * 1024 * 1024), (100 * 1024 * 1024, 50 * 1024 * 1024)]);
    s.checkpoint(&d.id, &ranges, ranges.total()).unwrap();

    // No clean-shutdown flag: we crashed.
    let (recovered, unclean) = s.resume_state(&d.id, true).unwrap();
    assert!(unclean);
    assert!(recovered.total() < ranges.total(), "nothing was given back");
    assert_eq!(ranges.total() - recovered.total(), 2 * 1024 * 1024, "one megabyte per fragment");
}

#[test]
fn a_clean_shutdown_keeps_every_byte() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    let ranges = RangeSet::from_pairs([(0, 50 * 1024 * 1024)]);
    s.checkpoint(&d.id, &ranges, ranges.total()).unwrap();
    s.mark_clean(&d.id, true).unwrap();

    let (recovered, unclean) = s.resume_state(&d.id, true).unwrap();
    assert!(!unclean);
    assert_eq!(recovered, ranges, "a clean stop must not cost the user any progress");
}

#[test]
fn startup_clears_clean_flags_so_a_crash_now_is_still_detected() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();
    s.mark_clean(&d.id, true).unwrap();

    s.clear_clean_flags().unwrap();
    assert!(!s.get(&d.id).unwrap().unwrap().clean_shutdown);
}

#[test]
fn swapping_a_url_preserves_every_byte_of_progress() {
    // The whole point of the feature: a new link must not cost the user their download.
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    let ranges = RangeSet::from_pairs([(0, 6_500_000_000)]);
    s.checkpoint(&d.id, &ranges, ranges.total()).unwrap();

    s.swap_url(
        &d.id,
        "https://cdn.example.com/movie.mkv?token=FRESH&expires=999",
        "https://edge7.example.com/movie.mkv",
        Some("\"new-etag\""),
        Some("Wed, 01 Jan 2025 00:00:00 GMT"),
        Some(10_000_000_000),
        1,
        ValidationState::ContentVerified,
        "accepted_confirmed",
    )
    .unwrap();

    let got = s.get(&d.id).unwrap().unwrap();
    assert_eq!(got.completed_ranges, ranges, "progress must be untouched by a URL swap");
    assert_eq!(got.bytes_done, 6_500_000_000);
    assert_eq!(got.url_refresh_count, 1);
    assert_eq!(got.validation_state, ValidationState::ContentVerified);
    assert_eq!(got.id, d.id, "identity is the download, not the link");

    // The original URL is never overwritten: it is what an expired link is re-resolved from.
    assert_eq!(got.original_url, d.original_url);
    assert!(got.current_url.contains("FRESH"));
}

#[test]
fn url_history_is_redacted() {
    // A signed URL is a bearer credential. Parameter names are useful for diagnostics; values
    // are secrets and must never be persisted outside the columns that have to fetch with them.
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    s.swap_url(
        &d.id,
        "https://b.s3.amazonaws.com/k?X-Amz-Signature=deadbeefcafe&X-Amz-Credential=AKIAEXAMPLE",
        "https://b.s3.amazonaws.com/k",
        None,
        None,
        None,
        1,
        ValidationState::AutoVerified,
        "accepted_auto",
    )
    .unwrap();

    let hist = s.url_history(&d.id).unwrap();
    assert_eq!(hist.len(), 2, "the original and the replacement");
    for e in &hist {
        for secret in ["SECRET", "deadbeefcafe", "AKIAEXAMPLE"] {
            assert!(!e.url_redacted.contains(secret), "leaked {secret} in {}", e.url_redacted);
        }
    }
    assert!(hist[1].url_redacted.contains("X-Amz-Signature="), "parameter names should survive");
    assert_eq!(hist[1].host, "b.s3.amazonaws.com");
}

#[test]
fn url_history_records_what_was_preserved_at_each_swap() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();
    s.checkpoint(&d.id, &RangeSet::from_pairs([(0, 6_500_000_000)]), 6_500_000_000).unwrap();
    s.swap_url(&d.id, "https://x/y?t=1", "https://x/y", None, None, None, 1, ValidationState::AutoVerified, "accepted_auto")
        .unwrap();

    let hist = s.url_history(&d.id).unwrap();
    assert_eq!(hist[1].bytes_done_at_swap, 6_500_000_000, "the evidence trail of what was saved");
}

#[test]
fn a_chain_of_swaps_keeps_one_download_and_one_history() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    for i in 0..4 {
        s.checkpoint(&d.id, &RangeSet::from_pairs([(0, (i + 1) * 1_000_000)]), (i + 1) * 1_000_000).unwrap();
        s.swap_url(&d.id, &format!("https://x/y?t={i}"), "https://x/y", None, None, None, 1, ValidationState::ContentVerified, "accepted_auto")
            .unwrap();
    }

    let got = s.get(&d.id).unwrap().unwrap();
    assert_eq!(got.url_refresh_count, 4);
    assert_eq!(got.id, d.id);
    let hist = s.url_history(&d.id).unwrap();
    assert_eq!(hist.len(), 5, "original plus four replacements");
    let saved: Vec<u64> = hist.iter().map(|h| h.bytes_done_at_swap).collect();
    assert!(saved.windows(2).all(|w| w[1] >= w[0]), "preserved bytes must only grow: {saved:?}");
}

#[test]
fn duplicate_detection_finds_only_resumable_downloads() {
    let s = Store::open_in_memory().unwrap();

    let mut paused = rec();
    paused.status = DownloadStatus::Paused;
    s.insert(&paused).unwrap();
    s.set_status(&paused.id, DownloadStatus::Paused, None).unwrap();

    let mut done = rec();
    done.identity_hint = "hint-abc".into();
    s.insert(&done).unwrap();
    s.set_status(&done.id, DownloadStatus::Completed, None).unwrap();

    let found = s.find_resumable_by_hint("hint-abc").unwrap();
    assert_eq!(found.len(), 1, "a finished download is not something to resume");
    assert_eq!(found[0].id, paused.id);
}

#[test]
fn status_transitions_and_listing() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();

    assert_eq!(s.list(Some(DownloadStatus::Queued)).unwrap().len(), 1);
    s.set_status(&d.id, DownloadStatus::Failed, Some("link expired")).unwrap();

    let got = s.get(&d.id).unwrap().unwrap();
    assert_eq!(got.status, DownloadStatus::Failed);
    assert_eq!(got.error_message.as_deref(), Some("link expired"));
    assert!(got.status.is_resumable(), "a failed download can still be retried");

    assert!(s.list(Some(DownloadStatus::Queued)).unwrap().is_empty());
    assert_eq!(s.list(None).unwrap().len(), 1);
}

#[test]
fn host_profiles_are_learned_and_updated() {
    // So the second download from a host starts at the right concurrency instead of
    // re-discovering it.
    let s = Store::open_in_memory().unwrap();
    assert!(s.host_profile("cdn.example.com").unwrap().is_none());

    s.record_host_profile("cdn.example.com", 8, 20_000_000, false).unwrap();
    let p = s.host_profile("cdn.example.com").unwrap().unwrap();
    assert_eq!(p.best_observed_conns, Some(8));
    assert_eq!(p.samples, 1);

    s.record_host_profile("cdn.example.com", 4, 18_000_000, true).unwrap();
    let p = s.host_profile("cdn.example.com").unwrap().unwrap();
    assert_eq!(p.best_observed_conns, Some(4));
    assert!(p.saturation_detected);
    assert_eq!(p.samples, 2);
}

#[test]
fn survives_reopening_the_database() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("swiftload.db");
    let ranges = RangeSet::from_pairs([(0, 123_456), (900_000, 1000)]);

    let id = {
        let s = Store::open(&path).unwrap();
        let d = rec();
        s.insert(&d).unwrap();
        s.checkpoint(&d.id, &ranges, ranges.total()).unwrap();
        s.mark_clean(&d.id, true).unwrap();
        d.id
    };

    let s = Store::open(&path).unwrap();
    let got = s.get(&id).unwrap().unwrap();
    assert_eq!(got.completed_ranges, ranges);
    assert!(got.clean_shutdown, "the flag must persist across a reopen");
}

#[test]
fn deleting_a_download_removes_its_history() {
    let s = Store::open_in_memory().unwrap();
    let d = rec();
    s.insert(&d).unwrap();
    assert_eq!(s.url_history(&d.id).unwrap().len(), 1);

    s.delete(&d.id).unwrap();
    assert!(s.get(&d.id).unwrap().is_none());
    assert!(s.url_history(&d.id).unwrap().is_empty(), "history must cascade");
}
