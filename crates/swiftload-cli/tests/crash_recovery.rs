//! Crash recovery and URL refresh, driven through the real binary.
//!
//! These tests kill the process outright rather than cancelling politely, because a graceful
//! shutdown exercises the flush path while `SIGKILL` exercises the one that actually matters:
//! whatever the checkpoint claims must be genuinely on disk, since nothing gets a chance to
//! tidy up afterwards.

use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use swiftload_testserver as ts;

const BIN: &str = env!("CARGO_BIN_EXE_swiftload");

fn run(args: &[&str], cwd: &Path) -> std::process::Output {
    Command::new(BIN)
        .args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("failed to run swiftload")
}

fn first_id(cwd: &Path, db: &str) -> String {
    let out = run(&["--db", db, "list", "--json"], cwd);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("list --json");
    v[0]["id"].as_str().expect("an id").to_string()
}

fn assert_content(path: &Path, seed: &str, size: u64) {
    let actual = std::fs::read(path).expect("read result");
    assert_eq!(
        actual.len() as u64,
        size,
        "size mismatch for {}",
        path.display()
    );
    let expected = ts::content::chunk(ts::content::seed_of(seed), 0, size as usize);
    if actual != expected {
        let at = actual.iter().zip(&expected).position(|(a, b)| a != b);
        panic!(
            "content mismatch at byte {at:?} of {size} in {}",
            path.display()
        );
    }
}

/// Start a download, kill -9 partway, then resume. The final file must be byte-exact.
async fn kill_and_resume(kill_after: Duration, conns: &str, seed: &str) -> (u64, u64) {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 24 * 1024 * 1024;

    let url = h.url(&format!("/throttle/{seed}/{SIZE}?bps=1500000&per=conn"));
    let mut child = Command::new(BIN)
        .args([
            "--db", "s.db", "get", &url, "--out", ".", "--name", "big.bin", "--conns", conns,
        ])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn download");

    tokio::time::sleep(kill_after).await;
    // SIGKILL: no flush, no checkpoint, no cleanup. Whatever the database already claims must
    // be true on disk, or the resumed file will be wrong.
    let _ = child.kill();
    let _ = child.wait();

    let dirp: PathBuf = dir.path().into();
    let id = first_id(&dirp, "s.db");

    let out = run(&["--db", "s.db", "resume", &id, "--json"], &dirp);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|_| {
        panic!(
            "resume did not produce JSON: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(v["ok"], true, "resume failed: {v}");

    assert_content(&dir.path().join("big.bin"), seed, SIZE);
    let resumed = v["resumed_from"].as_u64().unwrap();
    let bytes = v["bytes"].as_u64().unwrap();
    assert_eq!(bytes, SIZE);
    (resumed, bytes)
}

#[tokio::test(flavor = "multi_thread")]
async fn survives_sigkill_at_several_points_and_stays_byte_exact() {
    // Different kill points land mid-chunk, mid-checkpoint and between segments.
    let mut resumed_any = false;
    for (i, ms) in [900u64, 1600, 2400, 3200].into_iter().enumerate() {
        let (resumed, _) =
            kill_and_resume(Duration::from_millis(ms), "4", &format!("crash{i}")).await;
        if resumed > 0 {
            resumed_any = true;
        }
    }
    assert!(
        resumed_any,
        "every run restarted from zero — recovery is not actually working"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_single_connection_download_also_recovers() {
    // Killed after a checkpoint is certain to have happened. Checkpoints fire every 8 MiB or
    // 5 s, whichever comes first, so at 1.5 MB/s on one connection the timer wins at ~7.5 MB.
    // That bound is the deliberate trade: work lost to a crash is capped at five seconds, paid
    // for with one fsync per interval rather than one per chunk.
    let (resumed, bytes) = kill_and_resume(Duration::from_millis(7000), "1", "crashsingle").await;
    assert!(
        resumed > 0,
        "single-connection download restarted from zero"
    );
    assert!(resumed < bytes);
}

#[tokio::test(flavor = "multi_thread")]
async fn recovery_never_restarts_from_zero_when_progress_was_checkpointed() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    const SIZE: u64 = 24 * 1024 * 1024;

    let url = h.url(&format!("/throttle/keep/{SIZE}?bps=1500000&per=conn"));
    let mut child = Command::new(BIN)
        .args([
            "--db", "s.db", "get", &url, "--out", ".", "--name", "keep.bin", "--conns", "4",
        ])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    // Long enough that several checkpoints have certainly been written.
    tokio::time::sleep(Duration::from_millis(3500)).await;
    let _ = child.kill();
    let _ = child.wait();

    let dirp: PathBuf = dir.path().into();
    let before = h.stats().bytes_served;
    let id = first_id(&dirp, "s.db");

    let out = run(&["--db", "s.db", "resume", &id, "--json"], &dirp);
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "{v}");

    let resumed = v["resumed_from"].as_u64().unwrap();
    assert!(
        resumed > 0,
        "restarted from zero despite checkpointed progress"
    );

    let transferred = h.stats().bytes_served - before;
    assert!(
        transferred < SIZE,
        "resume transferred {transferred} of a {SIZE}-byte file — it effectively restarted"
    );
    assert_content(&dir.path().join("keep.bin"), "keep", SIZE);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_expired_link_is_refreshed_and_the_download_finishes() {
    // The 10 GB / 6.5 GB scenario in miniature, through the real binary.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dirp: PathBuf = dir.path().into();
    const SIZE: u64 = 24 * 1024 * 1024;

    let c = reqwest::Client::builder().no_proxy().build().unwrap();
    let signed = c
        .get(h.url(&format!("/mint/refresh?n={SIZE}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let throttled = format!("{signed}&bps=1500000&per=conn");

    let mut child = Command::new(BIN)
        .args([
            "--db",
            "s.db",
            "get",
            &throttled,
            "--out",
            ".",
            "--name",
            "movie.mkv",
            "--conns",
            "4",
        ])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let _ = child.kill();
    let _ = child.wait();

    let id = first_id(&dirp, "s.db");

    // The link expires while the download is stopped.
    c.get(h.url("/expire/refresh")).send().await.unwrap();
    let stale = run(&["--db", "s.db", "resume", &id, "--json"], &dirp);
    let v: serde_json::Value = serde_json::from_slice(&stale.stdout).unwrap();
    assert_eq!(
        v["ok"], false,
        "resuming an expired link should fail, not silently succeed"
    );

    // The partial must still be there — losing it is the failure this feature prevents.
    assert!(
        dir.path().join("movie.mkv.slpart").exists(),
        "partial file was discarded"
    );

    // The user fetches a fresh link for the same file.
    let fresh = c
        .get(h.url(&format!("/mint/refresh?n={SIZE}")))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert_ne!(fresh, signed);

    let before = h.stats().bytes_served;
    let out = run(
        &["--db", "s.db", "refresh-url", &id, &fresh, "--json"],
        &dirp,
    );
    let stdout = String::from_utf8_lossy(&out.stdout);

    // Two JSON documents are printed: the validation report, then the download result.
    let mut docs = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok());
    let report = docs.next().unwrap_or_else(|| {
        panic!(
            "no validation report.\nstdout: {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(
        report["safe"], true,
        "the same file should validate cleanly: {report}"
    );
    assert!(
        report["bytes_verified"].as_u64().unwrap() <= 1024 * 1024,
        "validation exceeded its 1 MB budget: {report}"
    );

    let result = docs
        .next()
        .unwrap_or_else(|| panic!("no download result: {stdout}"));
    assert_eq!(result["ok"], true, "refresh+resume failed: {stdout}");
    assert!(
        result["resumed_from"].as_u64().unwrap() > 0,
        "the refresh threw away progress: {result}"
    );

    assert_content(&dir.path().join("movie.mkv"), "refresh", SIZE);

    let transferred = h.stats().bytes_served - before;
    assert!(
        transferred < SIZE,
        "refresh transferred {transferred} of {SIZE} — it restarted instead of resuming"
    );

    // And the recorded link history must not contain the signed token.
    let links = run(&["--db", "s.db", "links", &id], &dirp);
    let text = String::from_utf8_lossy(&links.stdout);
    let token = signed
        .split("token=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    assert!(
        !text.contains(token),
        "signed token leaked into stored history:\n{text}"
    );
    assert!(
        text.contains("token="),
        "parameter names should survive redaction:\n{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refreshing_onto_a_different_file_is_refused() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dirp: PathBuf = dir.path().into();
    const SIZE: u64 = 24 * 1024 * 1024;

    let url = h.url(&format!("/throttle/genuine/{SIZE}?bps=1500000&per=conn"));
    let mut child = Command::new(BIN)
        .args([
            "--db",
            "s.db",
            "get",
            &url,
            "--out",
            ".",
            "--name",
            "movie.mkv",
            "--conns",
            "4",
        ])
        .current_dir(dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let _ = child.kill();
    let _ = child.wait();

    let id = first_id(&dirp, "s.db");
    let before_bytes = std::fs::metadata(dir.path().join("movie.mkv.slpart"))
        .unwrap()
        .len();

    // Same size, same name, entirely different content.
    let decoy = h.url(&format!("/decoy/imposter/{SIZE}"));
    let out = run(
        &[
            "--db",
            "s.db",
            "refresh-url",
            &id,
            &decoy,
            "--yes",
            "--json",
        ],
        &dirp,
    );

    assert!(!out.status.success(), "a decoy link was accepted");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("Reject"), "expected a rejection, got: {text}");

    // The partial must be untouched: a refusal must not cost the user their download.
    assert_eq!(
        std::fs::metadata(dir.path().join("movie.mkv.slpart"))
            .unwrap()
            .len(),
        before_bytes,
        "the partial file was modified by a rejected refresh"
    );
    assert!(
        !dir.path().join("movie.mkv").exists(),
        "a rejected refresh must not publish a file"
    );
}
