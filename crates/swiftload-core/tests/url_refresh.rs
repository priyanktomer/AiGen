//! Resuming a download with a replaced URL.
//!
//! The scenario: a large download is interrupted, its signed link expires, and the user
//! obtains a fresh one. The bytes already on disk must survive — but only if the new link
//! really serves the same file. Every test here ends by verifying the final file byte-for-byte
//! against the server's content function, because a resume that "succeeds" onto the wrong
//! bytes is the failure this whole mechanism exists to prevent.

use std::sync::{Arc, Mutex};
use swiftload_core::{
    config::{RequestSpec, Settings},
    task::{
        download,
        identity::{
            validate_replacement_url, RejectReason, ResourceSignals, Verdict, WINDOW_COUNT,
        },
        probe::probe,
        writer::CheckpointSink,
        DownloadRequest,
    },
    util::intervals::RangeSet,
};
use swiftload_testserver as ts;
use tokio_util::sync::CancellationToken;

struct MemSink(Mutex<RangeSet>);
impl MemSink {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(RangeSet::new())))
    }
    fn ranges(&self) -> RangeSet {
        self.0.lock().unwrap().clone()
    }
}
impl CheckpointSink for MemSink {
    fn checkpoint(&self, r: &RangeSet, _b: u64) -> std::io::Result<()> {
        *self.0.lock().unwrap() = r.clone();
        Ok(())
    }
}

fn settings(dir: &std::path::Path) -> Settings {
    Settings {
        download_dir: dir.to_path_buf(),
        hash_on_complete: false,
        ..Default::default()
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

fn assert_content(path: &std::path::Path, seed: &str, size: u64) {
    let actual = std::fs::read(path).expect("read file");
    assert_eq!(actual.len() as u64, size, "size mismatch");
    let expected = ts::content::chunk(ts::content::seed_of(seed), 0, size as usize);
    if actual != expected {
        let at = actual.iter().zip(&expected).position(|(a, b)| a != b);
        panic!("content mismatch at byte {at:?} of {size}");
    }
}

/// Start a download and interrupt it once a target amount has been checkpointed.
///
/// Cancellation is driven by **observed progress**, not by a wall-clock timer. A fixed sleep
/// makes these tests flaky the moment the machine is loaded or the engine gets faster: the
/// download either finishes early (and the test asserts on a complete file) or barely starts.
/// Watching the checkpoint sink instead makes the interruption land in the same place every
/// time, on any machine.
async fn partial_download_to(
    url: &str,
    dir: &std::path::Path,
    name: &str,
    stop_after: u64,
) -> (RangeSet, ResourceSignals) {
    let s = settings(dir);
    let pr = probe(url, &s, &RequestSpec::default(), false)
        .await
        .unwrap();
    let old = ResourceSignals::from_probe(&pr);

    let sink = MemSink::new();
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            // Generous ceiling so a genuinely stuck download fails the test rather than hanging.
            for _ in 0..600 {
                if sink.ranges().total() >= stop_after {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            cancel.cancel();
        });
    }
    let mut req = DownloadRequest::new(url.to_string(), dir);
    req.filename = Some(name.into());
    req.max_conns = Some(4);
    let _ = download(req, s, sink.clone(), cancel, None).await;

    let ranges = sink.ranges();
    assert!(
        ranges.total() > 0,
        "nothing was downloaded before the interruption"
    );
    if let Some(total) = pr.total_size {
        assert!(
            ranges.total() < total,
            "the download finished ({} of {total} bytes) before it could be interrupted",
            ranges.total()
        );
    }
    assert!(
        dir.join(format!("{name}.slpart")).exists(),
        "no partial file was left behind"
    );
    (ranges, old)
}

/// Interrupt after roughly a quarter of the file, which every test here wants.
async fn partial_download(
    url: &str,
    dir: &std::path::Path,
    name: &str,
    _ms: u64,
) -> (RangeSet, ResourceSignals) {
    // One byte is enough: the first checkpoint lands at 8 MiB, roughly a quarter of the file,
    // which is what every test here wants and is stable regardless of machine speed.
    partial_download_to(url, dir, name, 1).await
}

const SIZE: u64 = 32 * 1024 * 1024;

/// Test 1 + 4: an expired signed link is replaced and the download resumes.
#[tokio::test]
async fn expired_link_is_replaced_and_the_download_resumes() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let c = client();

    let url = c
        .get(h.url(&format!("/mint/alpha?n={SIZE}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let throttled = format!("{url}&bps=1500000&per=conn");

    let (partial, old) = partial_download(&throttled, dir.path(), "movie.mkv", 1500).await;
    assert!(
        partial.total() < SIZE,
        "the download completed before it could be interrupted"
    );

    // The link expires while the download is paused.
    c.get(h.url("/expire/alpha")).send().await.unwrap();
    assert_eq!(c.get(&url).send().await.unwrap().status(), 403);

    // The user obtains a fresh link for the same file.
    let fresh = c
        .get(h.url(&format!("/mint/alpha?n={SIZE}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_ne!(fresh, url);

    let part = dir.path().join("movie.mkv.slpart");
    let before = h.stats().bytes_served;

    let report = validate_replacement_url(
        &fresh,
        &old,
        &part,
        &partial,
        "dl-1",
        &settings(dir.path()),
        &RequestSpec::default(),
    )
    .await
    .unwrap();

    assert!(
        report.verdict.is_safe_to_resume(),
        "same file should resume without a prompt: {:?}",
        report.verdict
    );
    assert!(
        report.bytes_verified <= 1024 * 1024,
        "validation transferred {} bytes; the budget is 1 MB",
        report.bytes_verified
    );

    // Resume onto the replacement URL, keeping the partial.
    let mut req = DownloadRequest::new(fresh, dir.path());
    req.filename = Some("movie.mkv".into());
    req.max_conns = Some(4);
    req.completed = partial.clone();
    let o = download(
        req,
        settings(dir.path()),
        MemSink::new(),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_eq!(
        o.resumed_from,
        partial.total(),
        "existing bytes must be kept"
    );
    assert_eq!(o.bytes, SIZE);
    assert_content(&o.path, "alpha", SIZE);

    let transferred = h.stats().bytes_served - before;
    let missing = SIZE - partial.total();
    assert!(
        transferred < missing + 4 * 1024 * 1024,
        "transferred {transferred} for {missing} bytes of missing data — it restarted"
    );
}

/// Test 5: same filename, same size, different content. The decoy must be refused.
#[tokio::test]
async fn a_decoy_with_matching_size_is_rejected_on_content() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let url = h.url(&format!("/plain/real/{SIZE}?bps=1500000&per=conn"));
    let (partial, old) = partial_download(&url, dir.path(), "movie.mkv", 1500).await;

    // Same size, same name, entirely different bytes. Headers alone cannot tell them apart.
    let decoy = h.url(&format!("/decoy/imposter/{SIZE}"));
    let report = validate_replacement_url(
        &decoy,
        &old,
        &dir.path().join("movie.mkv.slpart"),
        &partial,
        "dl-decoy",
        &settings(dir.path()),
        &RequestSpec::default(),
    )
    .await
    .unwrap();

    assert!(
        matches!(
            report.verdict,
            Verdict::Reject(RejectReason::ContentMismatch { .. })
        ),
        "decoy was not rejected: {:?}",
        report.verdict
    );
    assert!(!report.verdict.is_safe_to_resume());
    assert!(
        !report.verdict.needs_user(),
        "a rejection must offer no confirmation path"
    );
    assert!(
        report.windows_checked > 0,
        "content must actually have been compared"
    );
}

/// Test 6: a different size is refused before any content is fetched.
#[tokio::test]
async fn a_size_mismatch_is_rejected_without_spending_traffic() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let url = h.url(&format!("/plain/sized/{SIZE}?bps=1500000&per=conn"));
    let (partial, old) = partial_download(&url, dir.path(), "movie.mkv", 1200).await;

    let bigger = h.url(&format!("/plain/sized/{}", SIZE + 8 * 1024 * 1024));
    let report = validate_replacement_url(
        &bigger,
        &old,
        &dir.path().join("movie.mkv.slpart"),
        &partial,
        "dl-size",
        &settings(dir.path()),
        &RequestSpec::default(),
    )
    .await
    .unwrap();

    assert!(
        matches!(
            report.verdict,
            Verdict::Reject(RejectReason::SizeMismatch { .. })
        ),
        "{:?}",
        report.verdict
    );
    assert_eq!(
        report.bytes_verified, 0,
        "size alone settles it; no content should be fetched"
    );
}

/// Test 7: the ETag rotates but the content is identical — the case that must not be rejected.
#[tokio::test]
async fn a_rotated_etag_verifies_by_content_and_asks_the_user() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let url = h.url(&format!("/plain/rot/{SIZE}?bps=1500000&per=conn"));
    let (partial, old) = partial_download(&url, dir.path(), "movie.mkv", 1500).await;

    // Same bytes, different ETag on every response — a different CDN edge would behave this way.
    let rotated = h.url(&format!("/rotate-etag/rot/{SIZE}"));
    let report = validate_replacement_url(
        &rotated,
        &old,
        &dir.path().join("movie.mkv.slpart"),
        &partial,
        "dl-rot",
        &settings(dir.path()),
        &RequestSpec::default(),
    )
    .await
    .unwrap();

    match &report.verdict {
        Verdict::Confirm(ev) => {
            assert!(
                ev.etag_changed,
                "the changed tag should be part of the evidence"
            );
            assert!(ev.size_matches);
            assert_eq!(ev.windows_verified, report.windows_checked);
            // The prompt must be readable by someone who has never heard of an ETag.
            let s = ev.user_summary();
            for jargon in ["ETag", "Range", "206", "offset"] {
                assert!(!s.contains(jargon), "leaked jargon {jargon:?}: {s}");
            }
        }
        other => panic!("a rotated ETag with matching content should ask, not refuse: {other:?}"),
    }

    // And resuming after confirmation must still produce the right file.
    let mut req = DownloadRequest::new(rotated, dir.path());
    req.filename = Some("movie.mkv".into());
    req.completed = partial;
    let o = download(
        req,
        settings(dir.path()),
        MemSink::new(),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    assert_content(&o.path, "rot", SIZE);
}

/// Test 8: the replacement cannot serve ranges, so resuming is impossible.
#[tokio::test]
async fn a_replacement_without_range_support_offers_restart_only() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let url = h.url(&format!("/plain/nr/{SIZE}?bps=1500000&per=conn"));
    let (partial, old) = partial_download(&url, dir.path(), "movie.mkv", 1200).await;

    let norange = h.url(&format!("/norange/nr/{SIZE}"));
    let report = validate_replacement_url(
        &norange,
        &old,
        &dir.path().join("movie.mkv.slpart"),
        &partial,
        "dl-nr",
        &settings(dir.path()),
        &RequestSpec::default(),
    )
    .await
    .unwrap();

    assert_eq!(report.verdict, Verdict::RestartOnly);
    assert!(
        report.verdict.needs_user(),
        "the user must choose; never silently re-download"
    );
    assert!(
        dir.path().join("movie.mkv.slpart").exists(),
        "the partial file must be preserved regardless of the choice"
    );
}

/// Test 9: the same logical download survives a chain of replacement URLs.
#[tokio::test]
async fn a_chain_of_replacements_keeps_one_download() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let c = client();

    let first = c
        .get(h.url(&format!("/mint/chain?n={SIZE}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let (mut partial, old) = partial_download(
        &format!("{first}&bps=1500000&per=conn"),
        dir.path(),
        "movie.mkv",
        1200,
    )
    .await;
    let part = dir.path().join("movie.mkv.slpart");

    let mut totals = vec![partial.total()];

    // Expire and replace twice more, advancing by one checkpoint each round.
    //
    // Two rounds, not three: progress is only observable at checkpoint granularity (8 MiB), so
    // each round advances by about that much. A third round would finish the 32 MiB file, and a
    // completed download renames its .slpart away — after which the final step legitimately has
    // nothing to resume from and the test would be asserting against its own setup.
    for round in 0..2 {
        c.get(h.url("/expire/chain")).send().await.unwrap();
        let fresh = c
            .get(h.url(&format!("/mint/chain?n={SIZE}")))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        let report = validate_replacement_url(
            &fresh,
            &old,
            &part,
            &partial,
            "dl-chain",
            &settings(dir.path()),
            &RequestSpec::default(),
        )
        .await
        .unwrap();
        assert!(
            report.verdict.is_safe_to_resume(),
            "round {round}: {:?}",
            report.verdict
        );

        let sink = MemSink::new();
        let cancel = CancellationToken::new();
        {
            // Stop at the first checkpoint past what we carried in. Keyed on observed
            // progress rather than a timer, so machine load cannot change where it lands.
            let target = partial.total() + 1;
            let cancel = cancel.clone();
            let sink = sink.clone();
            let carried = partial.clone();
            tokio::spawn(async move {
                for _ in 0..600 {
                    let seen = sink.ranges().total().max(carried.total());
                    if seen >= target {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                cancel.cancel();
            });
        }
        let mut req = DownloadRequest::new(format!("{fresh}&bps=1500000&per=conn"), dir.path());
        req.filename = Some("movie.mkv".into());
        req.max_conns = Some(4);
        req.completed = partial.clone();
        let _ = download(req, settings(dir.path()), sink.clone(), cancel, None).await;

        let next = sink.ranges();
        assert!(
            next.total() >= partial.total(),
            "round {round}: progress went backwards, {} -> {}",
            partial.total(),
            next.total()
        );
        partial = next;
        totals.push(partial.total());
    }

    // Finish on a final fresh link.
    c.get(h.url("/expire/chain")).send().await.unwrap();
    let last = c
        .get(h.url(&format!("/mint/chain?n={SIZE}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let mut req = DownloadRequest::new(last, dir.path());
    req.filename = Some("movie.mkv".into());
    req.max_conns = Some(4);
    req.completed = partial;
    let o = download(
        req,
        settings(dir.path()),
        MemSink::new(),
        CancellationToken::new(),
        None,
    )
    .await
    .unwrap();

    assert_content(&o.path, "chain", SIZE);
    assert!(
        totals.windows(2).all(|w| w[1] >= w[0]),
        "completed bytes must only ever grow across refreshes: {totals:?}"
    );
    assert!(
        o.resumed_from > 0,
        "the chain should have preserved real progress"
    );
}

/// Test 10: the local partial is damaged, so even a legitimate link must be refused.
#[tokio::test]
async fn a_corrupted_partial_file_is_detected_rather_than_resumed_over() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let url = h.url(&format!("/plain/corrupt/{SIZE}?bps=1500000&per=conn"));
    let (partial, old) = partial_download(&url, dir.path(), "movie.mkv", 1500).await;
    let part = dir.path().join("movie.mkv.slpart");

    // Damage the very first bytes, which the first verification window always covers.
    {
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::OpenOptions::new().write(true).open(&part).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0xAB; 4096]).unwrap();
        f.sync_all().unwrap();
    }

    // The link itself is perfectly valid — only the local data is wrong.
    let report = validate_replacement_url(
        &h.url(&format!("/plain/corrupt/{SIZE}")),
        &old,
        &part,
        &partial,
        "dl-corrupt",
        &settings(dir.path()),
        &RequestSpec::default(),
    )
    .await
    .unwrap();

    assert!(
        matches!(
            report.verdict,
            Verdict::Reject(RejectReason::ContentMismatch { .. })
        ),
        "damaged local data must not be resumed over: {:?}",
        report.verdict
    );

    // And the message must not blame the link, because we cannot tell which side is wrong.
    if let Verdict::Reject(r) = &report.verdict {
        let m = r.user_message();
        assert!(
            m.contains("damaged"),
            "must admit the local file could be at fault: {m}"
        );
    }
}

/// The verification budget, measured rather than asserted.
#[tokio::test]
async fn validation_cost_is_negligible_against_the_file_size() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let url = h.url(&format!("/plain/budget/{SIZE}?bps=1500000&per=conn"));
    let (partial, old) = partial_download(&url, dir.path(), "movie.mkv", 1500).await;

    let before = h.stats().bytes_served;
    let report = validate_replacement_url(
        &h.url(&format!("/rotate-etag/budget/{SIZE}")),
        &old,
        &dir.path().join("movie.mkv.slpart"),
        &partial,
        "dl-budget",
        &settings(dir.path()),
        &RequestSpec::default(),
    )
    .await
    .unwrap();

    let spent = h.stats().bytes_served - before;
    assert!(report.windows_checked <= WINDOW_COUNT);
    assert!(
        spent <= 1024 * 1024,
        "validation spent {spent} bytes, over the 1 MB budget"
    );
    assert!(
        (spent as f64 / partial.total() as f64) < 0.05,
        "validation cost {:.2}% of what was already downloaded",
        spent as f64 / partial.total() as f64 * 100.0
    );
}
