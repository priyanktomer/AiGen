//! Download orchestration: probe, plan, workers, governor, writer.

pub mod governor;
pub mod identity;
pub mod plan;
pub mod probe;
pub mod worker;
pub mod writer;

use crate::{
    config::{GovernorConfig, RequestSpec, Settings, MIN_SEGMENT, SMALL_FILE_THRESHOLD},
    fsx,
    http::errors::ErrorClass,
    task::{
        governor::{Decision, Governor, Sample, StopReason},
        plan::Plan,
        probe::{probe, ProbeResult},
        worker::{WorkerCtx, WorkerExit, WorkerMetrics},
        writer::{CheckpointSink, WriterHandle},
    },
    util::{intervals::RangeSet, rate::SpeedMeter},
};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct DownloadRequest {
    pub url: String,
    pub dest_dir: PathBuf,
    /// Override the server-supplied name.
    pub filename: Option<String>,
    /// `None` lets the governor decide.
    pub max_conns: Option<usize>,
    pub spec: RequestSpec,
    /// Resume state from a previous session, if any.
    pub completed: RangeSet,
    /// Verify the finished file against this, if the user supplied one.
    pub expected_sha256: Option<String>,
}

impl DownloadRequest {
    pub fn new(url: impl Into<String>, dest_dir: impl Into<PathBuf>) -> Self {
        Self {
            url: url.into(),
            dest_dir: dest_dir.into(),
            filename: None,
            max_conns: None,
            spec: RequestSpec::default(),
            completed: RangeSet::new(),
            expected_sha256: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub elapsed: Duration,
    pub peak_conns: usize,
    pub final_conns: usize,
    pub avg_bps: u64,
    pub peak_bps: u64,
    pub retries: u64,
    pub requests: u64,
    pub stop_reason: StopReason,
    pub saturation_detected: bool,
    pub resumed_from: u64,
    pub sha256: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("probe failed: {0}")]
    Probe(#[from] probe::ProbeError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("download failed: {0:?}")]
    Failed(ErrorClass),
    #[error("incomplete: {done} of {total} bytes, {gaps} gap(s) remain")]
    Incomplete { done: u64, total: u64, gaps: usize },
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("cancelled")]
    Cancelled,
}

/// Snapshot for the UI or CLI. Cheap to produce; published a few times a second, never
/// per chunk.
#[derive(Debug, Clone)]
pub struct Progress {
    pub bytes_done: u64,
    pub total: Option<u64>,
    pub current_bps: u64,
    pub avg_bps: u64,
    pub peak_bps: u64,
    pub eta: Option<Duration>,
    pub conns: usize,
    pub retries: u64,
    pub connections: Vec<ConnectionInfo>,
}

#[derive(Debug, Clone)]
pub struct ConnectionInfo {
    pub id: usize,
    pub bytes: u64,
    pub bps: u64,
    pub retries: u64,
    pub claim: Option<(u64, u64)>,
    pub state: worker::WorkerState,
}

/// Shared live state, read by the progress publisher and the governor.
struct Live {
    initial_bytes: u64,
    fetched: Arc<AtomicU64>,
    conns: AtomicUsize,
    peak_conns: AtomicUsize,
    workers: Mutex<Vec<Arc<WorkerMetrics>>>,
    meter: Mutex<SpeedMeter>,
    errors: Arc<AtomicU64>,
}

impl Live {
    fn bytes_done(&self) -> u64 {
        self.initial_bytes + self.fetched.load(Ordering::Relaxed)
    }
}

/// Choose the starting connection count.
///
/// Starting at 1 wastes the first seconds of every download relearning what is usually
/// already known; starting at 16 is antisocial and often trips throttling before anything has
/// been measured. Four is nearly always safe and nearly always helps when parallelism helps.
pub fn initial_conns(pr: &ProbeResult, settings: &Settings, override_k: Option<usize>) -> usize {
    if !pr.is_resumable() {
        return 1; // cannot segment without working ranges
    }
    let Some(size) = pr.total_size else {
        return 1; // cannot plan without a size
    };
    if size < SMALL_FILE_THRESHOLD {
        return 1; // handshakes cost more than they save
    }

    let by_size = (size / MIN_SEGMENT).max(1) as usize;
    let ceiling = override_k.unwrap_or(settings.max_conns_per_download);
    let base = if let Some(k) = override_k {
        k
    } else if size < 32 * 1024 * 1024 {
        2
    } else {
        4
    };
    base.min(by_size)
        .min(ceiling)
        .min(settings.max_total_conns)
        .max(1)
}

/// Run a download to completion.
pub async fn download(
    req: DownloadRequest,
    settings: Settings,
    sink: Arc<dyn CheckpointSink>,
    cancel: CancellationToken,
    on_progress: Option<Box<dyn Fn(Progress) + Send + Sync>>,
) -> Result<DownloadOutcome, DownloadError> {
    let started = Instant::now();
    let pr = probe(&req.url, &settings, &req.spec, false).await?;

    let filename = req.filename.clone().unwrap_or_else(|| pr.filename.clone());
    let dest =
        crate::util::filename::resolve_destination(&req.dest_dir, &filename).map_err(|e| {
            DownloadError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                e.to_string(),
            ))
        })?;
    let part = fsx::part_path(
        &req.dest_dir,
        dest.file_name().unwrap().to_string_lossy().as_ref(),
    );

    if let Some(total) = pr.total_size {
        // Fail now rather than at 90%.
        fsx::ensure_space(&req.dest_dir, total.saturating_sub(req.completed.total()))?;
    }

    // Whether the part file already existed must be decided *before* opening it, because
    // opening preallocates to the full size — after which the file's length says nothing about
    // how much was actually written.
    let part_existed = part.exists();
    let file = fsx::open_part(&part, pr.total_size)?;

    // Reconcile the checkpoint against reality. A checkpoint can outlive its data: the user
    // deletes the partial file, or it is truncated externally. Trust the file, never the record.
    let mut completed = req.completed.clone();
    if !part_existed && !completed.is_empty() {
        // The record claims progress for a file that is not there. Believing it would leave
        // the "downloaded" region as preallocated zeros — a right-sized, wrong-content file.
        tracing::warn!(
            claimed = completed.total(),
            "checkpoint refers to a missing partial file; restarting from zero"
        );
        completed = RangeSet::new();
    } else if pr.total_size.is_none() {
        // Without preallocation the file length is meaningful, so a short file means the tail
        // of the checkpoint never made it to disk.
        completed.truncate_to(file.metadata()?.len());
    }
    let resumed_from = completed.total();

    let plan = Arc::new(Plan::new(pr.total_size, &completed));
    let (writer, writer_join) = writer::spawn(file, completed, sink);

    let k0 = initial_conns(&pr, &settings, req.max_conns);
    plan.seed(k0);

    let live = Arc::new(Live {
        initial_bytes: resumed_from,
        fetched: Arc::new(AtomicU64::new(0)),
        conns: AtomicUsize::new(0),
        peak_conns: AtomicUsize::new(0),
        workers: Mutex::new(Vec::new()),
        meter: Mutex::new(SpeedMeter::new()),
        errors: Arc::new(AtomicU64::new(0)),
    });

    let gov_cfg = GovernorConfig {
        max_conns: req
            .max_conns
            .unwrap_or(settings.max_conns_per_download)
            .min(32),
        ..Default::default()
    };
    // A fixed connection count was requested explicitly, so do not adapt away from it.
    let adaptive = settings.adaptive_concurrency && req.max_conns.is_none();

    let outcome = drive(
        &pr,
        &settings,
        &req,
        plan.clone(),
        writer.clone(),
        live.clone(),
        gov_cfg,
        adaptive,
        k0,
        cancel.clone(),
        on_progress,
    )
    .await;

    // Always flush and record, even on failure: the partial file must stay resumable.
    let final_ranges = writer.finish().await?;
    drop(writer);
    let _ = writer_join.await;

    outcome?;

    if cancel.is_cancelled() {
        return Err(DownloadError::Cancelled);
    }

    // ── Completion verification ──
    //
    // Check *coverage*, not just the byte counter. A counter can be correct while the
    // coverage has a hole, and that is precisely the bug that produces a right-sized,
    // wrong-content file.
    if let Some(total) = pr.total_size {
        if !final_ranges.contains_all(0, total) {
            return Err(DownloadError::Incomplete {
                done: final_ranges.total(),
                total,
                gaps: final_ranges.gaps_in(0, total).len(),
            });
        }
    }

    let bytes = final_ranges.total();
    let sha256 = if settings.hash_on_complete || req.expected_sha256.is_some() {
        Some(hash_file(&part).await?)
    } else {
        None
    };

    if let (Some(expected), Some(actual)) = (&req.expected_sha256, &sha256) {
        if !expected.eq_ignore_ascii_case(actual) {
            // Never publish a file that failed its checksum under the final name.
            return Err(DownloadError::ChecksumMismatch {
                expected: expected.clone(),
                actual: actual.clone(),
            });
        }
    }

    let final_path = unique_destination(&dest, &settings);
    fsx::finalize(
        &part,
        &final_path,
        settings.collision_policy == crate::config::CollisionPolicy::Overwrite,
    )?;

    if settings.apply_motw {
        // Best-effort: failing to mark the zone must not fail the download.
        let _ = fsx::motw::apply(&final_path, pr.final_url.as_str(), None);
    }

    let elapsed = started.elapsed();
    let (retries, requests) = {
        let ws = live.workers.lock().unwrap();
        (
            ws.iter()
                .map(|w| w.retries.load(Ordering::Relaxed))
                .sum::<u64>(),
            ws.iter()
                .map(|w| w.requests.load(Ordering::Relaxed))
                .sum::<u64>(),
        )
    };
    // Average is transferred bytes over wall time. Bytes carried over from a previous session
    // are excluded, because "how fast was this download" means this run, not history.
    let transferred = bytes.saturating_sub(resumed_from);
    let avg_bps = if elapsed.as_secs_f64() > 0.0 {
        (transferred as f64 / elapsed.as_secs_f64()) as u64
    } else {
        0
    };
    let peak_bps = live.meter.lock().unwrap().peak_bps().max(avg_bps);
    Ok(DownloadOutcome {
        path: final_path,
        bytes,
        elapsed,
        peak_conns: live.peak_conns.load(Ordering::Relaxed),
        final_conns: live.conns.load(Ordering::Relaxed),
        avg_bps,
        peak_bps,
        retries,
        requests,
        stop_reason: StopReason::StillRamping,
        saturation_detected: false,
        resumed_from,
        sha256,
    })
}

fn unique_destination(dest: &std::path::Path, settings: &Settings) -> PathBuf {
    match settings.collision_policy {
        crate::config::CollisionPolicy::Rename => {
            let dir = dest.parent().unwrap_or(std::path::Path::new("."));
            let name = dest.file_name().unwrap().to_string_lossy();
            dir.join(crate::util::filename::dedupe(dir, &name))
        }
        _ => dest.to_path_buf(),
    }
}

/// Spawn workers, run the governor, and wait for the work to drain.
#[allow(clippy::too_many_arguments)]
async fn drive(
    pr: &ProbeResult,
    settings: &Settings,
    req: &DownloadRequest,
    plan: Arc<Plan>,
    writer: WriterHandle,
    live: Arc<Live>,
    gov_cfg: GovernorConfig,
    adaptive: bool,
    k0: usize,
    cancel: CancellationToken,
    on_progress: Option<Box<dyn Fn(Progress) + Send + Sync>>,
) -> Result<(), DownloadError> {
    let mut gov = Governor::new(gov_cfg, k0, Instant::now());
    let mut set: tokio::task::JoinSet<WorkerExit> = tokio::task::JoinSet::new();
    let mut retire_tokens: Vec<CancellationToken> = Vec::new();
    let mut next_id = 0usize;
    let mut fatal: Option<ErrorClass> = None;

    let mut spawn_worker = |set: &mut tokio::task::JoinSet<WorkerExit>,
                            retire_tokens: &mut Vec<CancellationToken>| {
        let metrics = WorkerMetrics::new(next_id);
        next_id += 1;
        let retire = CancellationToken::new();
        retire_tokens.push(retire.clone());
        live.workers.lock().unwrap().push(metrics.clone());

        let fetched = live.fetched.clone();
        let ctx = WorkerCtx {
            url: pr.final_url.clone(),
            settings: settings.clone(),
            spec: req.spec.clone(),
            plan: plan.clone(),
            writer: writer.clone(),
            range_support: pr.range_support,
            total: pr.total_size,
            cancel: cancel.clone(),
            retire,
            metrics: metrics.clone(),
            errors: live.errors.clone(),
        };
        set.spawn(async move {
            let before = metrics.bytes.load(Ordering::Relaxed);
            let exit = worker::run(ctx).await;
            let _ = before;
            let _ = fetched;
            exit
        });
        live.conns.fetch_add(1, Ordering::Relaxed);
        live.peak_conns
            .fetch_max(live.conns.load(Ordering::Relaxed), Ordering::Relaxed);
    };

    for _ in 0..k0 {
        spawn_worker(&mut set, &mut retire_tokens);
    }

    let mut ticker = tokio::time::interval(Duration::from_millis(250));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_errors = 0u64;

    loop {
        tokio::select! {
            joined = set.join_next(), if !set.is_empty() => {
                match joined {
                    Some(Ok(exit)) => {
                        live.conns.fetch_sub(1, Ordering::Relaxed);
                        match exit {
                            WorkerExit::Failed(class) => {
                                if fatal.is_none() { fatal = Some(class); }
                                cancel.cancel();
                            }
                            WorkerExit::Cancelled => {}
                            WorkerExit::Exhausted | WorkerExit::Retired => {}
                        }
                    }
                    Some(Err(_join_err)) => {
                        live.conns.fetch_sub(1, Ordering::Relaxed);
                    }
                    None => {}
                }
                if set.is_empty() { break; }
            }
            _ = ticker.tick() => {
                // Aggregate what the workers have moved.
                let (fetched, retries) = {
                    let ws = live.workers.lock().unwrap();
                    (
                        ws.iter().map(|w| w.bytes.load(Ordering::Relaxed)).sum::<u64>(),
                        ws.iter().map(|w| w.retries.load(Ordering::Relaxed)).sum::<u64>(),
                    )
                };
                let now = Instant::now();
                let prev = live.fetched.swap(fetched, Ordering::Relaxed);
                {
                    let mut m = live.meter.lock().unwrap();
                    m.record(fetched.saturating_sub(prev), now);
                    if fetched == prev { m.tick(now); }
                }
                for w in live.workers.lock().unwrap().iter() { w.tick(now); }

                if let Some(cb) = &on_progress {
                    cb(snapshot(&live, pr.total_size, retries));
                }

                if cancel.is_cancelled() { break; }

                if adaptive {
                    let errors = live.errors.load(Ordering::Relaxed);
                    let sample = Sample {
                        now,
                        bytes_total: live.bytes_done(),
                        conns: live.conns.load(Ordering::Relaxed),
                        errors_since_last: (errors - last_errors) as u32,
                        rate_limited: None,
                        disk_backpressure: writer.is_backpressured(),
                        remaining_bytes: plan.outstanding(),
                        probe_token: true,
                    };
                    last_errors = errors;

                    match gov.observe(&sample) {
                        Decision::SpawnTo(k) => {
                            let live_now = live.conns.load(Ordering::Relaxed);
                            // Only spawn what the remaining work can actually occupy.
                            let useful = plan.useful_workers();
                            for _ in live_now..k.min(useful) {
                                spawn_worker(&mut set, &mut retire_tokens);
                            }
                        }
                        Decision::RetireTo(k) | Decision::ReleaseToken(k) => {
                            retire_down_to(&mut retire_tokens, live.conns.load(Ordering::Relaxed), k);
                        }
                        Decision::BackOff { conns, .. } => {
                            retire_down_to(&mut retire_tokens, live.conns.load(Ordering::Relaxed), conns);
                        }
                        Decision::Hold => {}
                    }

                    // Endgame: a swarm fighting over the last few hundred KB just adds tail
                    // latency.
                    if let Some(target) = gov.endgame_target(plan.outstanding()) {
                        retire_down_to(&mut retire_tokens, live.conns.load(Ordering::Relaxed), target);
                    }
                }
            }
        }
        if set.is_empty() {
            break;
        }
    }

    // Final aggregation. Without this, a download that completes inside a single tick
    // interval never records a sample, and reports 0 B/s despite having transferred the file.
    {
        let fetched = live
            .workers
            .lock()
            .unwrap()
            .iter()
            .map(|w| w.bytes.load(Ordering::Relaxed))
            .sum::<u64>();
        let prev = live.fetched.swap(fetched, Ordering::Relaxed);
        live.meter
            .lock()
            .unwrap()
            .record(fetched.saturating_sub(prev), Instant::now());
    }

    match fatal {
        Some(class) => Err(DownloadError::Failed(class)),
        None => Ok(()),
    }
}

/// Stand workers down without killing them mid-range: each finishes its current claim.
fn retire_down_to(tokens: &mut [CancellationToken], live: usize, target: usize) {
    let mut excess = live.saturating_sub(target.max(1));
    while excess > 0 {
        match tokens.iter().position(|t| !t.is_cancelled()) {
            Some(i) => {
                tokens[i].cancel();
                excess -= 1;
            }
            None => break,
        }
    }
}

fn snapshot(live: &Live, total: Option<u64>, retries: u64) -> Progress {
    let meter = live.meter.lock().unwrap();
    let done = live.bytes_done();
    let connections = live
        .workers
        .lock()
        .unwrap()
        .iter()
        .filter(|w| {
            !matches!(
                w.state(),
                worker::WorkerState::Done | worker::WorkerState::Failed
            )
        })
        .map(|w| ConnectionInfo {
            id: w.id,
            bytes: w.bytes.load(Ordering::Relaxed),
            bps: w.current_bps(),
            retries: w.retries.load(Ordering::Relaxed),
            claim: w.claim(),
            state: w.state(),
        })
        .collect();

    Progress {
        bytes_done: done,
        total,
        current_bps: meter.current_bps(),
        avg_bps: meter.current_bps(),
        peak_bps: meter.peak_bps(),
        eta: total.and_then(|t| meter.eta(t.saturating_sub(done))),
        conns: live.conns.load(Ordering::Relaxed),
        retries,
        connections,
    }
}

/// Hash the finished file in one sequential pass, off the hot path.
async fn hash_file(path: &std::path::Path) -> std::io::Result<String> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut f = std::fs::File::open(&path)?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1024 * 1024];
        loop {
            match f.read(&mut buf)? {
                0 => break,
                n => hasher.update(&buf[..n]),
            }
        }
        Ok(format!("{:x}", hasher.finalize()))
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task::probe::RangeSupport;

    fn pr(size: Option<u64>, support: RangeSupport) -> ProbeResult {
        ProbeResult {
            final_url: url::Url::parse("https://example.com/f.bin").unwrap(),
            filename: "f.bin".into(),
            total_size: size,
            range_support: support,
            etag: None,
            last_modified: None,
            content_type: None,
            disposition_filename: None,
            transforming_encoding: false,
            http_version: "HTTP/1.1".into(),
            redirect_chain: vec![],
        }
    }

    #[test]
    fn small_files_and_unrangeable_servers_get_one_connection() {
        let s = Settings::default();
        assert_eq!(
            initial_conns(&pr(Some(1024), RangeSupport::Supported), &s, None),
            1
        );
        assert_eq!(
            initial_conns(&pr(Some(1 << 30), RangeSupport::Unsupported), &s, None),
            1
        );
        assert_eq!(
            initial_conns(&pr(None, RangeSupport::Supported), &s, None),
            1
        );
    }

    #[test]
    fn large_files_start_at_a_conservative_four() {
        let s = Settings::default();
        assert_eq!(
            initial_conns(&pr(Some(1 << 30), RangeSupport::Supported), &s, None),
            4
        );
    }

    #[test]
    fn medium_files_start_smaller() {
        let s = Settings::default();
        assert_eq!(
            initial_conns(&pr(Some(16 << 20), RangeSupport::Supported), &s, None),
            2
        );
    }

    #[test]
    fn an_explicit_request_is_respected_within_limits() {
        let s = Settings::default();
        assert_eq!(
            initial_conns(&pr(Some(1 << 30), RangeSupport::Supported), &s, Some(16)),
            16
        );
        // But never beyond what the file can be split into.
        assert_eq!(
            initial_conns(&pr(Some(6 << 20), RangeSupport::Supported), &s, Some(16)),
            3
        );
    }

    #[test]
    fn retire_cancels_only_the_excess() {
        let mut tokens: Vec<CancellationToken> = (0..8).map(|_| CancellationToken::new()).collect();
        retire_down_to(&mut tokens, 8, 3);
        assert_eq!(tokens.iter().filter(|t| t.is_cancelled()).count(), 5);

        // Idempotent: asking again changes nothing.
        retire_down_to(&mut tokens, 3, 3);
        assert_eq!(tokens.iter().filter(|t| t.is_cancelled()).count(), 5);
    }

    #[test]
    fn retire_never_stands_down_the_last_worker() {
        let mut tokens: Vec<CancellationToken> = (0..4).map(|_| CancellationToken::new()).collect();
        retire_down_to(&mut tokens, 4, 0);
        assert!(
            tokens.iter().any(|t| !t.is_cancelled()),
            "at least one worker must survive or the download stalls"
        );
    }
}
