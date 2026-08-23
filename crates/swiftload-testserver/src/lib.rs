//! Deterministic, adversarial HTTP server for SwiftLoad's tests and benchmarks.
//!
//! Two jobs:
//!
//! 1. **Determinism** — content is a pure function of `(seed, offset)` (see [`content`]), so a
//!    test can assert byte-exactness of a multi-GB assembly without storing a reference file.
//! 2. **Adversarial behaviour** — servers in the wild lie about range support, omit
//!    `Content-Length`, throttle per connection or per IP, reset mid-stream, rotate ETags and
//!    expire signed URLs. Every one of those is reproducible here, on demand, in-process.
//!
//! The throttling modes matter most of all: `per=conn` versus `per=total` is precisely the
//! distinction the adaptive concurrency governor has to detect, and here it is ground truth
//! rather than a guess about what some CDN is doing.

pub mod content;

use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use futures_util::stream;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

const CHUNK: usize = 64 * 1024;

// ─────────────────────────────── statistics ───────────────────────────────

/// Server-side counters. These are the source of truth for assertions like "16 workers
/// opened 16 distinct TCP connections" and "validation transferred under 1 MB" — both of
/// which would be meaningless if measured by the client under test.
#[derive(Default, Debug)]
pub struct Stats {
    /// Distinct TCP accepts. The h2-multiplexing trap is invisible at the HTTP layer and
    /// only shows up here.
    pub accepts: AtomicU64,
    pub open_conns: AtomicU64,
    pub requests: AtomicU64,
    pub bytes_served: AtomicU64,
    pub range_requests: AtomicU64,
    pub full_requests: AtomicU64,
    pub resets_injected: AtomicU64,
    pub status_403: AtomicU64,
    pub status_429: AtomicU64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StatsSnapshot {
    pub accepts: u64,
    pub open_conns: u64,
    pub requests: u64,
    pub bytes_served: u64,
    pub range_requests: u64,
    pub full_requests: u64,
    pub resets_injected: u64,
    pub status_403: u64,
    pub status_429: u64,
    pub peak_concurrent_streams: u64,
}

// ─────────────────────────────── shared state ───────────────────────────────

pub struct AppState {
    pub stats: Stats,
    /// Valid tokens per seed, for the signed-URL scenarios.
    signed: Mutex<HashMap<String, HashSet<String>>>,
    /// Shared token bucket for `per=total` throttling.
    /// Shared bucket for `per=total`, tagged with the rate it was built for so a run at a
    /// different rate cannot inherit the previous run's accumulated tokens.
    total_bucket: tokio::sync::Mutex<Option<(u64, Bucket)>>,
    /// In-flight streams, for `maxconn` and peak tracking.
    live_streams: AtomicU64,
    peak_streams: AtomicU64,
    /// Monotonic counter used to rotate ETags.
    etag_tick: AtomicU64,
    base_url: Mutex<String>,
}

impl AppState {
    fn new() -> Self {
        Self {
            stats: Stats::default(),
            signed: Mutex::new(HashMap::new()),
            total_bucket: tokio::sync::Mutex::new(None),
            live_streams: AtomicU64::new(0),
            peak_streams: AtomicU64::new(0),
            etag_tick: AtomicU64::new(0),
            base_url: Mutex::new(String::new()),
        }
    }

    pub fn snapshot(&self) -> StatsSnapshot {
        let s = &self.stats;
        StatsSnapshot {
            accepts: s.accepts.load(Ordering::Relaxed),
            open_conns: s.open_conns.load(Ordering::Relaxed),
            requests: s.requests.load(Ordering::Relaxed),
            bytes_served: s.bytes_served.load(Ordering::Relaxed),
            range_requests: s.range_requests.load(Ordering::Relaxed),
            full_requests: s.full_requests.load(Ordering::Relaxed),
            resets_injected: s.resets_injected.load(Ordering::Relaxed),
            status_403: s.status_403.load(Ordering::Relaxed),
            status_429: s.status_429.load(Ordering::Relaxed),
            peak_concurrent_streams: self.peak_streams.load(Ordering::Relaxed),
        }
    }

    fn reset_stats(&self) {
        for c in [
            &self.stats.accepts,
            &self.stats.requests,
            &self.stats.bytes_served,
            &self.stats.range_requests,
            &self.stats.full_requests,
            &self.stats.resets_injected,
            &self.stats.status_403,
            &self.stats.status_429,
        ] {
            c.store(0, Ordering::Relaxed);
        }
        self.peak_streams.store(0, Ordering::Relaxed);
    }
}

/// Simple token bucket used for both per-connection and shared throttling.
struct Bucket {
    tokens: f64,
    rate: f64,
    cap: f64,
    last: Instant,
}

impl Bucket {
    fn new(rate: f64) -> Self {
        // Burst is deliberately small. A bucket left to accumulate a full second of tokens
        // while idle hands the next request a large head start, which reads as a baseline well
        // above the configured cap and quietly invalidates every comparison against it.
        let burst = (rate / 10.0).max(CHUNK as f64 * 2.0);
        Self {
            tokens: burst,
            rate,
            cap: burst,
            last: Instant::now(),
        }
    }
    /// Time to wait before `n` bytes may be sent, consuming them.
    fn reserve(&mut self, n: u64) -> Duration {
        let now = Instant::now();
        self.tokens =
            (self.tokens + now.duration_since(self.last).as_secs_f64() * self.rate).min(self.cap);
        self.last = now;
        self.tokens -= n as f64;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.rate)
        }
    }
}

// ─────────────────────────────── scenario ───────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeMode {
    /// Honest: advertises `Accept-Ranges: bytes` and serves 206.
    Yes,
    /// Honest refusal: `Accept-Ranges: none`, always 200 with the whole body.
    No,
    /// **Lies**: advertises `bytes`, then ignores `Range` and returns 200. The nastiest
    /// real-world case, because a naive client writes full-file bytes at a segment offset.
    Lie,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EtagMode {
    Stable,
    /// Different ETag on every response, identical content — simulates CDN-node variance
    /// and is the case that must NOT be treated as proof of a different file.
    Rotate,
    None,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Q {
    pub bps: Option<u64>,
    pub per: Option<String>,
    pub rtt: Option<u64>,
    pub reset_at: Option<u64>,
    pub token: Option<String>,
    pub n: Option<usize>,
    pub name: Option<String>,
    pub stall: Option<u64>,
}

#[derive(Debug, Clone)]
struct Scenario {
    seed: u64,
    size: u64,
    advertise_len: bool,
    ranges: RangeMode,
    etag: EtagMode,
    last_modified: bool,
    filename: Option<String>,
    gzip_anyway: bool,
    rtt: Duration,
    bps: Option<u64>,
    per_total: bool,
    reset_at: Option<u64>,
    stall_after: Option<u64>,
    max_conn: Option<usize>,
}

impl Scenario {
    fn plain(seed: u64, size: u64) -> Self {
        Self {
            seed,
            size,
            advertise_len: true,
            ranges: RangeMode::Yes,
            etag: EtagMode::Stable,
            last_modified: true,
            filename: None,
            gzip_anyway: false,
            rtt: Duration::ZERO,
            bps: None,
            per_total: false,
            reset_at: None,
            stall_after: None,
            max_conn: None,
        }
    }
    fn with_q(mut self, q: &Q) -> Self {
        self.bps = q.bps;
        self.per_total = q.per.as_deref() == Some("total");
        self.rtt = Duration::from_millis(q.rtt.unwrap_or(0));
        self.reset_at = q.reset_at;
        self.stall_after = q.stall;
        self.max_conn = q.n;
        if let Some(name) = &q.name {
            self.filename = Some(name.clone());
        }
        self
    }
}

// ─────────────────────────────── router ───────────────────────────────

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/plain/{seed}/{size}", get(h_plain))
        .route("/norange/{seed}/{size}", get(h_norange))
        .route("/liar/{seed}/{size}", get(h_liar))
        .route("/nolen/{seed}/{size}", get(h_nolen))
        .route("/throttle/{seed}/{size}", get(h_throttle))
        .route("/latency/{seed}/{size}", get(h_plain))
        .route("/flaky/{seed}/{size}", get(h_plain))
        .route("/slowconn/{seed}/{size}", get(h_plain))
        .route("/maxconn/{seed}/{size}", get(h_maxconn))
        .route("/gzip/{seed}/{size}", get(h_gzip))
        .route("/rotate-etag/{seed}/{size}", get(h_rotate_etag))
        .route("/nometa/{seed}/{size}", get(h_nometa))
        .route("/decoy/{seed}/{size}", get(h_plain))
        .route("/signed/{seed}/{size}", get(h_signed))
        .route("/mint/{seed}", get(h_mint))
        .route("/expire/{seed}", get(h_expire))
        .route("/redirect/{hops}/{seed}/{size}", get(h_redirect))
        .route("/status/{code}", get(h_status))
        .route("/stats", get(h_stats))
        .route("/stats/reset", get(h_stats_reset))
        .with_state(state)
}

async fn h_plain(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    serve(
        st,
        Scenario::plain(content::seed_of(&seed), size).with_q(&q),
        &headers,
    )
    .await
}

async fn h_norange(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let mut s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    s.ranges = RangeMode::No;
    serve(st, s, &headers).await
}

async fn h_liar(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let mut s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    s.ranges = RangeMode::Lie;
    serve(st, s, &headers).await
}

async fn h_nolen(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let mut s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    s.advertise_len = false;
    s.ranges = RangeMode::No;
    serve(st, s, &headers).await
}

async fn h_throttle(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    serve(st, s, &headers).await
}

async fn h_maxconn(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let mut s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    s.max_conn = Some(q.n.unwrap_or(6));
    serve(st, s, &headers).await
}

async fn h_gzip(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let mut s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    s.gzip_anyway = true;
    serve(st, s, &headers).await
}

async fn h_rotate_etag(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let mut s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    s.etag = EtagMode::Rotate;
    serve(st, s, &headers).await
}

async fn h_nometa(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let mut s = Scenario::plain(content::seed_of(&seed), size).with_q(&q);
    s.etag = EtagMode::None;
    s.last_modified = false;
    serve(st, s, &headers).await
}

/// Signed-URL scenario: rejects with 403 unless the token is currently valid.
async fn h_signed(
    State(st): State<Arc<AppState>>,
    Path((seed, size)): Path<(String, u64)>,
    Query(q): Query<Q>,
    headers: HeaderMap,
) -> Response {
    let ok = {
        let map = st.signed.lock().unwrap();
        match (&q.token, map.get(&seed)) {
            (Some(t), Some(valid)) => valid.contains(t),
            _ => false,
        }
    };
    if !ok {
        st.stats.status_403.fetch_add(1, Ordering::Relaxed);
        return (StatusCode::FORBIDDEN, "signature expired or invalid").into_response();
    }
    serve(
        st,
        Scenario::plain(content::seed_of(&seed), size).with_q(&q),
        &headers,
    )
    .await
}

/// Issue a fresh signed URL for the same underlying content — "the user got a new link".
async fn h_mint(
    State(st): State<Arc<AppState>>,
    Path(seed): Path<String>,
    Query(q): Query<Q>,
) -> Response {
    let token: String = {
        use rand::Rng;
        let mut rng = rand::rng();
        (0..24)
            .map(|_| char::from(b'a' + rng.random_range(0..26)))
            .collect()
    };
    st.signed
        .lock()
        .unwrap()
        .entry(seed.clone())
        .or_default()
        .insert(token.clone());
    let size = q.n.unwrap_or(1 << 20);
    let base = st.base_url.lock().unwrap().clone();
    let url = format!("{base}/signed/{seed}/{size}?token={token}&expires=9999999999");
    (StatusCode::OK, url).into_response()
}

/// Invalidate every outstanding token for a seed — "the link expired".
async fn h_expire(State(st): State<Arc<AppState>>, Path(seed): Path<String>) -> Response {
    st.signed.lock().unwrap().remove(&seed);
    (StatusCode::OK, "expired").into_response()
}

async fn h_redirect(Path((hops, seed, size)): Path<(u32, String, u64)>) -> Response {
    let next = if hops <= 1 {
        format!("/plain/{seed}/{size}")
    } else {
        format!("/redirect/{}/{seed}/{size}", hops - 1)
    };
    let mut h = HeaderMap::new();
    h.insert(header::LOCATION, HeaderValue::from_str(&next).unwrap());
    (StatusCode::FOUND, h).into_response()
}

async fn h_status(
    State(st): State<Arc<AppState>>,
    Path(code): Path<u16>,
    Query(q): Query<Q>,
) -> Response {
    match code {
        403 => st.stats.status_403.fetch_add(1, Ordering::Relaxed),
        429 => st.stats.status_429.fetch_add(1, Ordering::Relaxed),
        _ => 0,
    };
    let status = StatusCode::from_u16(code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut h = HeaderMap::new();
    if matches!(code, 429 | 503) {
        // Retry-After must be honoured rather than routed around with more connections.
        h.insert(
            header::RETRY_AFTER,
            HeaderValue::from_str(&q.n.unwrap_or(1).to_string()).unwrap(),
        );
    }
    (status, h, format!("status {code}")).into_response()
}

async fn h_stats(State(st): State<Arc<AppState>>) -> Response {
    axum::Json(st.snapshot()).into_response()
}

async fn h_stats_reset(State(st): State<Arc<AppState>>) -> Response {
    st.reset_stats();
    (StatusCode::OK, "reset").into_response()
}

// ─────────────────────────────── core response builder ───────────────────────────────

fn parse_range(headers: &HeaderMap, size: u64) -> Option<(u64, u64)> {
    let raw = headers.get(header::RANGE)?.to_str().ok()?;
    let spec = raw.strip_prefix("bytes=")?;
    let (a, b) = spec.split_once('-')?;
    let start: u64 = a.trim().parse().ok()?;
    let end = match b.trim() {
        "" => size.saturating_sub(1),
        v => v.parse::<u64>().ok()?.min(size.saturating_sub(1)),
    };
    if start > end {
        return None;
    }
    Some((start, end))
}

async fn serve(st: Arc<AppState>, sc: Scenario, headers: &HeaderMap) -> Response {
    st.stats.requests.fetch_add(1, Ordering::Relaxed);

    // Server-side connection cap: refuse beyond N concurrent streams.
    if let Some(limit) = sc.max_conn {
        let live = st.live_streams.load(Ordering::Relaxed);
        if live as usize >= limit {
            st.stats.status_429.fetch_add(1, Ordering::Relaxed);
            let mut h = HeaderMap::new();
            h.insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            return (StatusCode::TOO_MANY_REQUESTS, h, "too many connections").into_response();
        }
    }

    let requested = parse_range(headers, sc.size);
    let honour_range = matches!(sc.ranges, RangeMode::Yes) && requested.is_some();

    if requested.is_some() {
        st.stats.range_requests.fetch_add(1, Ordering::Relaxed);
    } else {
        st.stats.full_requests.fetch_add(1, Ordering::Relaxed);
    }

    // 416 only when the server actually implements ranges.
    if matches!(sc.ranges, RangeMode::Yes) {
        if let Some(raw) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
            if raw.starts_with("bytes=") && requested.is_none() {
                return (StatusCode::RANGE_NOT_SATISFIABLE, "bad range").into_response();
            }
        }
    }

    let (start, end) = if honour_range {
        requested.unwrap()
    } else {
        (0, sc.size.saturating_sub(1))
    };
    let body_len = end + 1 - start;

    let mut h = HeaderMap::new();
    h.insert(
        header::ACCEPT_RANGES,
        HeaderValue::from_static(match sc.ranges {
            // The liar advertises support it does not honour.
            RangeMode::Yes | RangeMode::Lie => "bytes",
            RangeMode::No => "none",
        }),
    );
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );

    if sc.advertise_len {
        h.insert(
            header::CONTENT_LENGTH,
            HeaderValue::from_str(&body_len.to_string()).unwrap(),
        );
    }
    match sc.etag {
        EtagMode::Stable => {
            h.insert(
                header::ETAG,
                HeaderValue::from_str(&format!("\"{:016x}\"", sc.seed)).unwrap(),
            );
        }
        EtagMode::Rotate => {
            let n = st.etag_tick.fetch_add(1, Ordering::Relaxed);
            h.insert(
                header::ETAG,
                HeaderValue::from_str(&format!("\"rot-{n:08x}\"")).unwrap(),
            );
        }
        EtagMode::None => {}
    }
    if sc.last_modified {
        h.insert(
            header::LAST_MODIFIED,
            HeaderValue::from_static("Wed, 01 Jan 2025 00:00:00 GMT"),
        );
    }
    if let Some(name) = &sc.filename {
        h.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{name}\"")).unwrap(),
        );
    }
    if sc.gzip_anyway {
        // Claim an encoding we were never asked for. Byte ranges would then address the
        // *compressed* stream while the client transparently decompresses — silent corruption
        // unless the client detects it and falls back to a single stream.
        h.insert(header::CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    }

    let status = if honour_range {
        h.insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{}", sc.size)).unwrap(),
        );
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    if let (true, Some(rate)) = (sc.per_total, sc.bps) {
        let mut b = st.total_bucket.lock().await;
        if b.as_ref().is_none_or(|(r, _)| *r != rate) {
            *b = Some((rate, Bucket::new(rate as f64)));
        }
    }

    st.live_streams.fetch_add(1, Ordering::Relaxed);
    st.peak_streams
        .fetch_max(st.live_streams.load(Ordering::Relaxed), Ordering::Relaxed);

    let body = Body::from_stream(body_stream(st, sc, start, body_len));
    (status, h, body).into_response()
}

/// The response body: deterministic bytes, with latency, throttling, stalls and resets
/// injected exactly where a real server would inflict them.
fn body_stream(
    st: Arc<AppState>,
    sc: Scenario,
    start: u64,
    len: u64,
) -> impl stream::Stream<Item = Result<Vec<u8>, std::io::Error>> {
    /// Decrements the live-stream counter however the stream ends — completed, reset, or
    /// dropped when the client hangs up. Lives in the fold state so it drops with the stream.
    struct Guard(Arc<AppState>);
    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.live_streams.fetch_sub(1, Ordering::Relaxed);
        }
    }

    struct S {
        sent: u64,
        done: bool,
        first: bool,
        bucket: Option<Bucket>,
        _guard: Guard,
    }

    let init = S {
        sent: 0,
        done: false,
        first: true,
        bucket: if sc.per_total {
            None
        } else {
            sc.bps.map(|b| Bucket::new(b as f64))
        },
        _guard: Guard(st.clone()),
    };

    stream::unfold(init, move |mut s| {
        let st = st.clone();
        let sc = sc.clone();
        async move {
            if s.done || s.sent >= len {
                return None;
            }

            // Time-to-first-byte latency.
            if std::mem::replace(&mut s.first, false) && !sc.rtt.is_zero() {
                tokio::time::sleep(sc.rtt).await;
            }

            // Deliberate mid-stream stall: stop producing bytes without closing the
            // connection, so it is the client's idle-read timeout that has to notice.
            if let Some(after) = sc.stall_after {
                if s.sent >= after {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return None;
                }
            }

            // Mid-stream connection reset.
            if let Some(at) = sc.reset_at {
                if s.sent >= at {
                    st.stats.resets_injected.fetch_add(1, Ordering::Relaxed);
                    s.done = true;
                    let err =
                        std::io::Error::new(std::io::ErrorKind::ConnectionReset, "injected reset");
                    return Some((Err(err), s));
                }
            }

            let n = CHUNK.min((len - s.sent) as usize);

            // Throttling. `per=conn` gives each connection its own budget, so parallelism
            // wins near-linearly; `per=total` shares one budget across every connection, so
            // it does not. Telling those two apart is the governor's whole job.
            let wait = if sc.per_total {
                match sc.bps {
                    Some(rate) => {
                        let mut g = st.total_bucket.lock().await;
                        if g.as_ref().is_none_or(|(r, _)| *r != rate) {
                            *g = Some((rate, Bucket::new(rate as f64)));
                        }
                        g.as_mut()
                            .map_or(Duration::ZERO, |(_, b)| b.reserve(n as u64))
                    }
                    None => Duration::ZERO,
                }
            } else {
                s.bucket
                    .as_mut()
                    .map_or(Duration::ZERO, |b| b.reserve(n as u64))
            };
            if !wait.is_zero() {
                tokio::time::sleep(wait).await;
            }

            let buf = content::chunk(sc.seed, start + s.sent, n);
            st.stats.bytes_served.fetch_add(n as u64, Ordering::Relaxed);
            s.sent += n as u64;
            Some((Ok(buf), s))
        }
    })
}

// ─────────────────────────────── connection-counting listener ───────────────────────────────

/// Wraps a `TcpListener` purely to count accepts.
///
/// This exists for one reason: if HTTP/2 multiplexes sixteen "parallel" segment requests onto
/// a single TCP connection, everything looks perfect at the HTTP layer while throughput is
/// silently that of one connection. Only an accept counter catches it.
pub struct CountingListener {
    inner: tokio::net::TcpListener,
    stats: Arc<AppState>,
}

impl axum::serve::Listener for CountingListener {
    type Io = tokio::net::TcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.inner.accept().await {
                Ok((io, addr)) => {
                    self.stats.stats.accepts.fetch_add(1, Ordering::Relaxed);
                    self.stats.stats.open_conns.fetch_add(1, Ordering::Relaxed);
                    return (io, addr);
                }
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

/// A running server instance.
pub struct Handle {
    pub addr: SocketAddr,
    pub state: Arc<AppState>,
}

impl Handle {
    pub fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
    pub fn stats(&self) -> StatsSnapshot {
        self.state.snapshot()
    }
}

/// Bind to `addr` and serve in the background. Pass port 0 for an ephemeral port, which is
/// what tests want so they can run concurrently.
pub async fn spawn(addr: SocketAddr) -> std::io::Result<Handle> {
    let state = Arc::new(AppState::new());
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    *state.base_url.lock().unwrap() = format!("http://{bound}");

    let app = router(state.clone());
    let counting = CountingListener {
        inner: listener,
        stats: state.clone(),
    };
    tokio::spawn(async move {
        let _ = axum::serve(counting, app).await;
    });
    Ok(Handle { addr: bound, state })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_header_parsing() {
        let mut h = HeaderMap::new();
        h.insert(header::RANGE, HeaderValue::from_static("bytes=100-199"));
        assert_eq!(parse_range(&h, 1000), Some((100, 199)));

        h.insert(header::RANGE, HeaderValue::from_static("bytes=100-"));
        assert_eq!(parse_range(&h, 1000), Some((100, 999)), "open-ended range");

        h.insert(header::RANGE, HeaderValue::from_static("bytes=0-0"));
        assert_eq!(parse_range(&h, 1000), Some((0, 0)), "the probe request");

        // End beyond EOF is clamped, per RFC 9110.
        h.insert(header::RANGE, HeaderValue::from_static("bytes=900-99999"));
        assert_eq!(parse_range(&h, 1000), Some((900, 999)));

        h.insert(header::RANGE, HeaderValue::from_static("bytes=500-100"));
        assert_eq!(parse_range(&h, 1000), None, "inverted range is invalid");

        h.insert(header::RANGE, HeaderValue::from_static("garbage"));
        assert_eq!(parse_range(&h, 1000), None);
    }

    #[test]
    fn bucket_throttles_to_the_configured_rate() {
        let mut b = Bucket::new(1_000_000.0);
        // Draining well past the bucket capacity must produce a positive wait.
        let mut total = Duration::ZERO;
        for _ in 0..40 {
            total += b.reserve(64 * 1024);
        }
        assert!(
            total > Duration::from_millis(500),
            "throttle produced only {total:?}"
        );
    }
}
