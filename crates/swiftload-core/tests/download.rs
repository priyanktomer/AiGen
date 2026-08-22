//! End-to-end download behaviour.
//!
//! Every test that produces a file verifies it **byte by byte** against the server's
//! deterministic content function. A download that finishes with the right size but the wrong
//! bytes is the failure mode that matters, and only a content check catches it.

use std::sync::{Arc, Mutex};
use swiftload_core::{
    config::Settings,
    task::{download, writer::CheckpointSink, DownloadError, DownloadRequest},
    util::intervals::RangeSet,
};
use swiftload_testserver as ts;
use tokio_util::sync::CancellationToken;

/// Captures checkpoints so a test can resume exactly as a restarted app would.
struct MemSink(Mutex<(RangeSet, u64)>);

impl MemSink {
    fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new((RangeSet::new(), 0))))
    }
    fn ranges(&self) -> RangeSet {
        self.0.lock().unwrap().0.clone()
    }
}

impl CheckpointSink for MemSink {
    fn checkpoint(&self, ranges: &RangeSet, bytes_done: u64) -> std::io::Result<()> {
        *self.0.lock().unwrap() = (ranges.clone(), bytes_done);
        Ok(())
    }
}

fn settings(dir: &std::path::Path) -> Settings {
    Settings { download_dir: dir.to_path_buf(), hash_on_complete: false, ..Default::default() }
}

/// Assert a file matches the server's generated content exactly.
fn assert_content(path: &std::path::Path, seed: &str, size: u64) {
    let actual = std::fs::read(path).expect("read downloaded file");
    assert_eq!(actual.len() as u64, size, "size mismatch");
    let expected = ts::content::chunk(ts::content::seed_of(seed), 0, size as usize);
    if actual != expected {
        let at = actual.iter().zip(&expected).position(|(a, b)| a != b);
        panic!("content mismatch at byte {at:?} of {size}");
    }
}

#[tokio::test]
async fn downloads_a_segmented_file_byte_exactly() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 24 * 1024 * 1024;

    let mut req = DownloadRequest::new(h.url(&format!("/plain/alpha/{SIZE}")), dir.path());
    req.filename = Some("out.bin".into());
    req.max_conns = Some(8);

    let o = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(o.bytes, SIZE);
    assert_content(&o.path, "alpha", SIZE);
    assert!(h.stats().accepts >= 2, "a segmented download should open several connections");
}

#[tokio::test]
async fn a_single_connection_download_is_also_byte_exact() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 4 * 1024 * 1024;

    let mut req = DownloadRequest::new(h.url(&format!("/plain/beta/{SIZE}")), dir.path());
    req.filename = Some("one.bin".into());
    req.max_conns = Some(1);

    let o = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_content(&o.path, "beta", SIZE);
}

struct NullS;
impl CheckpointSink for NullS {
    fn checkpoint(&self, _r: &RangeSet, _b: u64) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn each_worker_opens_its_own_tcp_connection() {
    // The h2-multiplexing trap: with a shared client, "parallel" segment requests can collapse
    // onto a single TCP connection, giving one congestion window and one loss-recovery domain
    // while looking perfectly healthy at the HTTP layer. Only an accept counter catches it.
    //
    // This server speaks HTTP/1.1 over plaintext, so it cannot reproduce h2 coalescing
    // directly; what it does prove is that the per-worker-client design produces genuinely
    // distinct connections rather than reusing one.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 64 * 1024 * 1024;
    const CONNS: usize = 8;

    let mut req = DownloadRequest::new(
        h.url(&format!("/throttle/gamma/{SIZE}?bps=4000000&per=conn")),
        dir.path(),
    );
    req.filename = Some("conns.bin".into());
    req.max_conns = Some(CONNS);

    let o = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();

    assert_content(&o.path, "gamma", SIZE);
    let accepts = h.stats().accepts;
    assert!(
        accepts >= CONNS as u64,
        "{CONNS} workers produced only {accepts} TCP connections — they are sharing sockets"
    );
    assert!(h.stats().peak_concurrent_streams >= 4, "streams did not actually overlap");
}

#[tokio::test]
async fn a_server_that_lies_about_ranges_does_not_corrupt_the_file() {
    // Advertises `Accept-Ranges: bytes`, then ignores Range and sends the whole body. A client
    // that trusts the advertisement writes full-file bytes at a segment offset and produces a
    // right-sized, wrong-content file — the exact failure this check exists to prevent.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 8 * 1024 * 1024;

    let mut req = DownloadRequest::new(h.url(&format!("/liar/delta/{SIZE}")), dir.path());
    req.filename = Some("liar.bin".into());
    req.max_conns = Some(4);

    let result = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None).await;

    match result {
        // Either we refuse outright...
        Err(e) => {
            let path = dir.path().join("liar.bin");
            assert!(!path.exists(), "a rejected download must not publish a file: {e}");
        }
        // ...or, if we produced a file at all, it must be correct. Never a silent corruption.
        Ok(o) => assert_content(&o.path, "delta", SIZE),
    }
}

#[tokio::test]
async fn a_server_without_range_support_still_downloads_correctly() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 3 * 1024 * 1024;

    let mut req = DownloadRequest::new(h.url(&format!("/norange/eps/{SIZE}")), dir.path());
    req.filename = Some("nr.bin".into());

    let o = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_content(&o.path, "eps", SIZE);
    assert_eq!(h.stats().accepts, 2, "must not open extra connections it cannot use");
}

#[tokio::test]
async fn resumes_from_a_checkpoint_without_refetching() {
    // The core resume property: bytes already on disk are never transferred again.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 32 * 1024 * 1024;
    let url = h.url(&format!("/throttle/zeta/{SIZE}?bps=3000000&per=conn"));

    // First attempt: cancel partway through.
    let sink = MemSink::new();
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            cancel.cancel();
        });
    }
    let mut req = DownloadRequest::new(url.clone(), dir.path());
    req.filename = Some("resume.bin".into());
    req.max_conns = Some(4);
    let first = download(req, settings(dir.path()), sink.clone(), cancel, None).await;
    assert!(matches!(first, Err(DownloadError::Cancelled)), "expected cancellation, got {first:?}");

    let partial = sink.ranges();
    assert!(partial.total() > 0, "nothing was checkpointed");
    assert!(partial.total() < SIZE, "the download finished before it could be interrupted");
    let bytes_before = h.stats().bytes_served;

    // Second attempt: resume from the checkpoint.
    let mut req = DownloadRequest::new(url, dir.path());
    req.filename = Some("resume.bin".into());
    req.max_conns = Some(4);
    req.completed = partial.clone();
    let o = download(req, settings(dir.path()), sink.clone(), CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(o.bytes, SIZE);
    assert_eq!(o.resumed_from, partial.total());
    assert_content(&o.path, "zeta", SIZE);

    let transferred = h.stats().bytes_served - bytes_before;
    let remaining = SIZE - partial.total();
    assert!(
        transferred < remaining + 4 * 1024 * 1024,
        "resume transferred {transferred} bytes for {remaining} bytes of missing data — \
         it is refetching data it already had"
    );
}

#[tokio::test]
async fn a_checkpoint_for_a_missing_partial_file_is_discarded() {
    // A checkpoint can outlive its data — the user deletes the .slpart, or it is replaced.
    //
    // This cannot be caught by comparing file lengths, because opening a part file
    // preallocates it to the full size: the file is always "long enough". Believing the
    // checkpoint anyway would leave the supposedly-downloaded region as preallocated zeros and
    // produce a right-sized, wrong-content file.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 4 * 1024 * 1024;

    let mut req = DownloadRequest::new(h.url(&format!("/plain/eta/{SIZE}")), dir.path());
    req.filename = Some("trunc.bin".into());
    // Claim almost the whole file, with no partial file to back it up.
    req.completed = RangeSet::from_pairs([(0, SIZE - 1000)]);

    let o = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(o.resumed_from, 0, "the phantom checkpoint must be discarded, not trusted");
    assert_content(&o.path, "eta", SIZE);
}

#[tokio::test]
async fn a_genuine_partial_file_is_still_resumed() {
    // The mirror of the test above: discarding phantom checkpoints must not throw away real
    // progress. Here the .slpart genuinely holds the first half of the file.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 4 * 1024 * 1024;
    const HAVE: u64 = 2 * 1024 * 1024;

    // Lay down a real partial file containing the correct first half.
    let part = dir.path().join("real.bin.slpart");
    let mut data = ts::content::chunk(ts::content::seed_of("mu"), 0, HAVE as usize);
    data.resize(SIZE as usize, 0);
    std::fs::write(&part, &data).unwrap();

    let bytes_before = h.stats().bytes_served;
    let mut req = DownloadRequest::new(h.url(&format!("/plain/mu/{SIZE}")), dir.path());
    req.filename = Some("real.bin".into());
    req.completed = RangeSet::from_pairs([(0, HAVE)]);

    let o = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();

    assert_eq!(o.resumed_from, HAVE, "real progress must be kept");
    assert_content(&o.path, "mu", SIZE);
    let transferred = h.stats().bytes_served - bytes_before;
    assert!(
        transferred < HAVE + 512 * 1024,
        "transferred {transferred} bytes when only {HAVE} were missing"
    );
}

#[tokio::test]
async fn recovers_from_mid_stream_connection_resets() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 8 * 1024 * 1024;

    let mut req = DownloadRequest::new(
        h.url(&format!("/plain/theta/{SIZE}?reset_at=524288")),
        dir.path(),
    );
    req.filename = Some("flaky.bin".into());
    req.max_conns = Some(2);

    let o = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();

    assert_content(&o.path, "theta", SIZE);
    assert!(o.retries > 0, "the reset should have been retried");
}

#[tokio::test]
async fn a_terminal_status_fails_without_publishing_a_file() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();

    let mut req = DownloadRequest::new(h.url("/status/404"), dir.path());
    req.filename = Some("gone.bin".into());

    let err = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, DownloadError::Probe(_)), "{err}");
    assert!(!dir.path().join("gone.bin").exists());
}

#[tokio::test]
async fn a_checksum_mismatch_never_publishes_the_file() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 1024 * 1024;

    let mut req = DownloadRequest::new(h.url(&format!("/plain/iota/{SIZE}")), dir.path());
    req.filename = Some("checked.bin".into());
    req.expected_sha256 = Some("0".repeat(64));

    let err = download(req, settings(dir.path()), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap_err();

    assert!(matches!(err, DownloadError::ChecksumMismatch { .. }), "{err}");
    assert!(
        !dir.path().join("checked.bin").exists(),
        "a file that failed its checksum must never be published under the final name"
    );
    assert!(
        dir.path().join("checked.bin.slpart").exists(),
        "the data should be kept as a partial for inspection"
    );
}

#[tokio::test]
async fn adaptive_concurrency_scales_up_under_a_per_connection_cap() {
    // Ground truth: the server caps each connection at 2 MB/s, so more connections genuinely
    // help. The governor must discover that on its own.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 48 * 1024 * 1024;

    let mut req = DownloadRequest::new(
        h.url(&format!("/throttle/kappa/{SIZE}?bps=2000000&per=conn")),
        dir.path(),
    );
    req.filename = Some("adaptive.bin".into());
    // max_conns stays None, so the governor is in charge.

    let mut s = settings(dir.path());
    s.max_conns_per_download = 16;

    let o = download(req, s, Arc::new(NullS), CancellationToken::new(), None).await.unwrap();

    assert_content(&o.path, "kappa", SIZE);
    assert!(
        o.peak_conns > 4,
        "governor never ramped past its starting point ({} conns) despite a per-connection cap",
        o.peak_conns
    );
}

#[tokio::test]
async fn filenames_from_the_server_cannot_escape_the_destination() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.join("downloads");
    std::fs::create_dir_all(&target).unwrap();

    // The server asks for a traversal path.
    let req = DownloadRequest::new(
        h.url("/plain/lambda/1024?name=..%2F..%2Fescaped.bin"),
        &target,
    );
    let o = download(req, settings(&target), Arc::new(NullS), CancellationToken::new(), None)
        .await
        .unwrap();

    assert!(o.path.starts_with(&target), "escaped to {}", o.path.display());
    assert!(!outside.join("escaped.bin").exists(), "traversal succeeded");
}
