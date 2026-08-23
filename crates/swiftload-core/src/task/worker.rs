//! One worker: one TCP connection, streaming one claim at a time.
//!
//! The critical rule here is **verify before writing**. A server that advertises
//! `Accept-Ranges: bytes` and then ignores the `Range` header will send the whole file with
//! status 200. A worker that trusts the advertisement writes those bytes at its segment
//! offset and silently corrupts the download — the file ends up the right size, so nothing
//! downstream notices. So every response is checked against what was asked for *before* a
//! single byte reaches the writer.

use crate::{
    config::{RequestSpec, Settings},
    http::{
        client,
        errors::{classify_status, classify_transport, ErrorClass},
        headers,
    },
    task::{
        plan::{ClaimSlot, Grant, Plan},
        probe::RangeSupport,
        writer::WriterHandle,
    },
    util::{backoff::Backoff, rate::SpeedMeter},
};
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

/// Live, per-connection state. In memory only: it is meaningless after a restart, and the
/// UI's connection view reads from here rather than from the database.
#[derive(Debug)]
pub struct WorkerMetrics {
    pub id: usize,
    pub bytes: AtomicU64,
    pub retries: AtomicU64,
    pub requests: AtomicU64,
    meter: Mutex<SpeedMeter>,
    state: Mutex<WorkerState>,
    claim: Mutex<Option<(u64, u64)>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum WorkerState {
    Starting,
    Connecting,
    Streaming,
    Retrying,
    Stalled,
    Retiring,
    Done,
    Failed,
}

impl WorkerMetrics {
    pub fn new(id: usize) -> Arc<Self> {
        Arc::new(Self {
            id,
            bytes: AtomicU64::new(0),
            retries: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            meter: Mutex::new(SpeedMeter::new()),
            state: Mutex::new(WorkerState::Starting),
            claim: Mutex::new(None),
        })
    }

    pub fn state(&self) -> WorkerState {
        *self.state.lock().unwrap()
    }

    fn set_state(&self, s: WorkerState) {
        *self.state.lock().unwrap() = s;
    }

    fn set_claim(&self, c: Option<(u64, u64)>) {
        *self.claim.lock().unwrap() = c;
    }

    pub fn claim(&self) -> Option<(u64, u64)> {
        *self.claim.lock().unwrap()
    }

    pub fn current_bps(&self) -> u64 {
        self.meter.lock().unwrap().current_bps()
    }

    fn record(&self, n: u64, now: Instant) {
        self.bytes.fetch_add(n, Ordering::Relaxed);
        self.meter.lock().unwrap().record(n, now);
    }

    /// Decay the rate when nothing arrives, so a stalled worker reads as slow rather than
    /// frozen at its last good value.
    pub fn tick(&self, now: Instant) {
        self.meter.lock().unwrap().tick(now);
    }
}

/// Why a worker stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerExit {
    /// No work left to claim.
    Exhausted,
    /// Asked to stand down by the governor.
    Retired,
    Cancelled,
    /// Unrecoverable for the whole download.
    Failed(ErrorClass),
}

pub struct WorkerCtx {
    pub url: url::Url,
    pub settings: Settings,
    pub spec: RequestSpec,
    pub plan: Arc<Plan>,
    pub writer: WriterHandle,
    pub range_support: RangeSupport,
    pub total: Option<u64>,
    pub cancel: CancellationToken,
    /// Set by the governor to stand a worker down. It finishes its current claim first;
    /// nothing is killed mid-range.
    pub retire: CancellationToken,
    pub metrics: Arc<WorkerMetrics>,
    /// Shared error counter feeding the governor.
    pub errors: Arc<AtomicU64>,
}

/// Run one worker to completion.
pub async fn run(ctx: WorkerCtx) -> WorkerExit {
    let http = match client::build(&ctx.settings, &ctx.spec, &ctx.url) {
        Ok(c) => c,
        Err(_) => {
            ctx.metrics.set_state(WorkerState::Failed);
            return WorkerExit::Failed(ErrorClass::Fatal);
        }
    };
    let backoff = Backoff {
        max_attempts: ctx.settings.max_retries_per_segment,
        ..Default::default()
    };

    loop {
        if ctx.cancel.is_cancelled() {
            ctx.metrics.set_state(WorkerState::Done);
            return WorkerExit::Cancelled;
        }
        if ctx.retire.is_cancelled() {
            ctx.metrics.set_state(WorkerState::Retiring);
            return WorkerExit::Retired;
        }

        let slot = match ctx.plan.request() {
            Grant::Claim(c) => c,
            Grant::Exhausted => {
                ctx.metrics.set_state(WorkerState::Done);
                return WorkerExit::Exhausted;
            }
        };
        ctx.metrics.set_claim(Some((slot.cursor(), slot.end())));

        match fetch_claim(&ctx, &http, &slot, &backoff).await {
            Ok(()) => {
                ctx.plan.release(&slot);
                ctx.metrics.set_claim(None);
            }
            Err(exit) => {
                ctx.plan.release(&slot);
                ctx.metrics.set_claim(None);
                return exit;
            }
        }
    }
}

/// Fetch one claim, retrying transient failures from wherever the cursor got to.
async fn fetch_claim(
    ctx: &WorkerCtx,
    http: &reqwest::Client,
    slot: &Arc<ClaimSlot>,
    backoff: &Backoff,
) -> Result<(), WorkerExit> {
    let mut attempt = 0u32;

    loop {
        if slot.is_done() {
            return Ok(());
        }
        if ctx.cancel.is_cancelled() {
            return Err(WorkerExit::Cancelled);
        }

        match stream_once(ctx, http, slot).await {
            Ok(()) => return Ok(()),
            Err(class) => {
                ctx.errors.fetch_add(1, Ordering::Relaxed);

                // Non-retryable: hand the whole download the verdict.
                if !class.is_retryable() {
                    ctx.metrics.set_state(WorkerState::Failed);
                    return Err(WorkerExit::Failed(class));
                }
                if backoff.exhausted(attempt) {
                    ctx.metrics.set_state(WorkerState::Failed);
                    return Err(WorkerExit::Failed(class));
                }

                ctx.metrics.set_state(WorkerState::Retrying);
                ctx.metrics.retries.fetch_add(1, Ordering::Relaxed);

                // Rate limiting: obey the server's own figure when it gave one. Never work
                // around a limit by retrying harder.
                let wait = match &class {
                    ErrorClass::RateLimited {
                        retry_after: Some(d),
                    } => *d,
                    _ => backoff.delay_for(attempt),
                };
                attempt += 1;

                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = ctx.cancel.cancelled() => return Err(WorkerExit::Cancelled),
                }
                // The retry resumes from the cursor, so nothing already written is re-fetched.
            }
        }
    }
}

/// One HTTP request covering `[cursor, end)`, streamed to the writer.
async fn stream_once(
    ctx: &WorkerCtx,
    http: &reqwest::Client,
    slot: &Arc<ClaimSlot>,
) -> Result<(), ErrorClass> {
    let start = slot.cursor();
    let end = slot.end();
    if start >= end {
        return Ok(());
    }

    let use_range = ctx.range_support == RangeSupport::Supported;
    ctx.metrics.set_state(WorkerState::Connecting);
    ctx.metrics.requests.fetch_add(1, Ordering::Relaxed);

    let mut req = client::apply_spec(http.get(ctx.url.clone()), &ctx.spec);
    if use_range {
        req = req.header(
            reqwest::header::RANGE,
            format!("bytes={}-{}", start, end - 1),
        );
    }

    let resp = match req.send().await {
        Ok(r) => r,
        Err(e) => return Err(classify_transport(&e)),
    };
    let status = resp.status().as_u16();

    if !(200..300).contains(&status) {
        let retry_after = resp
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| headers::parse_retry_after(v, Duration::from_secs(120)));
        // A worker only runs on a URL that already probed successfully, so a 403 here means
        // the link expired rather than that access was never granted.
        return Err(classify_status(status, retry_after, true));
    }

    // ── Verification, before a single byte is written ──
    if use_range {
        if status != 206 {
            // Status 200 at offset 0 just means the server is serving the whole file: that is
            // recoverable by degrading to a single stream. Anywhere else, the server ignored
            // our Range and the body does not belong at our offset.
            return Err(if start == 0 {
                ErrorClass::RangeUnsupported
            } else {
                ErrorClass::RangeLied
            });
        }
        let cr = resp
            .headers()
            .get(reqwest::header::CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(headers::parse_content_range);

        match cr {
            Some(cr) => {
                if cr.start != start {
                    // The server sent a different part of the file than we asked for.
                    return Err(ErrorClass::RangeLied);
                }
                if let (Some(server_total), Some(expected)) = (cr.total, ctx.total) {
                    if server_total != expected {
                        // The file changed size underneath us; continuing would interleave
                        // bytes from two different versions.
                        return Err(ErrorClass::ResourceChanged);
                    }
                }
            }
            // 206 without a parseable Content-Range: we cannot confirm where these bytes go.
            None => return Err(ErrorClass::RangeLied),
        }
    }

    // A transforming encoding makes byte offsets meaningless.
    if resp
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .is_some_and(headers::is_transforming_encoding)
    {
        return Err(ErrorClass::RangeUnsupported);
    }

    ctx.metrics.set_state(WorkerState::Streaming);
    let mut resp = resp;

    loop {
        if ctx.cancel.is_cancelled() {
            return Err(ErrorClass::TransientNetwork);
        }

        // Idle timeout between chunks, deliberately not a deadline on the whole body: a
        // whole-body timeout on a multi-GB download is a bug, not a safety net.
        let next = tokio::time::timeout(ctx.settings.read_idle_timeout, resp.chunk()).await;

        let chunk = match next {
            Err(_elapsed) => {
                ctx.metrics.set_state(WorkerState::Stalled);
                return Err(ErrorClass::TransientNetwork);
            }
            Ok(Err(e)) => return Err(classify_transport(&e)),
            Ok(Ok(None)) => break, // clean end of body
            Ok(Ok(Some(c))) => c,
        };

        let cursor = slot.cursor();
        let limit = slot.end();
        if cursor >= limit {
            // Our claim was shrunk by a steal while we were streaming. Stop cleanly and let
            // the thief have the rest; the connection is simply dropped.
            break;
        }

        // Never write past our claim, even if the server sends more than we asked for.
        let take = ((limit - cursor) as usize).min(chunk.len());
        let bytes = chunk.slice(..take);

        // Awaiting here is what applies backpressure: when the disk is behind, this blocks
        // and memory stays bounded regardless of connection count.
        if ctx.writer.write(cursor, bytes).await.is_err() {
            return Err(ErrorClass::LocalFatal);
        }

        slot.advance(take as u64);
        ctx.metrics.record(take as u64, Instant::now());

        if take < chunk.len() {
            break; // claim satisfied mid-chunk
        }
    }

    // The body ended. If it ended early we did not get everything we asked for, which is a
    // truncated response rather than success — retry from wherever the cursor stopped.
    if use_range && slot.cursor() < slot.end() && !ctx.retire.is_cancelled() {
        return Err(ErrorClass::TransientNetwork);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_track_bytes_and_state() {
        let m = WorkerMetrics::new(3);
        assert_eq!(m.id, 3);
        assert_eq!(m.state(), WorkerState::Starting);

        m.set_state(WorkerState::Streaming);
        m.record(1000, Instant::now());
        assert_eq!(m.bytes.load(Ordering::Relaxed), 1000);
        assert_eq!(m.state(), WorkerState::Streaming);
    }

    #[test]
    fn metrics_expose_the_current_claim() {
        let m = WorkerMetrics::new(0);
        assert_eq!(m.claim(), None);
        m.set_claim(Some((100, 500)));
        assert_eq!(m.claim(), Some((100, 500)));
    }

    #[test]
    fn a_stalled_worker_decays_to_zero_rather_than_reporting_a_stale_rate() {
        let m = WorkerMetrics::new(0);
        let t = Instant::now();
        for i in 1..=30 {
            m.record(1_000_000, t + Duration::from_millis(i * 100));
        }
        assert!(m.current_bps() > 1_000_000);

        m.tick(t + Duration::from_secs(60));
        assert!(
            m.current_bps() < 100_000,
            "stalled worker still reports {}",
            m.current_bps()
        );
    }
}
