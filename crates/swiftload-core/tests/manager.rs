//! Application-level behaviour: the queue, the budget, the event stream, and the lifecycle
//! commands the UI drives.
//!
//! These run against the real test server through the real engine. A manager test that stubbed
//! the download would prove only that the manager calls a function.

use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use swiftload_core::{
    config::Settings,
    events::{EngineEvent, Notice},
    manager::{AddRequest, Manager, RefreshOutcome},
    scheduler::Priority,
    store::{
        models::{DownloadStatus, UrlSource},
        Store,
    },
};
use swiftload_testserver as ts;

/// Enough to download quickly where the test only cares that it finished.
const SIZE: u64 = 8 * 1024 * 1024;
/// Progress is only observable at checkpoint granularity — 8 MiB, or 5 s, whichever comes
/// first. Any test that wants to interrupt a download *in the middle* therefore needs a file
/// several checkpoints long, or the first checkpoint it sees is also the last one.
const BIG: u64 = 24 * 1024 * 1024;
/// One checkpoint in. Waiting for this rather than for "any bytes" says what is actually
/// being waited for.
const ONE_CHECKPOINT: u64 = 8 * 1024 * 1024;

/// A shaped URL for the interrupt tests.
///
/// `per=total` rather than `per=conn`: with a per-connection cap the aggregate rate depends on
/// how far the governor has ramped, which makes "how long is this download" unpredictable and
/// the test flaky. A total cap gives a stable duration.
fn shaped(h: &ts::Handle, seed: &str, size: u64, bps: u64) -> String {
    h.url(&format!("/throttle/{seed}/{size}?bps={bps}&per=total"))
}

fn settings(dir: &Path) -> Settings {
    Settings {
        download_dir: dir.to_path_buf(),
        // Hashing an 8 MB file on every test adds nothing these tests are checking.
        hash_on_complete: false,
        apply_motw: false,
        ..Default::default()
    }
}

fn manager(dir: &Path, s: Settings) -> Arc<Manager> {
    let store = Arc::new(Store::open(&dir.join("swiftload.db")).unwrap());
    Manager::new(store, s)
}

fn add(url: &str, filename: &str) -> AddRequest {
    AddRequest {
        url: url.to_string(),
        dest_dir: None,
        filename: Some(filename.to_string()),
        category: None,
        max_conns: None,
        priority: Priority::Normal,
        start_now: false,
        expected_sha256: None,
    }
}

/// Every event the manager emitted, in order.
fn collect_events(m: &Arc<Manager>) -> Arc<Mutex<Vec<EngineEvent>>> {
    let log = Arc::new(Mutex::new(Vec::new()));
    let mut rx = m.subscribe();
    let sink = log.clone();
    tokio::spawn(async move {
        while let Ok(ev) = rx.recv().await {
            sink.lock().unwrap().push(ev);
        }
    });
    log
}

/// Poll until `pred` holds, or give up. Polling the store rather than waiting on an event
/// keeps these tests independent of event timing, which is what makes them stable under load.
async fn wait_until(
    m: &Arc<Manager>,
    id: &str,
    secs: u64,
    pred: impl Fn(DownloadStatus) -> bool,
) -> DownloadStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let st = m
            .list(None)
            .unwrap()
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| d.status);
        if let Some(st) = st {
            if pred(st) {
                return st;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting on {id}; last status {st:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_bytes(m: &Arc<Manager>, id: &str, at_least: u64, secs: u64) -> u64 {
    let deadline = std::time::Instant::now() + Duration::from_secs(secs);
    loop {
        let done = m
            .list(None)
            .unwrap()
            .into_iter()
            .find(|d| d.id == id)
            .map(|d| d.bytes_done)
            .unwrap_or(0);
        if done >= at_least {
            return done;
        }
        if std::time::Instant::now() > deadline {
            panic!("timed out waiting for {at_least} bytes on {id}; reached {done}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn assert_content(path: &Path, seed: &str, size: u64) {
    let actual = std::fs::read(path).expect("read downloaded file");
    assert_eq!(actual.len() as u64, size, "size mismatch");
    let expected = ts::content::chunk(ts::content::seed_of(seed), 0, size as usize);
    if actual != expected {
        let at = actual.iter().zip(&expected).position(|(a, b)| a != b);
        panic!("content mismatch at byte {at:?} of {size}");
    }
}

fn client() -> reqwest::Client {
    // The container routes through an HTTPS proxy; loopback must not go via it.
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn add_downloads_a_file_byte_exactly() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));

    let id = m
        .add(add(&h.url(&format!("/plain/alpha/{SIZE}")), "out.bin"))
        .await
        .unwrap();

    wait_until(&m, &id, 60, |s| s == DownloadStatus::Completed).await;
    assert_content(&dir.path().join("out.bin"), "alpha", SIZE);
    assert!(
        !dir.path().join("out.bin.slpart").exists(),
        "the part file should be gone once the download completes"
    );
}

#[tokio::test]
async fn a_completed_download_reports_its_path_in_a_notice() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));
    let log = collect_events(&m);

    let id = m
        .add(add(&h.url(&format!("/plain/beta/{SIZE}")), "out.bin"))
        .await
        .unwrap();
    wait_until(&m, &id, 60, |s| s == DownloadStatus::Completed).await;
    // The notice is emitted from the same task that sets the status, but the subscriber runs
    // on its own; give it a tick to drain.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let events = log.lock().unwrap();
    let completed = events.iter().find_map(|e| match e {
        EngineEvent::Notice(n @ Notice::Completed { .. }) => Some(n.clone()),
        _ => None,
    });
    match completed {
        Some(Notice::Completed { path, .. }) => {
            assert_eq!(path, dir.path().join("out.bin").to_string_lossy());
        }
        other => panic!("expected a completion notice, got {other:?}"),
    }
}

#[tokio::test]
async fn pause_keeps_progress_and_resume_finishes_byte_exactly() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));

    // Throttled so there is a middle to interrupt.
    let url = shaped(&h, "gamma", BIG, 8_000_000);
    let id = m.add(add(&url, "out.bin")).await.unwrap();

    // Wait for a real checkpoint rather than a wall-clock interval.
    let before = wait_for_bytes(&m, &id, ONE_CHECKPOINT, 60).await;
    m.pause(&id).unwrap();
    let status = wait_until(&m, &id, 30, |s| s == DownloadStatus::Paused).await;
    assert_eq!(status, DownloadStatus::Paused);

    let paused_at = m
        .list(None)
        .unwrap()
        .into_iter()
        .find(|d| d.id == id)
        .unwrap()
        .bytes_done;
    assert!(paused_at >= before, "pausing must not discard progress");
    assert!(
        paused_at < BIG,
        "the download finished before it could be paused ({paused_at} of {BIG})"
    );
    assert!(
        dir.path().join("out.bin.slpart").exists(),
        "pausing keeps the partial file"
    );

    m.resume(&id).unwrap();
    wait_until(&m, &id, 90, |s| s == DownloadStatus::Completed).await;
    assert_content(&dir.path().join("out.bin"), "gamma", BIG);
}

#[tokio::test]
async fn no_more_than_the_configured_number_of_downloads_run_at_once() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(
        dir.path(),
        Settings {
            max_concurrent_downloads: 2,
            ..settings(dir.path())
        },
    );

    let mut ids = Vec::new();
    for i in 0..4 {
        let url = h.url(&format!("/throttle/s{i}/{SIZE}?bps=4000000&per=conn"));
        ids.push(m.add(add(&url, &format!("out{i}.bin"))).await.unwrap());
    }

    // Sample the running count while the batch is in flight.
    let mut peak = 0usize;
    let deadline = std::time::Instant::now() + Duration::from_secs(120);
    loop {
        let all = m.list(None).unwrap();
        let active = all
            .iter()
            .filter(|d| d.status == DownloadStatus::Active)
            .count();
        peak = peak.max(active);
        if all.iter().all(|d| d.status == DownloadStatus::Completed) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "downloads did not all finish: {:?}",
            all.iter()
                .map(|d| (&d.filename, d.status))
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        peak <= 2,
        "ran {peak} downloads at once with a limit of 2 — the queue is not holding"
    );
    assert!(peak >= 1);
    // Every queued download eventually ran: the limit throttles, it does not strand.
    for (i, id) in ids.iter().enumerate() {
        let d = m
            .list(None)
            .unwrap()
            .into_iter()
            .find(|d| &d.id == id)
            .unwrap();
        assert_eq!(d.status, DownloadStatus::Completed);
        assert_content(
            &dir.path().join(format!("out{i}.bin")),
            &format!("s{i}"),
            SIZE,
        );
    }
}

#[tokio::test]
async fn progress_arrives_as_one_batched_event_covering_every_download() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(
        dir.path(),
        Settings {
            max_concurrent_downloads: 2,
            ..settings(dir.path())
        },
    );
    let log = collect_events(&m);

    for i in 0..2 {
        let url = shaped(&h, &format!("p{i}"), BIG, 6_000_000);
        m.add(add(&url, &format!("out{i}.bin"))).await.unwrap();
    }

    // Let a few ticks go by with both downloads running.
    tokio::time::sleep(Duration::from_millis(1200)).await;

    let events = log.lock().unwrap().clone();
    let batches: Vec<usize> = events
        .iter()
        .filter_map(|e| match e {
            EngineEvent::Progress { downloads } => Some(downloads.len()),
            _ => None,
        })
        .collect();

    assert!(!batches.is_empty(), "no progress events were emitted");
    assert!(
        batches.contains(&2),
        "two downloads were running but no single event carried both: {batches:?}"
    );
}

#[tokio::test]
async fn an_idle_manager_emits_no_progress_events() {
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));
    let log = collect_events(&m);

    tokio::time::sleep(Duration::from_millis(1200)).await;

    let progress = log
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, EngineEvent::Progress { .. }))
        .count();
    assert_eq!(
        progress, 0,
        "an app with nothing running must not emit progress four times a second"
    );
}

#[tokio::test]
async fn connection_tables_go_only_to_subscribers() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));
    let log = collect_events(&m);

    let url = shaped(&h, "conn", BIG, 4_000_000);
    let id = m.add(add(&url, "out.bin")).await.unwrap();

    tokio::time::sleep(Duration::from_millis(800)).await;
    let before = log
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, EngineEvent::Connections(_)))
        .count();
    assert_eq!(
        before, 0,
        "connection tables were sent with nobody watching"
    );

    m.subscribe_connections(&id);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let during = log
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, EngineEvent::Connections(_)))
        .count();
    assert!(during > 0, "a subscriber received no connection tables");

    m.unsubscribe_connections(&id);
    let at_unsubscribe = log
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, EngineEvent::Connections(_)))
        .count();
    tokio::time::sleep(Duration::from_millis(800)).await;
    let after = log
        .lock()
        .unwrap()
        .iter()
        .filter(|e| matches!(e, EngineEvent::Connections(_)))
        .count();
    assert_eq!(
        after, at_unsubscribe,
        "connection tables kept coming after unsubscribing"
    );
}

#[tokio::test]
async fn cancel_keeps_the_partial_and_remove_deletes_it_only_when_asked() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));

    let url = shaped(&h, "keep", BIG, 8_000_000);
    let id = m.add(add(&url, "keep.bin")).await.unwrap();
    wait_for_bytes(&m, &id, ONE_CHECKPOINT, 60).await;

    m.cancel(&id).unwrap();
    wait_until(&m, &id, 30, |s| s == DownloadStatus::Cancelled).await;
    assert!(
        dir.path().join("keep.bin.slpart").exists(),
        "cancel must not delete the partial file"
    );

    m.remove(&id, false).unwrap();
    assert!(
        m.list(None).unwrap().iter().all(|d| d.id != id),
        "the record should be gone"
    );
    assert!(
        dir.path().join("keep.bin.slpart").exists(),
        "remove without delete_file keeps what was downloaded"
    );

    // And again, this time asking for the bytes to go too.
    let id2 = m.add(add(&url, "gone.bin")).await.unwrap();
    wait_for_bytes(&m, &id2, ONE_CHECKPOINT, 60).await;
    m.remove(&id2, true).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while dir.path().join("gone.bin.slpart").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "partial file was not deleted"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn a_download_interrupted_by_a_crash_comes_back_paused_not_failed() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("swiftload.db");

    // Simulate the previous run dying with a download still marked Active.
    {
        let store = Store::open(&db).unwrap();
        let mut rec = swiftload_core::store::models::DownloadRecord::new(
            "http://example.invalid/f.bin",
            "f.bin",
            &dir.path().to_string_lossy(),
            &dir.path().join("f.bin.slpart").to_string_lossy(),
        );
        rec.status = DownloadStatus::Active;
        store.insert(&rec).unwrap();
        store
            .set_status(&rec.id, DownloadStatus::Active, None)
            .unwrap();
    }

    let m = Manager::open(&db).unwrap();
    let all = m.list(None).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(
        all[0].status,
        DownloadStatus::Paused,
        "an interrupted download is resumable, not failed"
    );
}

#[tokio::test]
async fn settings_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("swiftload.db");

    {
        let m = Manager::open(&db).unwrap();
        let mut s = m.settings();
        s.max_concurrent_downloads = 7;
        s.max_conns_per_download = 3;
        m.set_settings(s).unwrap();
    }

    let m = Manager::open(&db).unwrap();
    assert_eq!(m.settings().max_concurrent_downloads, 7);
    assert_eq!(m.settings().max_conns_per_download, 3);
}

#[tokio::test]
async fn a_probe_preview_offers_an_existing_partial_rather_than_a_second_copy() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));

    let url = shaped(&h, "dup", BIG, 8_000_000);
    let id = m.add(add(&url, "dup.bin")).await.unwrap();
    wait_for_bytes(&m, &id, ONE_CHECKPOINT, 60).await;
    m.pause(&id).unwrap();
    wait_until(&m, &id, 30, |s| s == DownloadStatus::Paused).await;

    let preview = m.probe_url(&url).await.unwrap();
    assert_eq!(
        preview.existing.len(),
        1,
        "the incomplete download should have been offered"
    );
    assert_eq!(preview.existing[0].id, id);
}

/// The §D.10 flow driven entirely through the manager: a signed link dies mid-download, the
/// user supplies a fresh one, and the download resumes rather than restarting.
#[tokio::test]
async fn an_expired_link_needs_attention_and_a_fresh_one_resumes_it() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));
    let c = client();

    let signed = c
        .get(h.url(&format!("/mint/movie?n={BIG}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let throttled = format!("{signed}&bps=8000000&per=total");

    let id = m.add(add(&throttled, "movie.mkv")).await.unwrap();
    let progressed = wait_for_bytes(&m, &id, ONE_CHECKPOINT, 60).await;
    m.pause(&id).unwrap();
    wait_until(&m, &id, 30, |s| s == DownloadStatus::Paused).await;
    assert!(progressed < BIG, "finished before it could be interrupted");

    // The link dies while the download is paused.
    c.get(h.url("/expire/movie")).send().await.unwrap();

    // A fresh link for the same file.
    let fresh = c
        .get(h.url(&format!("/mint/movie?n={BIG}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_ne!(fresh, signed);

    let report = m.validate_replacement_url(&id, &fresh).await.unwrap();
    assert_eq!(
        report.outcome,
        RefreshOutcome::Verified,
        "same file, so it should verify without asking: {}",
        report.message
    );
    assert!(
        report.bytes_verified < 1024 * 1024,
        "proving identity cost {} bytes; the point is to avoid re-transferring",
        report.bytes_verified
    );
    assert_eq!(report.preserved_bytes, progressed_ranges(&m, &id));

    m.commit_replacement_url(&id, &fresh, UrlSource::User, false)
        .await
        .unwrap();
    wait_until(&m, &id, 90, |s| s == DownloadStatus::Completed).await;
    assert_content(&dir.path().join("movie.mkv"), "movie", BIG);

    // The history keeps both links, and neither in a form that could be replayed. Parameter
    // *names* are deliberately preserved — they are what makes a history entry legible — so
    // what must be absent is the token's value.
    let history = m.url_history(&id).unwrap();
    assert!(history.len() >= 2, "the swap should be recorded");
    for secret in [token_of(&signed), token_of(&fresh)] {
        assert!(!secret.is_empty(), "the test server minted no token");
        for entry in &history {
            assert!(
                !entry.url_redacted.contains(&secret),
                "a signed token reached the stored history: {}",
                entry.url_redacted
            );
        }
    }
    assert!(
        history.iter().any(|e| e.url_redacted.contains("token=")),
        "the parameter name should survive so the entry stays readable"
    );
}

/// The value of the `token` query parameter, which is the part that must never be stored.
fn token_of(url: &str) -> String {
    url.split_once("token=")
        .map(|(_, rest)| rest.split('&').next().unwrap_or("").to_string())
        .unwrap_or_default()
}

/// A replacement that is demonstrably a different file is refused, and refusing it changes
/// nothing on disk.
#[tokio::test]
async fn a_replacement_of_the_wrong_size_is_rejected() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let m = manager(dir.path(), settings(dir.path()));

    let url = shaped(&h, "orig", BIG, 8_000_000);
    let id = m.add(add(&url, "orig.bin")).await.unwrap();
    wait_for_bytes(&m, &id, ONE_CHECKPOINT, 60).await;
    m.pause(&id).unwrap();
    wait_until(&m, &id, 30, |s| s == DownloadStatus::Paused).await;
    let before = progressed_ranges(&m, &id);

    let wrong = h.url(&format!("/plain/orig/{}", BIG * 2));
    let report = m.validate_replacement_url(&id, &wrong).await.unwrap();
    assert_eq!(report.outcome, RefreshOutcome::Reject);

    assert!(
        m.commit_replacement_url(&id, &wrong, UrlSource::User, true)
            .await
            .is_err(),
        "a rejected replacement must not be committable, even with confirmation"
    );
    assert_eq!(
        progressed_ranges(&m, &id),
        before,
        "a rejected swap must not touch what has been downloaded"
    );
}

fn progressed_ranges(m: &Arc<Manager>, id: &str) -> u64 {
    m.store().get(id).unwrap().unwrap().completed_ranges.total()
}
