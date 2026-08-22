# SwiftLoad — Implementation Plan

## Context

**What we're building.** SwiftLoad: a free, modern Windows download manager positioned against
IDM. Repo `priyanktomer/AiGen` is **completely empty** (no commits, no files) — this is greenfield,
so there is no existing code to reuse and nothing to explore.

**Why.** IDM is paid, its UI is dated, and its performance claims are unverifiable. The opportunity
is a download manager that is *demonstrably* fast — adaptive rather than brute-force in its
concurrency, honest about when parallelism helps, and shipping a public benchmark harness that
proves it.

**Intended outcome after 3 sessions.** A genuinely functional Windows installer producing an app
that segments downloads intelligently, survives crashes/network loss/restarts without restarting
from zero, and carries a reproducible benchmark report showing exactly under which conditions it
beats a single-stream downloader — and under which it doesn't.

### Decisions locked with the user

1. **Windows-only.** `seek_write`, `FSCTL_SET_SPARSE`, `MoveFileExW`, Mark-of-the-Web, and
   `INetworkListManager` are called directly — no platform abstraction layer, no Unix impl, no
   `#[cfg]` ladders. OS calls still live only inside `crates/swiftload-core/src/fsx/` behind narrow
   function signatures, so a future port is a mechanical addition to four files rather than a
   rewrite, but nothing is built or tested for it now.
2. **The deliverable is the finished product, not a session count.** The three sessions below are a
   sequencing guide, not a budget to spend down. **§J (Definition of Done) governs completion** — if
   work overruns a session boundary it continues into the next; if it underruns, pull the next
   session's work forward. Two consequences, both already applied to §F:
   - The **highest-risk unknowns are settled in Session 1**, before a single line of UI exists — the
     h2 distinct-connection proof, the crash-recovery matrix, and a minimal throughput benchmark are
     Session 1 exit criteria, not Session 3 discoveries.
   - **Session 2 ends shippable.** Installer, security pass, and a README results table land there,
     so there is a releasable product even if Session 3 is short. Session 3 is then tuning,
     soak-testing and polish — improving a working product rather than finishing an unfinished one.
3. **Resume-with-a-replaced-URL is a first-class core feature**, not an add-on. Download identity is
   the `DownloadId`; a URL is a *revocable attribute* of it. See §D.10 for the full design and
   §D.10.0 for what the plan already covered versus what changed.

---

## 0. Three framing facts that drive every decision below

### 0.1 You cannot download faster than your link

If the server saturates the user's pipe with one stream, SwiftLoad, IDM, curl and Chrome all finish
in identical time. Multi-connection downloading is **not a speed multiplier** — it is a technique
for *recovering throughput a single TCP flow leaves on the table*. It pays when:

| Condition | Mechanism | Typical gain |
|---|---|---|
| Server enforces a **per-connection** rate cap | N conns → N × cap, up to a per-IP ceiling | near-linear |
| **High BDP** (150 ms+ RTT) | one flow's cwnd ramps slowly & halves on loss; N flows fill the pipe | 2–6× |
| **Packet loss** (~0.1%+) | single-flow ≈ MSS/(RTT·√p); parallel flows multiply it | 2–10× |
| DNS round-robin / anycast | different conns hit differently-loaded edges | variable |
| **Straggler / dying route** | rebalancing routes around it; a single stream just stalls | avoids worst case |

It is **neutral or harmful** when: the last-mile link is already the bottleneck; the server caps
**per-IP total** (zero gain, 32× the load); the file is small (< ~4 MB — handshakes cost more than
they save); the server answers connection floods with 429/503; disk is the bottleneck (HDD,
BitLocker, AV real-time scanning); or bufferbloat wrecks the user's interactive latency.

**Consequence:** "benchmark 4/8/12/16/24/32 and pick the winner" is right in spirit but wrong if
implemented naively. A blind ramp on a saturated link measures noise, burns CPU, and irritates
servers. The engine must **discriminate between the causes of a plateau**, not merely detect one.
See §D.5.

### 0.2 The engine's language is not the performance lever

Rust, Go, C#, even Node can all saturate 1 Gbps. At 125 MB/s with 64 KB reads that's ~2,000
syscalls/sec — nothing. What actually determines throughput, in order of impact:

1. **Correct concurrency decisions** (~80% of it).
2. **Not accidentally multiplexing all "parallel" streams onto one TCP connection** — the HTTP/2
   trap (§D.2). This single bug silently turns a 16-connection downloader into a 1-connection one
   on any h2 CDN, and it looks exactly like "the server is saturated."
3. **h2 flow-control window sizing** on high-BDP paths (defaults are 64 KB — catastrophic).
4. Connection reuse across chunks (avoiding a handshake per chunk).
5. Write strategy: large positional writes, backpressure, no per-chunk fsync.

So language gets chosen on **memory footprint, packaging, correctness guarantees and
maintainability** — which is how §A decides it.

### 0.3 What "faster than IDM" can honestly mean

IDM's engine is mature and good. The defensible claim:

> On typical CDN downloads SwiftLoad is **within measurement noise of IDM**; on
> per-connection-throttled and high-latency/lossy sources it is **competitive or better**; it uses
> **substantially less RAM**; it recovers from stalls, resets and restarts **more reliably**; and
> unlike IDM its performance claims are **reproducible from a public harness**. It is free.

We publish the harness and the conditions. We never claim "always faster" — the first user with
fibre and a nearby mirror would disprove it.

---

## A. Recommended tech stack

### **Rust engine (`swiftload-core`) + Tauri v2 shell + React/TypeScript UI**

| Layer | Choice | Why |
|---|---|---|
| Engine | **Rust**, `tokio` + `reqwest`/`hyper` | Precise control over connection pools (critical for §D.2), no GC pauses on multi-GB transfers, cheap structured cancellation, `bytes::Bytes` refcounting moves chunks worker→writer with no copy, single static binary. |
| TLS | **rustls** + `rustls-platform-verifier` | No OpenSSL build pain on Windows. Uses the **Windows cert store**, so enterprise roots and corporate MITM proxies work *without* us ever shipping a "skip cert check" escape hatch. |
| Persistence | **SQLite** (`rusqlite`, bundled), WAL | ACID, single file, crash-safe, zero-config. Checkpoint protocol in §D.7. |
| Shell | **Tauri v2** | Uses the WebView2 runtime already on Win 10/11 → **~8–12 MB installer** (Electron ~120 MB+), **~60–90 MB idle RSS** (Electron ~250 MB+). Ships NSIS **and** MSI via `tauri-bundler`. Engine is a linked crate — UI↔engine IPC is a function call, not a socket. |
| UI | **React 18 + TS + Vite + Tailwind v4 + Radix primitives** | Fastest path to a genuinely modern UI in one session. Radix gives accessible dialog/tabs/dropdown/tooltip with no heavyweight component library. Charts: hand-rolled SVG sparklines (~80 lines), no chart library. |
| UI state | **Zustand** | ~1 KB, no provider pyramid. |
| Test server | **axum** (own crate) | Adversarial + deterministic HTTP server — the foundation of the entire test and benchmark story. |
| Installer | NSIS (primary, per-user, no admin) + MSI (enterprise/silent) | Zero extra work via `tauri-bundler`. |

### Why not the alternatives

- **Option B — Go + Wails.** Genuinely viable, slightly faster to write. Rejected because
  `net/http`'s transport gives less direct control over *forcing distinct TCP connections* and h2
  window sizing — precisely the two things that matter most here; GC pauses are small but non-zero
  under 32 streams; and Wails' packaging/auto-update story trails Tauri's. The delta is not large:
  **if the implementer is markedly stronger in Go, substitute it — nothing in §C, §D, §G or §I
  changes**, they're language-neutral.
- **C# .NET 9 + WinUI 3.** Strongest challenger and the most Windows-native result;
  `SocketsHttpHandler` is excellent. Rejected on WinUI 3 packaging pain (MSIX vs unpackaged),
  60–80 MB self-contained installer, and LOH pressure from big buffers needing `ArrayPool`
  discipline that's easy to get subtly wrong. Choose it only if native fidelity outranks all else.
- **Electron + sidecar.** 10× the RAM and installer size for an app whose pitch is being
  lightweight. Self-defeating.
- **Win32/C++.** Cannot hit a 3-session budget.

### The one non-obvious call: no separate engine process

`swiftload-core` is a **separate crate with zero UI dependencies** (CI-enforced via `cargo-deny`),
but it runs **in-process** with the shell.

- Separation is real: the same crate is driven by `swiftload-cli` (headless — Session 1's only
  "UI", and the CI harness) and `swiftload-bench`.
- We skip a real IPC boundary (serialization, supervision, orphan cleanup, version skew) that buys
  nothing at this scale.
- The boundary is *already* an async command/event API, so promoting it to a background service
  later is contained, not a rewrite (§B.3).

---

## B. Architecture

```text
┌───────────────────────────────────────────────────────────────────────┐
│  UI — React + TypeScript (WebView2)                                   │
│  Downloads │ Queue │ Completed │ Settings   + per-item Details drawer  │
└──────────────────────────┬────────────────────────────────────────────┘
        invoke() commands (req/resp)  ▲ emit() events (batched 4 Hz)
                           ▼          │
┌───────────────────────────────────────────────────────────────────────┐
│  swiftload-app — Tauri v2 shell (Rust)                                │
│  • thin command handlers (validate → core → map errors)               │
│  • EventPump: coalesces engine events into 4 Hz UI snapshots          │
│  • OS integration: dialogs, tray, notifications, single-instance,     │
│    autostart, reveal-in-Explorer, theme + Mica backdrop               │
└──────────────────────────┬────────────────────────────────────────────┘
              Command mpsc ▼          ▲ Event broadcast
┌───────────────────────────────────────────────────────────────────────┐
│  swiftload-core — engine crate (no UI deps at all)                    │
│  ┌──────────────────────────────────────────────────────────────────┐ │
│  │ Manager — public API, task registry, store handle                │ │
│  └───────┬──────────────────────────────────────────────────────────┘ │
│  ┌───────▼─────────┐  ┌────────────────┐  ┌────────────────────────┐  │
│  │ Scheduler       │  │ StateStore     │  │ HostProfileCache       │  │
│  │ • queue order   │  │ • SQLite WAL   │  │ • learned useful conns │  │
│  │ • N-concurrent  │  │ • checkpoints  │  │ • per-conn cap inferred│  │
│  │ • per-host cap  │  │ • crash recov. │  │ • 429 history          │  │
│  │ • GLOBAL PROBE  │  └────────────────┘  └────────────────────────┘  │
│  │   TOKEN (1 only)│                                                  │
│  └───────┬─────────┘                                                  │
│  ┌───────▼──────────────────────────────────────────────────────────┐ │
│  │ DownloadTask (one per download — state machine)                  │ │
│  │   Probe ──► Plan(RangeSet + Claims) ──► Governor ◄── Metrics     │ │
│  │                   │ claim                 │ spawn/shrink/kill    │ │
│  │        ┌──────────▼──────────────────────▼──────────┐            │ │
│  │        │ Worker 1 │ Worker 2 │ … │ Worker k          │            │ │
│  │        │  each owns a reqwest::Client ⇒ own TCP conn │            │ │
│  │        └────────────────┬─────────────────────────────┘           │ │
│  │            bounded mpsc │ (offset, Bytes) — cap 256 ≈ 16 MB       │ │
│  │        ┌────────────────▼─────────────────────────────┐           │ │
│  │        │ Writer (single task, owns the file)          │           │ │
│  │        │ coalesce → seek_write → sync_data →          │           │ │
│  │        │ commit checkpoint (one row, one txn)         │           │ │
│  │        └──────────────────────────────────────────────┘           │ │
│  └──────────────────────────────────────────────────────────────────┘ │
└──────────────────────────┬────────────────────────────────────────────┘
              reqwest / hyper / rustls (+ Windows cert store) ▼ Network
```

### B.1 Three decisions worth defending

**1. One writer task per download, not per-worker writes.** All workers send `(offset, Bytes)` over
a *bounded* channel to a single writer doing positional writes (`FileExt::seek_write`). This buys:

- **Backpressure for free** — slow disk fills the channel, workers block on `send().await`, memory
  stays bounded at ~16 MB regardless of connection count. It also gives the Governor a signal:
  *sustained writer backpressure means we're disk-bound and more connections cannot help.*
- **One owner of durability ordering** — data-flush-then-checkpoint happens in one task with no
  cross-task race. This is what makes crash recovery correct rather than hopeful.
- **Write coalescing** — adjacent chunks merge into larger sequential writes; matters a lot on
  spinning disks and under AV real-time scanning.

Cost: one channel hop per 64 KB (~2,000/sec at 125 MB/s — immeasurable), and `Bytes` is refcounted
so there's no copy.

**2. Claim-based ranges with cooperative shrink-and-steal.**
*Fixed N-way split* (classic IDM) is straggler-bound — the slowest connection sets the finish time.
*Fixed-size chunk queue* load-balances well but costs one request RTT per chunk (~7% at 4 MB /
10 MB/s / 30 ms RTT, worse on high-RTT paths — exactly where we want to win).
**Claims** get both: each worker holds a mutable `[start, end)` and streams it in one long-lived
request; when a worker goes idle the coordinator **lowers another worker's `end`** and hands the
freed tail over. The shrunk worker stops naturally at its next chunk boundary. Extra requests occur
*only* on a steal.

**3. One global "probe token."** Only one download app-wide may be in a concurrency-ramp phase at a
time. Without this, three downloads ramping on a shared link each read the others' growth as their
own plateau and oscillate against each other forever.

### B.2 UI ↔ engine contract

Commands (Tauri `invoke`, all `Result<T, AppError>`):
`add_download` · `probe_url` · `pause`/`resume`/`cancel`/`remove(id, delete_file)` · `retry` ·
`set_priority`/`start_now` · `list_downloads(filter)` · `get_details` ·
`subscribe_connections`/`unsubscribe_connections` · `get_settings`/`set_settings` ·
`choose_folder` · `reveal_in_explorer` · `open_file(id)`

URL-refresh commands (§D.10) — deliberately a two-step flow so the UI can show evidence before
anything is mutated:

```text
validate_replacement_url(id, url) -> ValidationReport   # read-only: probes + verifies, mutates nothing
commit_replacement_url(id, url, source)                 # swaps the URL and resumes
                                                        # source ∈ {user, browser_extension, auto_reresolve}
find_existing_for(probe: ProbeResult) -> Vec<DownloadSummary>   # duplicate detection (§D.10.7)
get_url_history(id) -> Vec<UrlHistoryEntry>             # redacted by default
reveal_full_url(id) -> String                           # explicit user action; audited in `events`
```

Events (engine → UI):
- `progress-tick` — **4 Hz, ONE event carrying a `Vec<ProgressSnapshot>` for all active downloads**
- `state-changed` — low frequency: status transitions, completion, failure
- `connections` — ~2 Hz, **only while a Details drawer is subscribed**
- `notice` — needs user action: URL expired, file changed, disk full, overwrite prompt

**Non-negotiable:** the engine never emits a UI event per chunk. High-frequency IPC into a WebView
is the single most common way to make a Tauri app burn 30% CPU doing nothing. The `EventPump` holds
a dirty-set and flushes on a 250 ms tick.

### B.3 Forward-compat hooks (≈zero cost now, save a rewrite later)

Designed in during Session 1, exposed later:

- `RequestSpec { headers, cookies, referer, user_agent, proxy }` threads through Probe and Worker
  from day one → browser-extension integration, authenticated downloads, custom headers and proxy
  need **no engine change**, only UI + a native-messaging host.
- A `RateLimiter` trait in the worker read loop, defaulting to a no-op `Unlimited` impl → speed
  limiter and bandwidth scheduler become a config change.
- The `Protocol` boundary (`probe` / `fetch_range`) is a trait; HTTP is one impl. A future
  torrent/metalink backend doesn't touch scheduler, store, or UI.

---

## C. Data model

SQLite, WAL, `synchronous = NORMAL`, schema versioned via `PRAGMA user_version` (forward-only
migrations). One writer connection owned by a dedicated store task; small read pool.

```sql
CREATE TABLE downloads (
  id                TEXT PRIMARY KEY,           -- uuid v7 (time-sortable)
  -- ── URL model (§D.10): identity is `id`; URLs are revocable attributes of it ──
  original_url      TEXT NOT NULL,              -- the FIRST url the user gave. Never overwritten;
                                                --   used for auto re-resolution of signed links
  current_url       TEXT NOT NULL,              -- ★ the url actively fetched from. Replaced by a
                                                --   URL refresh; starts equal to original_url
  final_url         TEXT NOT NULL,              -- current_url after redirects (the segment target)
  url_refresh_count INTEGER NOT NULL DEFAULT 0,
  identity_hint     TEXT NOT NULL,              -- sha256(lower(filename) || ':' || total_size)
                                                --   ONLY for duplicate detection (§D.10.7).
                                                --   NEVER treated as proof of identity.
  validation_state  TEXT NOT NULL DEFAULT 'not_required',
                                                -- not_required | auto_verified | content_verified
                                                -- | user_confirmed | rejected
  verified_windows  BLOB NOT NULL DEFAULT '',   -- RangeSet of byte windows content-verified against
                                                --   a replacement URL; accumulates across refreshes
  filename          TEXT NOT NULL,              -- sanitized
  dest_dir          TEXT NOT NULL,
  part_path         TEXT NOT NULL,              -- <dest>/<name>.slpart
  category          TEXT NOT NULL DEFAULT 'other',

  total_size        INTEGER,                    -- NULL = unknown (no Content-Length)
  bytes_done        INTEGER NOT NULL DEFAULT 0, -- denormalized cache of completed_ranges total
  completed_ranges  BLOB NOT NULL DEFAULT '',   -- ★ THE resume state (see C.1)

  status            TEXT NOT NULL,              -- queued|probing|active|paused|retrying|
                                                -- waiting_network|needs_attention|completed|
                                                -- failed|cancelled
  accept_ranges     INTEGER NOT NULL DEFAULT 0, -- 0 unknown, 1 verified yes, 2 verified no, 3 LIAR
  etag              TEXT,
  last_modified     TEXT,
  content_type      TEXT,
  http_version      TEXT,

  queue_position    INTEGER NOT NULL DEFAULT 0,
  max_connections   INTEGER,                    -- NULL = auto (adaptive)

  avg_speed_bps     INTEGER NOT NULL DEFAULT 0,
  peak_speed_bps    INTEGER NOT NULL DEFAULT 0, -- max over 1-second windows (C.3)
  active_seconds    INTEGER NOT NULL DEFAULT 0, -- excludes paused/queued time
  peak_connections  INTEGER NOT NULL DEFAULT 0,
  retry_count       INTEGER NOT NULL DEFAULT 0,
  conn_failures     INTEGER NOT NULL DEFAULT 0,

  sha256            TEXT,
  expected_sha256   TEXT,                       -- optional, user-supplied
  integrity_state   TEXT NOT NULL DEFAULT 'unverified',

  created_at INTEGER NOT NULL, started_at INTEGER, completed_at INTEGER,
  error_code TEXT, error_message TEXT,
  clean_shutdown    INTEGER NOT NULL DEFAULT 0  -- ★ 0 on load ⇒ we crashed (D.7)
);
CREATE INDEX idx_dl_status   ON downloads(status, queue_position);
CREATE INDEX idx_dl_created  ON downloads(created_at DESC);
CREATE INDEX idx_dl_identity ON downloads(identity_hint);   -- duplicate detection lookup

-- ── url_history ─── append-only chain of URLs for one logical download (§D.10.6) ──
-- ★ PRIVACY: `url_redacted` only. Signed URLs are bearer credentials; the history table,
--   the events ring, every log line and the Details tab all use the redacted form (§D.10.8).
CREATE TABLE url_history (
  download_id  TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
  seq          INTEGER NOT NULL,              -- 0 = original_url
  url_redacted TEXT NOT NULL,                 -- scheme://host/path?token=<redacted>&expires=<redacted>
  host         TEXT NOT NULL,
  source       TEXT NOT NULL,                 -- user | browser_extension | auto_reresolve
  outcome      TEXT NOT NULL,                 -- accepted_auto | accepted_confirmed | rejected_size
                                              -- | rejected_content | rejected_no_range | expired
  -- validator snapshot AT THE TIME this URL was accepted — the evidence trail
  etag TEXT, last_modified TEXT, total_size INTEGER, accept_ranges INTEGER,
  content_type TEXT, disposition_filename TEXT,
  bytes_done_at_swap INTEGER NOT NULL,        -- what we preserved — the point of the feature
  added_at     INTEGER NOT NULL,
  PRIMARY KEY (download_id, seq)
);
-- capped at the newest 20 entries per download by a trigger

-- learned server behaviour — makes the SECOND download from a host smart
CREATE TABLE host_profiles (
  host TEXT PRIMARY KEY,
  supports_range INTEGER NOT NULL DEFAULT 0,
  best_observed_conns INTEGER, best_observed_bps INTEGER,
  per_conn_cap_bps INTEGER,                    -- inferred per-connection throttle
  saturation_detected INTEGER NOT NULL DEFAULT 0,
  last_429_at INTEGER, conn_limit_hint INTEGER,
  samples INTEGER NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL
);

-- bounded diagnostics ring → Details ▸ Timeline tab (trigger trims to newest 500 per download)
CREATE TABLE events (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  download_id TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
  ts INTEGER NOT NULL, level TEXT NOT NULL,
  kind TEXT NOT NULL,   -- probe|redirect|conn_open|conn_fail|retry|concurrency_change|
                        -- throttle_detected|stall|checkpoint
  detail TEXT NOT NULL  -- JSON
);
CREATE INDEX idx_ev_dl ON events(download_id, id DESC);

CREATE TABLE settings   (id INTEGER PRIMARY KEY CHECK (id=1), json TEXT NOT NULL);
CREATE TABLE categories (name TEXT PRIMARY KEY, extensions TEXT NOT NULL, dest_dir TEXT);

CREATE TABLE benchmark_runs (            -- Session 3
  run_id TEXT, scenario TEXT, conns TEXT, rep INTEGER,
  bytes INTEGER, wall_ms INTEGER, mean_bps INTEGER, peak_bps INTEGER, ttfb_ms INTEGER,
  retries INTEGER, conn_failures INTEGER, cpu_ms INTEGER, peak_rss_bytes INTEGER,
  git_sha TEXT, machine TEXT, ts INTEGER,
  PRIMARY KEY (run_id, scenario, conns, rep)
);
```

### C.1 Why segment state is a BLOB, not a `segments` table

The requirement says "persist segment states." The naive reading — a row per segment, updated as it
advances — is the wrong implementation:

- Segments are **not stable entities** under shrink-and-steal. They split, merge, and get
  reassigned constantly; rows would thrash.
- What must survive a crash is one fact: **which byte ranges are durably on disk.** That's a
  `RangeSet` — sorted, non-overlapping `(start, len)` pairs — which varint-delta-encodes to a few
  dozen bytes even for a heavily fragmented multi-GB download.
- Storing it as **one column in one row** makes the checkpoint a **single-row UPDATE in a single
  transaction** — atomic by construction. No multi-row consistency problem, no partial-checkpoint
  state for recovery to reason about.

Live per-connection state (claim, instantaneous speed, retries) is deliberately **in-memory only** —
it's meaningless after a restart, and the UI's connection table reads from memory, not the DB.

### C.2 `RangeSet` — where correctness lives

Heaviest unit tests in the project, including `proptest` properties: insert-in-any-order yields the
same canonical set; `total()` equals the sum of lengths; no overlaps; no unmerged adjacent pairs.

```rust
pub struct RangeSet { spans: Vec<(u64, u64)> }   // sorted, disjoint, non-adjacent
impl RangeSet {
    fn insert(&mut self, start: u64, len: u64);   // merges neighbours
    fn total(&self) -> u64;
    fn first_gap_at_or_after(&self, pos: u64) -> Option<(u64, Option<u64>)>;
    fn gaps_in(&self, start: u64, end: u64) -> impl Iterator<Item=(u64,u64)>;
    fn contains_all(&self, start: u64, end: u64) -> bool;
    fn encode(&self) -> Vec<u8>;                  // varint deltas
    fn decode(b: &[u8]) -> Result<Self>;
    fn rewind_tails(&mut self, margin: u64);      // paranoid crash recovery (D.7)
}
```

### C.3 Metric definitions ("peak speed" is otherwise meaningless)

An undefined peak is just the biggest noise spike you happened to sample. We define:

- **Current** — EWMA over ~3 s (readable, not jittery).
- **Average** — `bytes_done / active_seconds`, excluding paused/queued/waiting-network time. That's
  the number users actually mean.
- **Peak** — max of **non-overlapping 1-second window averages**, labelled "Peak (1s)" in the UI.
  Instantaneous burst rates are not reported; they measure the OS receive buffer, not the network.

---

## D. Download algorithm

### D.1 Probe — metadata detection

**Use a 1-byte ranged GET, not HEAD.** HEAD is unreliable in the field: some servers 405, some
return a `Content-Length` that differs from the GET body, many CDNs don't reflect `Accept-Ranges` on
HEAD, and signed-URL gateways sometimes reject it outright.

```text
PROBE(url, request_spec):
 1. Validate scheme ∈ {http, https}. Reject file:, data:, ftp:, blob:, javascript:.
 2. GET url
      Range: bytes=0-0
      Accept-Encoding: identity          ← ★ critical, see step 5
      User-Agent: SwiftLoad/<ver> (+https://github.com/…)
    Manual redirect handling, max 10 hops:
      • http/https targets only
      • https→http downgrade REFUSED unless settings.allow_insecure_redirect
      • strip Authorization/Cookie on cross-origin hops
      • record the full chain for Details ▸ Server
 3. Interpret:
      206 + Content-Range: bytes 0-0/N  ⇒ ranges OK, total = N            [best case]
      206 + Content-Range: bytes 0-0/*  ⇒ ranges OK, total unknown
      200 + Content-Length: N           ⇒ server IGNORED Range ⇒ no ranges, total = N
                                          (abort the body immediately — don't drain 4 GB)
      200 + no Content-Length           ⇒ no ranges, unknown size, single stream
      416                               ⇒ zero-length or broken server → retry plain GET
      401/403/404/410/451               ⇒ terminal; classify and surface
      429/5xx                           ⇒ retryable; honour Retry-After
 4. Capture ETag, Last-Modified, Content-Type, Content-Disposition, Content-Encoding,
    Accept-Ranges, final URL, negotiated HTTP version, TLS peer info.
 5. ★ Content-Encoding trap: if the response carries a non-identity Content-Encoding despite
    our identity request, byte ranges address the COMPRESSED stream while the client
    decompresses transparently — offsets become meaningless and the file silently corrupts.
    Response: force single-connection mode, disable Range, do not treat Content-Length as the
    on-disk size. Explicit regression test.
 6. Resolve filename (D.8); check free disk space (size + 100 MB margin).
 7. Upsert host_profiles by host; consult it for starting concurrency.
```

### D.2 ★ The HTTP/2 trap — highest-value detail in this document

Build one `reqwest::Client`, issue 16 concurrent ranged GETs, server negotiates **h2** → hyper
multiplexes all 16 as **streams over one TCP connection**. You now have one congestion window, one
loss-recovery domain, and 64 KB default flow-control windows. That is **strictly worse than a
single-connection downloader**: all the coordination overhead, none of the parallelism. Every
benchmark shows a flat line that looks like server saturation. Most large CDNs serve h2 by default,
so this is the *common* case.

**Mitigation, from the first commit:**

1. **One `reqwest::Client` per worker** → separate pools → guaranteed distinct TCP connections even
   under h2. (A Client is just a handle around a pool; this is cheap.)
2. `.pool_max_idle_per_host(1)` — keeps the conn alive across chunk/steal boundaries, never
   accumulates.
3. Raise h2 windows: `http2_initial_stream_window_size(4 MB)`,
   `http2_initial_connection_window_size(8 MB)`, `http2_adaptive_window(true)`.
4. `.tcp_nodelay(true)`; connect timeout 15 s; **read-idle** timeout 20 s (a whole-body timeout on
   a 4 GB download is a bug).
5. `.redirect(Policy::none())` — redirects handled explicitly so downgrade/cross-origin rules apply.
6. **Assert it in tests.** The test server counts distinct TCP accepts; `distinct_connections.rs`
   fails if 16 workers produce fewer than 16 connections. This bug silently regresses and only
   surfaces later as "why aren't we faster?"

### D.3 Initial concurrency selection

```text
CHOOSE_K0(size, host_profile, settings):
  if not range_capable  -> 1        (never re-evaluate)
  if size unknown       -> 1        (can't plan; may upgrade if a size appears)
  if size < 4 MiB       -> 1        (handshakes cost more than they save)
  if size < 32 MiB      -> 2

  k_by_size    = clamp(size / MIN_SEGMENT(2 MiB), 1, settings.max_conns_per_download)
  k_by_profile = host_profile.best_observed_conns  (if samples >= 3 and fresh)
  k_by_policy  = min(settings.max_conns_per_download,
                     host_budget_remaining(host),    -- per-host cap, default 8
                     global_conn_budget_remaining()) -- default 24 across all downloads

  k0 = min(k_by_size, k_by_policy, k_by_profile ?? 4)
  if host_profile.saturation_detected      -> k0 = min(k0, host_profile.best_observed_conns)
  if host_profile.last_429_at within 10min -> k0 = min(k0, 2)
  return max(k0, 1)
```

Default start of **4**, not 1 and not 16: starting at 1 wastes seconds re-learning what we usually
know; starting at 16 is antisocial and often triggers throttling before we've measured anything.

### D.4 Segmentation — claims, shrink, steal

```text
PLAN:
  claims = split [0, size) into k0 equal parts, skipping ranges already in completed_ranges
           (on resume, plan over the GAPS, not over [0,size))

WORKER LOOP (worker w):
  loop:
    if cursor >= end: request a new claim; if none, exit cleanly
    GET final_url  Range: bytes=<cursor>-<end-1>  Accept-Encoding: identity
    ★ VERIFY BEFORE WRITING A SINGLE BYTE:
        status must be 206; Content-Range start == cursor;
        Content-Range total (if present) == total_size
      Anything else ⇒ raise RangeLied.
      (If cursor==0 and status==200 the server is simply serving the whole file →
       degrade to single-stream rather than fail.)
    stream 64 KiB chunks:
        send (offset, Bytes) to writer, awaiting the bounded channel (backpressure)
        cursor += n; feed metrics; check the shrink flag each chunk
        if cursor >= end (possibly lowered by a steal): break → new claim

STEAL (coordinator, when a worker idles and no unclaimed gap remains):
  victim = worker with max(remaining = end - cursor)
  if victim.remaining < 2 * MIN_SEGMENT: no steal (avoids thrash)
  split_at = victim.cursor + remaining/2, rounded up to 1 MiB
  victim.end = split_at            (atomic store; victim notices on its next chunk)
  idle worker's claim = [split_at, old_victim_end)

STRAGGLER HANDLING (every 5 s) — this is what beats a naive segmented downloader:
  med = median per-worker throughput over the last 5 s
  if w.bps < max(0.25*med, 32 KiB/s) and w's remaining would exceed the download ETA:
      shrink w.end to what it can finish by the projected ETA; redistribute the tail  [preferred]
  if w.bps == 0 for 20 s:
      hard-kill and respawn as a NEW TCP connection (frequently lands on a different CDN
      edge / clears a bad path); cap respawns at 3 per worker per minute
```

### D.5 ★ Dynamic concurrency — the Governor

Core insight: **a throughput plateau has several possible causes demanding opposite responses**, and
aggregate throughput alone cannot tell them apart. So the Governor tracks both `T(k)` and
**per-connection throughput `T(k)/k`**, and uses their relationship as the discriminator:

| Observation on k → 2k | Diagnosis | Action |
|---|---|---|
| `T(2k) ≈ 2·T(k)` (per-conn flat) | **per-connection cap** — parallelism paying linearly | keep ramping |
| `T(2k) ≈ T(k)` (per-conn halves) | **saturated** — link or per-IP cap is the bottleneck | revert to k, stop, set `saturation_detected` |
| `T(2k) < T(k)` | server **penalising** concurrency (shaping/queueing/overload) | revert *below* k, set ceiling |
| 429 / 503 appear | rate limiting | halve k, honour Retry-After, host ceiling |
| writer channel persistently full | **disk-bound** | cap k; more connections are pure waste |

```text
GOVERNOR (per download; ticks 500 ms, acts only on phase boundaries)

Constants (settings-tunable; defaults fixed by Session 3 benchmarking):
  WARMUP_MS=400   DWELL_MS=1200   GAIN_THRESHOLD=0.15   REPROBE_MS=30_000
  K_MAX = min(settings.max_conns_per_download, 32)

PHASE A — BASELINE
  k := k0 (D.3); spawn; discard WARMUP_MS; measure T(k) over DWELL_MS.
  Requires the GLOBAL PROBE TOKEN to advance to Phase B; without it, hold at k0.

PHASE B — RAMP (holds the probe token)
  k_next := min(k*2, K_MAX, k_by_size, host_budget_remaining)
  if k_next == k: goto HOLD
  if remaining_bytes / k_next < MIN_SEGMENT: goto HOLD      # nothing left to split
  spawn (k_next - k) workers; discard WARMUP_MS; measure T(k_next) over DWELL_MS

  gain     := T(k_next)/T(k) - 1
  per_conn := (T(k_next)/k_next) / (T(k)/k)

  if gain >= GAIN_THRESHOLD:
      emit concurrency_change{from:k, to:k_next, gain, per_conn}; k := k_next; repeat B
  else:
      k_mid := (k + k_next)/2                    # try the midpoint before giving up
      if k_mid > k and untried: retire to k_mid; measure; keep the better of {k, k_mid}
      else: retire to k
      if per_conn <= 0.6: host_profile.saturation_detected := 1
      host_profile.best_observed_conns := k; release probe token; goto HOLD

PHASE C — HOLD
  Steady state. Re-probe (+2 conns, one DWELL) at most every REPROBE_MS, and only if:
    no errors in 30 s AND no 429/503 ever seen on this host this session AND
    estimated remaining time > 20 s AND the probe token is free.
  Accept only on >=10% gain; else revert and DOUBLE REPROBE_MS (a stable download stops
  probing rather than nagging the server forever).

PHASE D — BACK OFF (pre-empts everything, no token needed)
  429/503: k := max(1, k/2); ceiling := k; honour Retry-After (cap 120 s);
           host_profile.last_429_at := now; conn_limit_hint := k
  reset/timeout rate > 2 per connection-minute: k := max(1, k-1); backoff that worker
  aggregate T drops >30% within 5 s of a spawn: revert that spawn immediately; ceiling := k
  writer backpressure sustained > 3 s: ceiling := k; emit throttle_detected{reason:"disk"}

RETIREMENT — never kill mid-claim. Set retiring=true; the worker finishes its claim and exits
  without requesting another; any remaining range returns to the pool.

ENDGAME — when remaining < k*MIN_SEGMENT stop stealing and let workers drain; below 2 MiB
  retire to 2 workers. Prevents a swarm fighting over the last few hundred KB (classic tail
  latency source).
```

**Why 15%, not "any improvement":** noise on a real network over a 1.2 s window is easily ±10%. A
lower threshold makes the Governor chase noise. Settled empirically in Session 3 against
`/throttle` scenarios where ground truth is known.

**The Governor is a pure function** — `fn decide(&GovernorState, &Sample) -> Decision` — so it is
unit-tested against **synthetic throughput traces** (per-conn-capped, saturated, throttling-server,
flaky) with no network at all. That's the difference between "we think the adaptive logic works" and
20 tests proving what it does in each regime.

### D.6 Retry policy and error classification

```rust
enum ErrorClass {
    TransientNetwork,   // reset, timeout, DNS temp fail, TLS handshake timeout
    TransientServer,    // 500,502,503,504,408,425
    RateLimited,        // 429, or 503 + Retry-After → ALSO reduce concurrency
    UrlExpired,         // 403 on a URL that previously worked → re-resolve from original_url
    Fatal,              // 401,404,410,451, 403-on-first-try
    RangeUnsupported,   // 200 where 206 expected, at start
    RangeLied,          // advertised bytes, then ignored Range mid-download
    ResourceChanged,    // If-Range failed / 416 / Content-Length changed / ETag changed
    LocalFatal,         // ENOSPC, access denied, path too long, invalid filename
    NetworkDown,        // ALL workers failed with connect errors inside 5 s
}
```

- **Per-segment retry:** exponential backoff with **full jitter** (`sleep = rand(0, min(cap,
  base·2^n))`), base 500 ms, cap 30 s, max 8 attempts. Full jitter matters: plain exponential makes
  all k workers that failed together retry together in lockstep, forever.
- **RateLimited:** honour `Retry-After` (seconds or HTTP-date, capped 120 s) *and* halve
  concurrency. Never work around a rate limit by opening more connections — that's abuse, and it's
  slower anyway.
- **UrlExpired:** enters the §D.10.5 escalation ladder — retry `current_url`, then re-probe
  `original_url` (which is why we never overwrite it), then the extension, then `needs_attention`
  with the "Provide New URL" flow. The partial file is **always preserved**, and any replacement URL
  goes through the §D.10.4 decision matrix before a single byte is appended.
- **NetworkDown:** do **not** burn the retry budget. → `waiting_network`, tear down the (dead)
  sockets, poll with backoff 2 s → 60 s. Session 3 upgrades to Windows `INetworkListManager` COM
  connectivity events so a Wi-Fi reconnect resumes in ~1 s. Progress is checkpointed first.
- **RangeLied mid-download:** cancel all workers; keep **only the contiguous prefix from offset 0**
  (everything past the first gap is unverifiable); set `accept_ranges = 3`; resume single-stream
  from the end of that prefix; record it on the host profile so we never segment that host again.
- Download-level failure only after per-segment retries are exhausted **and** the error is
  non-transient. `retry_count` and the per-error histogram are persisted and shown in Details.

### D.7 Persistence, checkpointing, crash recovery

**The durability rule:** *a checkpoint may only claim ranges whose data is already durable.*
Violating it is exactly how a "resumed" download yields a corrupt file that passes a size check. So
the writer's cycle is strictly ordered:

```text
WRITER LOOP:
  recv (offset, bytes); coalesce adjacent pending chunks (up to 1 MiB)
  seek_write(buffer, offset)                    # positional, no shared cursor
  pending.insert(offset, len)

  every 8 MiB written OR every 5 s (whichever first):
      file.sync_data()                          # ★ data durable FIRST
      then ONE SQLite transaction:
          UPDATE downloads SET completed_ranges=?, bytes_done=?, avg_speed_bps=?,
                 peak_speed_bps=?, active_seconds=? WHERE id=?
  # ordering guarantee: a checkpoint never describes bytes that aren't on disk
```

`sync_data()` of 8 MiB already in page cache is single-digit ms on NVMe, tens on spinning rust —
negligible against silent corruption.

```text
ON STARTUP:
  read clean_shutdown; immediately reset it to 0 for all rows (so a crash right now is detected)
  for each download in {active, retrying, paused, waiting_network}:
    • .slpart missing            -> reset completed_ranges, restart from 0
    • .slpart shorter than the highest checkpointed offset -> drop ranges beyond real length
    • clean == 0 (crash / power loss):
          completed_ranges.rewind_tails(1 MiB)          # ★ paranoid recovery
      Rationale: fsync is honoured by essentially all modern drives, but consumer SSDs with
      volatile write caches and some virtualised storage have been observed to lie. Re-fetching
      ≤1 MiB per fragment is trivial insurance against a corruption class we could otherwise
      never detect. Setting: paranoid_recovery, default ON.
    • status becomes `queued` (not auto-`active`) unless settings.auto_resume_on_start

ON RESUME (before requesting any range):
  GET final_url  Range: bytes=0-0  If-Range: <etag ?? last_modified>
    206 -> validator matched + size matches -> resume, planning over the gaps
    200 -> resource CHANGED (If-Range failed) -> needs_attention; UI asks
           "restart from scratch" or "keep the existing partial file"
    416 -> resource shrank/vanished -> needs_attention
    403 -> UrlExpired; enter the §D.10.5 escalation ladder (re-resolve, then user-supplied URL)
  # the 64 KiB spot-check below is the Tier-1 mechanism generalized in §D.10.3
  If the server offers NO validators at all:
    compare Content-Length; if equal, SPOT-CHECK — re-fetch [0, 64 KiB) and compare against
    disk. Mismatch ⇒ treat as changed. Match ⇒ resume with a "could not fully verify" flag in
    Details. Cheap, and catches the common "same size, different build" case a size check misses.

ON CLEAN SHUTDOWN:
  pause all active, flush writers, sync_data, final checkpoint, SET clean_shutdown=1, exit.
  Also registered on WM_QUERYENDSESSION / WM_ENDSESSION so a Windows restart is a clean stop.
```

**Sparse preallocation.** `set_len(total_size)` at start — reserves space so disk-full fails
immediately rather than at 90%, and reduces NTFS fragmentation. Then mark sparse
(`FSCTL_SET_SPARSE`) so NTFS skips zero-filling multi-GB files. We deliberately do **not** use
`SetFileValidData` despite it being faster: it needs `SeManageVolumePrivilege` and can expose
previously-deleted disk contents inside the user's file. Real security trade-off; the perf gain
doesn't justify it.

### D.8 Filename resolution and sanitization

Priority: `Content-Disposition` `filename*` (RFC 5987, with charset) → `filename` → final-URL path
basename (percent-decoded) → `download` + extension guessed from `Content-Type`.

Then unconditionally — all of these have been real CVEs in download tools:

```text
1. Last path component only; strip every '/' and '\'.
2. Reject components equal to "." or ".."                → path traversal
3. Strip control chars (<0x20, 0x7F) and Unicode bidi overrides U+202A–202E, U+2066–2069
   → "invoice\u202Excod.exe" renders as "invoicexe.docx" — extension spoofing
4. Reject/replace Windows-invalid chars  < > : " | ? *
   ':' matters twice — it also creates an NTFS ALTERNATE DATA STREAM. "report.pdf:payload.exe"
   would write a hidden executable stream. Non-negotiable reject.
5. Strip trailing dots and spaces (Windows silently trims them, so our recorded name would not
   match the on-disk name — later breaking resume and delete).
6. Reject reserved device names with or without extension, case-insensitive:
   CON PRN AUX NUL COM1-9 LPT1-9   ("CON.txt" is still CON)
7. Truncate to 200 UTF-16 units, preserving the extension.
8. Enable long-path support; use the \\?\ prefix for full paths.
9. ★ FINAL BACKSTOP: canonicalize the resolved destination and assert
      canonical(dest).starts_with(canonical(configured_dir))
   Belt-and-braces against anything above that was missed. Fail closed.
10. Collision policy (setting): auto-rename "file (2).ext" [default] | overwrite | ask.
    Overwrite is never silent for a file we did not create.
```

### D.9 Completion and verification

```text
1. Assert completed_ranges covers [0, total_size) with NO gaps — check the RangeSet, not just
   the byte counter. A counter can be right while coverage is wrong; this has caught real bugs.
2. sync_all(); drop the handle.
3. Assert on-disk length == total_size.
4. If a Content-MD5 / Digest / Repr-Digest header was captured, verify it.
5. If expected_sha256 was supplied, verify. Mismatch ⇒ FAIL LOUDLY, keep as .slpart, never
   publish under the final name.
6. Else if settings.hash_on_complete: SHA-256 in one sequential pass (~1–2 GB/s), off the hot
   path, after the file is already usable.
7. Apply Mark-of-the-Web: write the :Zone.Identifier ADS (ZoneId=3 + HostUrl + ReferrerUrl) so
   SmartScreen and Office Protected View engage exactly as for a browser download.
   (Session 3: switch to IAttachmentExecute::Save, which additionally triggers a Defender scan.)
8. Atomic rename .slpart → final (MoveFileExW, same volume; REPLACE_EXISTING only when the user
   explicitly chose overwrite).
9. One transaction: status=completed, completed_at, final metrics; update host_profiles with the
   concurrency that measured best.
```

**On integrity, honestly:** without a server-supplied checksum, no download manager can prove the
bytes are what the publisher intended. What we *can* guarantee is that the assembled bytes are
exactly what the server sent this session — via full range coverage, exact size, `If-Range`
validator continuity, and the resume spot-check. `integrity_state` is reported truthfully as
`verified_checksum` / `verified_size_and_validators` / `unverified`, and we never show a green check
for the weaker states.

### D.10 ★ Resource identity and resume with a replaced URL

The 10 GB / 6.5 GB scenario: a signed URL expires, the user obtains a fresh link, and the 6.5 GB
already on disk must survive. **The governing priority is: never silently corrupt a large
download** — accept an occasional confirmation prompt as the cost.

#### D.10.0 What the plan already covered vs. what changed

| Already supported | Where | Verdict |
|---|---|---|
| Identity is a uuid `DownloadId`, **not** the URL | §C `downloads.id` | ✅ no change — the architecture was already URL-independent |
| Segment state keyed on the download, not the URL | §C.1 `completed_ranges` BLOB | ✅ no change — a URL swap cannot disturb it |
| `original_url` preserved separately from the resolved URL | §C, §D.6 | ⚠️ extended to a **three-URL model** (original / current / final) |
| `UrlExpired` class + auto re-resolve from `original_url` | §D.6 | ⚠️ extended — becomes step 2 of the escalation ladder (§D.10.5) |
| `needs_attention` status + a fresh-URL command | §B.2, §D.6 | ⚠️ extended — split into read-only `validate` + mutating `commit` |
| Resume plans over **gaps**, not over `[0,size)` | §D.7 | ✅ no change — this is exactly what a swapped URL needs |
| 64 KB prefix spot-check when validators are absent | §D.7 | ⚠️ **generalized** into the verification ladder (§D.10.3) |
| Range verification before writing any byte | §D.4 | ✅ no change — protects a replacement URL identically |

| Genuinely new | Added in |
|---|---|
| `current_url` distinct from original/final; `url_history` chain | §C schema |
| A **decision procedure** for identity (the plan had no answer for "ETag changed, size same") | §D.10.2 |
| Multi-window content verification with deterministic window placement | §D.10.3 |
| Capability re-verification + Governor/host-profile reset across a host change | §D.10.4 |
| Duplicate-download detection on add | §D.10.7 |
| Redaction of signed-URL secrets across DB, logs, events and UI | §D.10.8 |
| `validate_replacement_url` / `commit_replacement_url` / `find_existing_for` | §B.2 |

#### D.10.1 Which signals are trustworthy

| Signal | Strength | Honest assessment |
|---|---|---|
| **Byte-range content comparison** against local data | **Proof-grade** | The only signal that actually proves the bytes line up. Everything else is circumstantial. |
| Server digest (`Repr-Digest`, `Content-MD5`) | **Proof-grade** | Rare in practice; use it when present. |
| **Content-Length** | **Necessary, not sufficient** | Equality proves nothing (a decoy is trivial to size-match). **Inequality is near-conclusive disproof** — this is the hard gate. |
| Strong `ETag` equal | Strong positive | But **inequality is NOT disproof** — see §D.10.2. |
| Weak `ETag` (`W/"…"`) equal | Moderate | Semantically "equivalent", explicitly not byte-identical. Never sufficient alone. |
| `Last-Modified` equal | Moderate | Cheap, often stable across re-signings of the same object. 1-second granularity. |
| `Content-Disposition` filename | **Weak** | Coincidence and spoofing are both trivial. Never proof — as the requirement states. |
| `Content-Type` | **Weak** | Almost no entropy. |
| URL path basename | **Very weak** | Diagnostic only. |

#### D.10.2 ★ The ETag-changed case — why mismatch must not auto-reject

`ETag` old `ABC` → new `XYZ`, `Content-Length` identical. **This is the common case, not the
suspicious one.** An ETag legitimately changes with identical content when: the new link resolves
to a **different CDN edge or storage backend** (ETags are per-node on many CDNs); the object was
re-uploaded byte-identically (S3 multipart ETags depend on part size, not just content); or the
server derives the ETag from inode/mtime.

So the rule is asymmetric:

- **ETag equal** → strong positive evidence.
- **ETag different** → *no* evidence either way. It downgrades confidence and **escalates to content
  verification**; it never rejects on its own.
- **Content-Length different (both known)** → hard reject. This is the one header that gates.

#### D.10.3 The verification ladder — fast vs. strong validation

Run the cheapest tier that reaches sufficient confidence.

| Tier | Cost on a 10 GB / 6.5 GB-done download | What it proves |
|---|---|---|
| **0 — Header agreement** | 0 extra bytes (the probe already happened) | Size matches **and** (strong ETag matches **or** Last-Modified matches). Circumstantial but strong. **Note (revised in implementation):** Tier 0 now means *no prompt is needed*, not *no verification is needed* — Tier 1 runs regardless. See the box below. |
| **1 — Anchored spot-check** ★ default | **4 × 64 KB = 256 KB ≈ 0.0025% of the file, < 2 s** | Server bytes match local bytes at 4 independent windows inside the already-completed region. Catches every realistic mismatch. |
| **2 — Full re-verification** | re-reads all 6.5 GB (65% of a restart) | Byte-identity of the whole completed region. Opt-in only. |
| **3 — Deferred whole-file hash** | 0 up front; whole-file read at the end | Requires a user-supplied `expected_sha256`. Resume immediately, verify at completion, never publish a bad file. |

**Tier 1 window placement** — 4 windows of 64 KB, all inside `completed_ranges`:
1. bytes `[0, 64 KB)` — catches a wholly different file instantly.
2. The **last 64 KB of the highest completed range** — where an off-by-one in range accounting or a
   size-preserving content difference is most likely to surface.
3. & 4. Two offsets from `hash(download_id)` mapped into the completed set — **deterministic per
   download** (a retried validation checks the same windows, so results are reproducible) but
   **unpredictable across downloads** (a server cannot pre-position decoy bytes).

Each window is fetched with `Range: bytes=s-e` and compared against `seek_read` of the local
`.slpart`. Every window must match.

**The trade-off, stated plainly:** Tier 1 costs ~0.003% of the transfer and reduces the residual
risk to an adversary who holds both the real file and the ability to serve matched decoy bytes at
unpredictable offsets. Tier 2 spends 65% of a full re-download to close a gap Tier 1 has already
made negligible. **Tier 1 is the default**; Tier 2 is an explicit "Verify thoroughly" button;
Tier 3 is strictly better than either whenever a checksum is available.

> **Revised during implementation: Tier 1 always runs when there is local data to verify.**
> The approved plan let a Tier-0 header match skip content verification entirely. Testing the
> corrupted-partial case (§G.4 test 10) showed why that is wrong: Tier 0 proves the *server* is
> serving the same resource, but says nothing about the bytes already on *our* disk. A partial
> damaged by a bad sector, a truncated write, or an unrelated process sails straight through a
> header comparison and gets resumed over — producing exactly the right-sized, wrong-content
> file this whole mechanism exists to prevent. At ~256 KB (0.003% of a 10 GB download) there is
> no case for skipping it. Tier 0 now governs whether the user is *prompted*, not whether the
> bytes are *checked*.

> **A Tier-1 mismatch is a hard reject, never a confirmable warning.** Letting a user click past a
> proven byte mismatch is precisely how a 10 GB file gets corrupted. On mismatch the message stays
> neutral about blame, because we genuinely cannot tell which side is wrong: *"The data already on
> disk doesn't match this link. Either this is a different file, or the existing partial download is
> damaged."* Offer **Start over** / **Cancel** — never **Resume anyway**.

#### D.10.4 Decision matrix — safe / confirm / unsafe

```text
INPUT: local {total_size, etag, last_modified, completed_ranges, bytes_done}
       probe of the replacement URL (a full §D.1 probe — never trust the old capabilities)

HARD REJECT (no prompt, offer "start a new download" instead)
  both sizes known AND differ                        -> rejected_size
  Tier-1 content comparison fails at ANY window      -> rejected_content
  replacement resolves to a non-http(s) scheme       -> rejected_scheme

AUTO-RESUME  (validation_state = auto_verified)
  size equal AND strong ETag equal
  size equal AND Last-Modified equal AND ETag absent on BOTH sides

CONTENT-VERIFY then AUTO-RESUME  (validation_state = content_verified)
  size equal AND Last-Modified equal AND exactly one side has an ETag   -> Tier 1
  size equal AND weak ETag equal                                        -> Tier 1

CONTENT-VERIFY then ASK  (validation_state = user_confirmed)
  size equal AND ETag DIFFERS          -> Tier 1; on pass, show evidence and ask   [§D.10.2]
  size equal AND no validators at all  -> Tier 1; on pass, show evidence and ask
  size unknown on the replacement URL  -> Tier 1 over what we can address; ask

SPECIAL — replacement URL does NOT support Range (see §D.10.5)
```

On a passing Tier 1 where the ETag rotated, we **adopt the new validator** (`etag`,
`last_modified`) as the basis for future `If-Range` checks, extend `verified_windows`, and record a
`validator_rotated` event. Without this, every subsequent pause/resume would re-prompt.

#### D.10.5 Escalation ladder, and the no-Range replacement

When `current_url` starts failing with 403/410, escalate in cost order — the user is only involved
at the last step:

```text
1. Retry current_url once after backoff            (403s are sometimes transient)
2. Re-probe original_url                            (many sites re-sign on a fresh request)
     -> if it yields a working URL: run §D.10.4. Tier-0 pass swaps SILENTLY, no prompt.
3. Ask the browser extension, if installed          (roadmap — same commit_replacement_url path)
4. status = needs_attention -> UI offers "Provide New URL"   (MVP)
```

**Replacement URL without Range support.** The old URL supported ranges; the new one doesn't. We
cannot request `[6.5 GB, 10 GB)` — so there is **no way to save the bytes over the wire**, and
pretending otherwise would be dishonest. Behaviour:

- **Never delete the partial file.** The user may find a resumable link later.
- Tell the truth in plain language: *"This link doesn't support resuming. Using it means downloading
  all 10 GB again."* Options: **Keep waiting for a resumable link** (default — stays
  `needs_attention`, partial preserved) / **Restart with this link** / **Cancel**.
- If the user restarts, stream from 0 and **verify-as-we-go** against `completed_ranges` rather than
  blindly overwriting, so a divergence aborts early instead of at 100%.

Other capability changes on the replacement URL, all **re-derived from a fresh §D.1 probe** — the
old values are never carried over:

| Change | Handling |
|---|---|
| Redirect chain differs | Fine — expected for signed links. Re-run downgrade and cross-origin rules. |
| **Host differs** (CDN change) | Fine. Switch `host_profiles` lookup to the new host and **reset the Governor to Phase A** — the new edge's behaviour is unknown, so the old concurrency conclusion doesn't transfer. |
| Content-Type / disposition filename differ | Recorded, shown as evidence; **never** decisive on their own. Keep the original on-disk filename — the user already sees it. |
| New URL now requires auth | Probe returns 401 → `rejected_auth`; MVP surfaces it plainly (custom headers/cookies are roadmap, §L). |
| Different connection limits / 429 behaviour | Governor re-learns from scratch; per-host caps apply to the new host. |
| Content-Encoding non-identity (§D.1.5) | Forces single-stream, same as any download. |

#### D.10.6 The swap itself — what is preserved and what resets

```text
COMMIT_REPLACEMENT_URL(id, url, source):   # ONE SQLite transaction
  PRESERVE (the entire point of the feature):
    completed_ranges, bytes_done, verified_windows, the .slpart file itself,
    filename, dest_dir, category, created_at, retry_count, conn_failures,
    avg_speed/peak_speed/active_seconds       (lifetime stats stay meaningful)
  UPDATE:
    current_url := url ; final_url := <resolved> ; url_refresh_count += 1
    etag, last_modified, total_size, accept_ranges, content_type := from the NEW probe
    validation_state := <outcome> ; append a url_history row (redacted)
  RESET (transient, and specific to the old endpoint):
    all worker/claim state, the Governor -> Phase A, the host-profile binding
  THEN: re-plan over the GAPS in completed_ranges (§D.7 resume path — unchanged) and resume.

NEVER: create a new download id, delete the .slpart, reset completed_ranges, or start at byte 0.
```

Because `Plan` already plans over gaps and every worker already verifies `206` +
`Content-Range` start before writing (§D.4), a replacement URL that misbehaves mid-download is
caught by the *existing* machinery — no new safety code on the hot path.

#### D.10.7 Duplicate detection on add

The user pastes a fresh link for a file that already has a partial download. On `add_download`,
after the probe we already run for the preview:

```text
candidates = SELECT * FROM downloads
             WHERE status IN (paused, failed, needs_attention, waiting_network)
               AND (identity_hint = <probe hint> OR (total_size = <probed> AND etag = <probed>))
```

If any match, show the "Existing Download Found" dialog. **Choosing "Resume Existing" routes into
the exact same §D.10.4 validation pipeline** — the prompt is a shortcut into
`validate_replacement_url`, not a parallel code path, so a duplicate can never bypass verification.
"Start New" creates a separate download with a de-duplicated filename.

#### D.10.8 Security and privacy of signed URLs

A signed URL **is a bearer credential**. Treating it as ordinary metadata would be a real
vulnerability.

- **Stored in full** (unavoidable — we must fetch with them): `current_url`, `final_url`,
  `original_url`. They live in the SQLite file under `%LOCALAPPDATA%\SwiftLoad\`, protected by
  default user-only ACLs.
- **Stored redacted, always**: `url_history.url_redacted`, the `events` ring, every log line, crash
  output, benchmark result files, and the Details ▸ Server tab.
- **Redaction is default-deny**: keep scheme, host, path and query *parameter names*; replace every
  query *value* with `<redacted>`, and strip `userinfo` from the authority. Denylisting known token
  names (`X-Amz-Signature`, `sig`, `token`, …) is not enough — the next provider invents a new one.
  Names alone are plenty for diagnostics.
- `reveal_full_url(id)` is an explicit user action (a "Copy full link" button), and it writes an
  `events` row so the disclosure is auditable.
- **No DB encryption in MVP.** Encrypting the SQLite file without a key-management story means
  storing the key beside the ciphertext — security theatre. We instead document plainly that signed
  URLs are stored locally and rely on the OS (BitLocker + user ACLs). SQLCipher plus an OS-keychain
  key is a roadmap item (§L), listed honestly rather than half-built.
- Future `RequestSpec` cookies/auth headers inherit exactly these rules.

#### D.10.9 Performance implication

The feature's entire purpose is to *avoid* re-transferring data, so validation traffic is held to a
hard budget: **Tier-1 validation must cost < 1 MB and < 2 s regardless of file size.** For the 10 GB
/ 6.5 GB case the wire cost is the remaining 3.5 GB plus 256 KB — i.e. **0.007% overhead**. This is
measured, not asserted: §H.2 adds a `url_refresh` benchmark scenario and §J adds the threshold as a
release gate.

---

## E. Project structure

```text
swiftload/
├─ Cargo.toml                       # workspace: core, testserver, cli, bench, app/src-tauri
├─ rust-toolchain.toml              # pinned stable
├─ deny.toml                        # cargo-deny: licences + FORBID tauri deps inside core
├─ README.md                        # benchmark table + its caveats
│
├─ crates/
│  ├─ swiftload-core/               # ★ the engine. ZERO UI deps (CI-enforced)
│  │  ├─ src/
│  │  │  ├─ lib.rs  config.rs  events.rs   # events.rs IS the UI contract
│  │  │  ├─ manager.rs                     # registry, command loop, event fan-out
│  │  │  ├─ scheduler.rs                   # queue, N-concurrent, host budget, probe token
│  │  │  ├─ task/{mod,probe,plan,worker,governor,writer}.rs
│  │  │  ├─ task/identity.rs                # ★ §D.10 verification ladder + decision matrix
│  │  │  ├─ http/{client,redirect,headers,errors}.rs
│  │  │  ├─ store/{mod,schema,migrations,models}.rs
│  │  │  ├─ fsx/{prealloc,atomic,motw,paths}.rs      # Windows file layer
│  │  │  └─ util/{intervals,filename,rate,backoff,redact}.rs   # redact.rs = §D.10.8
│  │  └─ tests/                             # all against swiftload-testserver
│  │     ├─ range_support.rs  liar_server.rs  no_content_length.rs
│  │     ├─ resume.rs  crash_recovery.rs  redirects.rs  rate_limit.rs
│  │     ├─ url_refresh.rs                  # ★ §G.4 tests 1,4,5,6,7,8,9,10
│  │     ├─ distinct_connections.rs         # ★ k workers ⇒ k TCP conns (the h2 trap)
│  │     └─ integrity.rs                    # byte-exact out-of-order assembly
│  ├─ swiftload-testserver/  src/{main,content,throttle,scenarios,connstats}.rs
│  ├─ swiftload-cli/         src/main.rs    # get/resume/list/verify/inspect — S1's only "UI"
│  └─ swiftload-bench/       src/{main,matrix,sysmetrics,report}.rs
│
├─ app/
│  ├─ src-tauri/  src/{main,commands,event_pump,tray,autostart,single_instance}.rs
│  │              tauri.conf.json (nsis + msi, CSP, capability allowlist), icons/
│  └─ ui/  src/{App.tsx, api/{commands,events,types}.ts, store/useDownloads.ts,
│                views/{Downloads,Queue,Completed,Settings}.tsx,
│                components/{DownloadRow,DetailsDrawer,ConnectionTable,Sparkline,
│                            AddDownloadDialog,ProgressBar,StatusPill,Sidebar}.tsx}
│
├─ benchmarks/{scenarios/*.toml, results/*.json, REPORT.md}
├─ docs/{PLAN.md, ARCHITECTURE.md, ENGINE.md, BENCHMARKS.md, SECURITY.md,
│         EDGE_CASES.md, ROADMAP.md}
└─ .github/workflows/ci.yml          # fmt, clippy -D warnings, test, deny, bench-smoke
```

**Type safety across the IPC boundary:** `ts-rs` derives TypeScript definitions from the Rust
event/command structs into `app/ui/src/api/types.ts` at build time; CI fails if checked-in types are
stale. Eliminates the whole class of "UI silently reads a field the engine renamed."

---

## F. Session-by-session implementation plan

### Session 1 — Download engine (the big one; no UI at all)

**Deliverable:** a headless downloader that is *correct*, driven by `swiftload-cli`, with an
adversarial test server and a real test suite. The engine is the hard part and must not be rushed to
make room for UI polish.

Build order:
1. Workspace, `deny.toml`, CI skeleton (`fmt`, `clippy -D warnings`, `test`).
2. `util/intervals.rs` (RangeSet) + `util/filename.rs` — **with their tests first**. These two are
   where silent corruption and security bugs live.
3. `swiftload-testserver`: deterministic content (`byte[i] = ChaCha8(seed)[i]` so a 10 GB "file"
   needs zero disk and every byte is verifiable), plus the adversarial modes in §G.2 and TCP-accept
   counting.
4. `http/client.rs` with the §D.2 per-worker-Client + h2-window configuration, and
   `distinct_connections.rs` proving it.
5. `probe.rs` — ranged-GET probe, redirect chain, validators, Content-Encoding trap, filename
   resolution.
6. `writer.rs` — bounded channel, coalescing, `seek_write`, preallocation + sparse, the
   sync-then-checkpoint ordering.
7. `store/` — schema, migrations, checkpoint transaction, startup recovery incl. `rewind_tails`.
8. `plan.rs` + `worker.rs` — claims, range verification, shrink/steal, straggler handling, endgame.
9. `governor.rs` — pure `decide()`, plus unit tests over synthetic traces.
10. `errors.rs` + retry/backoff, NetworkDown handling, RangeLied degradation.
11. `manager.rs` + `scheduler.rs` — queue, N-concurrent, per-host budget, probe token.
12. **★ Resource identity and URL refresh (§D.10)** — `task/identity.rs` (verification ladder Tier 0
    and Tier 1, decision matrix), `util/redact.rs`, the three-URL model, `url_history`,
    `validate_replacement_url` / `commit_replacement_url` / `find_existing_for`, Governor reset and
    host-profile rebinding on swap.

    > **Why this is Session 1 and not Session 3.** The requirement's suggested split puts validation
    > in Session 3, but the verification ladder *determines the data model* (`current_url`,
    > `validation_state`, `verified_windows`, `url_history`) and *sits inside the resume path*.
    > Deferring it would mean Session 2 ships a UI for a pipeline that doesn't exist and Session 3
    > forces a schema migration through the engine's most safety-critical code — exactly the
    > "redesign the download engine later" outcome this is meant to avoid. Identity is a *core
    > engine concern*; only the dialogs belong downstream.

13. `swiftload-cli`: `get <url> [--conns N|auto] [--out DIR]`, `resume`, `list`, `verify`,
    `inspect`, **`refresh-url <id> <new-url> [--verify=tier0|tier1|tier2]`** — makes the whole
    feature testable headlessly, before any UI exists.

14. **Minimal throughput benchmark** (`swiftload-bench --smoke`): the `1 / 4 / 8 / adaptive` ×
    `{per-conn throttle, total throttle}` slice only. Not the full matrix — just enough to record a
    committed baseline and to prove the Governor's discriminator behaves correctly against **known
    ground truth** before any UI exists.

**Session 1 exit criteria — these are the front-loaded risk gates; do not start Session 2 until all
pass:**
- 2 GB download from the test server at 32 connections is **byte-exact** against the deterministic
  content function.
- ★ 16 workers produce **16 distinct TCP accepts** on an h2-enabled test server (risk #1 retired).
- ★ `kill -9` at 10 random points × {1, 4, 16 conns} → restart → always resumes to a byte-exact
  file, never restarts from zero (risks #2/#3/#4 retired).
- Liar-server, no-Content-Length, no-Range, gzip-despite-identity, 429-with-Retry-After,
  mid-stream-reset, expiring-URL, and changed-ETag scenarios all behave per §D and §K.
- Governor unit tests cover all five regimes in the §D.5 table, with no oscillation over a 60 s
  synthetic trace.
- ★ Smoke benchmark shows **near-linear scaling under `per=conn` throttling** and **no gain under
  `per=total`**, and adaptive lands within 10% of the best fixed level in both (risk #6 retired —
  the central performance thesis is proven before any UI work begins).
- ★ **URL refresh works end-to-end headlessly**: a 2 GB download interrupted at ~60%, its signed URL
  invalidated, a fresh URL supplied via `swiftload-cli refresh-url` → resumes from ~60% and the
  final file is byte-exact. Tier-1 validation transfers **< 1 MB**. The decoy case (same filename,
  same size, different content) is **rejected**, and the size-mismatch case is rejected. §G.4 tests
  1, 4, 5, 6, 7, 8, 9, 10 all pass.
- Sustained ≥ 90% of the test server's configured aggregate rate at 8 connections, RSS < 100 MB.
- Baseline JSON committed to `benchmarks/results/` and wired into CI's regression gate.

### Session 2 — Application, UI, productization

1. Tauri v2 shell: commands (§B.2), `EventPump` (4 Hz batching, dirty-set), single-instance,
   error mapping.
2. `ts-rs` type generation wired into the build.
3. React shell: sidebar (Downloads / Queue / Completed / Settings), category filter, dark/light
   following OS theme, Mica backdrop, custom titlebar, system accent.
4. `DownloadRow`: filename + type icon, thin progress bar,
   `128 MB / 1.2 GB · 24.3 MB/s · 00:00:45 left`, status pill, pause/resume/cancel, and a `⋮`
   overflow menu: Resume · Pause · Cancel · Retry · **Use New URL** · Open File · Open Folder ·
   Details.
5. `AddDownloadDialog`: URL (pre-filled from clipboard **on explicit button click** — clipboard
   *monitoring* is roadmap), live probe preview (size, resumable?, filename), editable filename,
   destination from category, `Auto`/manual connection count, Start now vs Add to queue.
6. `DetailsDrawer` (expand chevron — advanced info never clutters the main list), tabs:
   - **Connections** — live table: #, range, bytes, speed, retries, state
   - **Server** — final URL + redirect chain, Accept-Ranges, ETag, Last-Modified, Content-Type,
     HTTP version, TLS
   - **Timeline** — the `events` ring
   - **Stats** — speed sparkline + **concurrency-over-time chart** (this visualizes the Governor and
     is the single best demo asset in the product)
   - **Links** — the redacted `url_history` chain with each entry's outcome and preserved-bytes
     figure, plus a "Copy full link" action (§D.10.8, audited)
6b. **★ URL-refresh UI (§D.10).** Three surfaces, all speaking plain language — the user never sees
    the words *ETag*, *Range*, *206*, or *byte offset*:
    - **Expired banner** on the row: `⚠ Download link expired — 6.5 GB / 10 GB already downloaded.`
      with `[Provide New URL]` and `[Retry Original]`.
    - **Refresh dialog**: paste box → `[Validate URL]` → a progress line ("Checking that this link
      is the same file…") driven by `validate_replacement_url`, which mutates nothing.
    - **Three result cards**, one per §D.10.4 outcome:
      - *Verified* → "Same file confirmed. Resuming from 6.5 GB." Auto-continues, no click needed.
      - *Confirm* → "This looks like the same file. The server's version tag changed, so we compared
        4 sample sections of what you've already downloaded — all matched."
        `[Resume from 6.5 GB]` `[Start over]`
      - *Reject* → states the concrete reason in user terms ("This link is a different file — it's
        12 GB, yours is 10 GB") with `[Start a new download]` `[Cancel]`. **No "resume anyway."**
    - **Duplicate-found dialog** on add (§D.10.7): "You already have an incomplete download for this
      file — movie.mkv, 6.5 GB / 10 GB." `[Resume Existing]` `[Start New]`.
    - **Non-resumable replacement** (§D.10.5): "This link doesn't support resuming. Using it means
      downloading all 10 GB again." `[Keep waiting for a resumable link]` `[Restart with this link]`
      `[Cancel]` — the partial file is never deleted by any of these.
7. Queue view (reorder, priority, start-now), Completed/history view (search, re-download, open
   folder, delete), Settings (dirs, categories, simultaneous limit, per-download conn limit,
   adaptive on/off + K_MAX, retries, timeouts, theme, notifications, MOTW, hash-on-complete,
   start-with-Windows, paranoid recovery).
8. MOTW on completion. **Nothing is ever auto-opened**; "Open File" exists only as a deliberate
   click in the `⋮` menu and goes through `ShellExecute`, so MOTW/SmartScreen engage exactly as for
   a browser download. Auto-open-on-complete is not a setting we offer.
9. **Installer built and VM-tested this session, not left to the end**: NSIS + MSI via
   `tauri-bundler`, icons, uninstall, upgrade-in-place.
10. **Security pass pulled forward** (so the shipped build is safe, not just the tuned one):
    `cargo-fuzz` over `filename.rs`, path-traversal and ADS tests, redirect/TLS tests,
    `cargo audit` + `cargo deny`, Tauri CSP and capability-allowlist review.
11. **README with the smoke-benchmark results table and its caveats**, including the "when
    parallelism does NOT help" rows. A shippable build must not ship an unfalsifiable claim.

**Session 2 exit criteria — the product is releasable at this point:**
- Install from the built installer on a **clean Windows 11 VM**; download a real 1 GB file; pause,
  resume, cancel; kill the app mid-download and relaunch → resumes; uninstall leaves no orphans.
- Idle RSS < 120 MB; CPU < 2% idle, < 10% of one core at 100 MB/s.
- No UI event storms: verified with the Governor deliberately oscillating.
- Security checklist in §J passes; `cargo audit` and `cargo deny` clean.
- README carries honest results. **If everything stopped here, this would be a real product** — a
  correct engine, a modern UI, a signed-off security posture, and defensible numbers.

### Session 3 — Tuning, soak, polish

Session 3 improves a working product rather than finishing an unfinished one.

1. `swiftload-bench`: the full matrix (§H), CPU/RSS sampling via `GetProcessTimes` /
   `GetProcessMemoryInfo`, JSON output, `benchmarks/REPORT.md` generator.
2. **Run the matrix and tune the Governor constants from measured data** (`DWELL_MS`,
   `GAIN_THRESHOLD`, `WARMUP_MS`, endgame thresholds) — these are currently educated guesses.
3. Failure-injection suite + a 24 h soak (many downloads, network flapping) with RSS/handle-leak
   checks.
4. Complete the §K edge-case matrix with a test for each row.
4b. **★ URL-refresh hardening (§D.10):** full-app versions of §G.4 tests 2 and 3 (app restart and
    extended network loss with the link expiring in between); **Tier 2** ("Verify thoroughly") and
    **Tier 3** (deferred whole-file hash) as user-selectable options; the multi-refresh soak
    (A→B→C→D→… over 20 swaps on one 5 GB download, asserting `completed_ranges` only ever grows and
    the final file is byte-exact); and the `url_refresh` benchmark scenario from §H.2.
5. Extend the Session 2 security pass: longer fuzz budget over `filename.rs`, a fuzz target for the
   `Content-Range` / `Content-Disposition` / `Retry-After` header parsers, and **a redaction fuzz
   target asserting no query-parameter *value* ever survives into `url_history`, `events`, or any
   log sink** (§D.10.8).
6. `INetworkListManager` connectivity events (Wi-Fi reconnect resumes in ~1 s instead of up to 60);
   `IAttachmentExecute::Save` for MOTW + Defender scan.
7. Profiling pass: allocation reduction, confirm RSS < 150 MB at 32 conns.
8. Docs: `BENCHMARKS.md` (with the "when parallelism does NOT help" section — non-negotiable),
   `SECURITY.md`, `ARCHITECTURE.md`, README with the results table. Code-signing note (unsigned
   installers trigger SmartScreen; publish SHA-256 at minimum).

---

## G. Testing strategy

### G.1 Unit
- **`RangeSet`** — `proptest`: insert-in-any-order → same canonical set; total == sum of lengths; no
  overlaps; no unmerged adjacent pairs; `encode`∘`decode` round-trips; `rewind_tails` never grows
  the set.
- **Filename sanitizer** — table tests for every rule in §D.8 (traversal, ADS `:`, bidi override,
  `CON.txt`, trailing dot/space, 300-char names, RFC 5987 `filename*` with UTF-8), plus `cargo-fuzz`
  asserting the containment invariant never breaks.
- **Governor** — `decide()` against synthetic traces for all five regimes; assert no oscillation
  (bounded state changes over a 60 s trace) and correct `saturation_detected` inference.
- Header parsing (`Content-Range`, `Content-Disposition`, `Retry-After` seconds *and* HTTP-date),
  error classification, backoff jitter bounds.

### G.2 Integration — everything against `swiftload-testserver`
Server modes (all with deterministic, verifiable content):

| Route | Behaviour |
|---|---|
| `/plain/<size>` | normal, range-capable |
| `/norange/<size>` | `Accept-Ranges: none`, always 200 |
| `/liar/<size>` | advertises `bytes`, then **ignores** Range |
| `/nolen/<size>` | chunked, no Content-Length |
| `/throttle/<size>?bps=N&per=conn\|total` | **token bucket per connection or global** — ★ the key mode: makes "when does parallelism help?" a controlled experiment with known ground truth |
| `/latency/<size>?rtt=Nms` | injected delay before first byte and between chunks |
| `/flaky/<size>?p=0.05` | random resets and mid-stream aborts |
| `/status/<code>` | 403/404/429/500/503, with and without Retry-After |
| `/expiring/<size>?ttl=Ns` | signed token that 403s after TTL |
| `/changing/<size>` | ETag/Last-Modified change after N seconds |
| `/redirect/N` | chains, incl. cross-origin and https→http |
| `/slowconn/<size>` | one connection deliberately starved (straggler) |
| `/maxconn?n=N` | refuses/queues beyond N concurrent connections |
| `/gzip/<size>` | applies `Content-Encoding: gzip` despite `identity` (§D.1 step 5) |

URL-refresh modes (§D.10) — all serve content from a named **seed**, so "same file" and "different
file" are exact, controllable facts rather than approximations:

| Route | Behaviour |
|---|---|
| `/signed/<seed>/<size>?token=T&exp=N` | 403 once `exp` passes, or on demand via `/expire/<seed>` |
| `/mint/<seed>` | issues a **fresh valid signed URL for the same seed** — the "user got a new link" step |
| `/rotate-etag/<seed>/<size>` | identical content, **different ETag on every request** — simulates CDN-node variance (Test 7) |
| `/decoy/<size>?name=X` | same filename, same size, **different seed** — must be rejected by Tier 1 (Test 5) |
| `/resized/<seed>/<size2>` | same seed and filename, **different size** — must be hard-rejected (Test 6) |
| `/nometa/<seed>/<size>` | no ETag, no Last-Modified — forces Tier 1 + confirm |
| `/newhost/<seed>/<size>` | same content on a second listener (different host:port) — CDN migration |

Plus `/stats` returning distinct TCP accept counts (for `distinct_connections.rs`) and per-route
**byte-served counters**, which is how the "Tier-1 validation transferred < 1 MB" assertion is made
server-side rather than trusted from the client.

### G.3 Failure recovery
- **Crash matrix:** kill the process at 10 randomized offsets × {1, 4, 16 conns} × {clean, SIGKILL}
  → resume → byte-exact. Run in CI.
- **Truncated `.slpart`** — externally truncate the file, verify recovery drops the phantom ranges.
- **Simulated lying fsync** — a test store that discards the last N bytes on "crash"; asserts
  `paranoid_recovery` catches it.
- **Disk full** — small VHD or a quota'd temp dir; assert a clean pause with a clear error, not a
  spin or a corrupt file.
- **Network flap** — test server closes all connections for 30 s; assert `waiting_network`, no
  retry-budget burn, clean resume.
- **Changed resource** — `/changing` → `If-Range` fails → `needs_attention`, both user choices work.
- **Expired URL** — `/expiring` → re-resolve from `original_url` succeeds; and the failure path.

### G.4 ★ URL-refresh tests (`tests/url_refresh.rs`)

Every test ends with a **byte-exact verification of the final file** against the seed's content
function — a resume that "succeeds" but produces wrong bytes must fail the test.

| # | Test | Expected behaviour |
|---|---|---|
| 1 | **Expired URL** — download 2 GB to ~60%, `/expire`, supply a `/mint` URL, resume | Resumes from ~60%; Tier 0 passes (same ETag) → no prompt; **< 1 MB validation traffic** (asserted from `/stats`); final file byte-exact |
| 2 | **Application restart** — partial, kill the process, expire the URL, restart, refresh, resume | `completed_ranges` survives the crash path (§D.7); resume from the checkpoint; byte-exact |
| 3 | **Network interruption** — sever connectivity mid-download for 60 s, expire during the outage | `waiting_network` → `needs_attention`; retry budget not burned; refresh then resumes |
| 4 | **Same file, different URL** — a `/mint` URL *before* expiry | Accepted, swapped, `url_refresh_count = 1`, no data re-downloaded |
| 5 | **Different file, same filename** — `/decoy` with matching name and size | ★ **Rejected** at Tier 1 (`rejected_content`). No bytes appended, partial preserved, and the UI offers no "resume anyway" path |
| 6 | **Changed file size** — `/resized` 10 GB → 12 GB | ★ **Hard-rejected** before any fetch (`rejected_size`); partial preserved |
| 7 | **Changed ETag, same content** — `/rotate-etag` | Tier 1 runs and passes → `user_confirmed`; on confirmation the **new validator is adopted** so a later pause/resume does *not* re-prompt |
| 8 | **No Range support on the replacement** — refresh onto `/norange` | Offered as restart-only with an honest message; **partial file not deleted**; declining leaves the download in `needs_attention` intact |
| 9 | **Multiple refreshes** A→B→C→D | One `DownloadId` throughout; `completed_ranges` monotonically grows; `url_history` has 4 entries, all redacted; byte-exact |
| 10 | **Corrupted local partial** — flip bytes in `.slpart`, then refresh with a *valid* URL | ★ Tier 1 detects the mismatch and **refuses to resume**; the message stays neutral about which side is wrong; no corrupt file is ever produced |

Supporting unit tests: the §D.10.4 decision matrix as a pure-function table test over all
signal combinations; deterministic window placement (`hash(download_id)` → same windows on retry,
different windows across downloads, all inside `completed_ranges`); and redaction round-trips.

Cross-cutting: a test asserting **no full URL with query values ever reaches `url_history`,
`events`, or the log sink**, run against a corpus of real-world signed-URL shapes (S3 presigned,
Azure SAS, GCS, Cloudflare, generic `?token=`).

### G.5 Performance / regression
- `bench-smoke` in CI: a fixed loopback scenario, fails the build if throughput regresses > 20% or
  RSS grows > 30% versus the recorded baseline.
- 24 h soak with periodic RSS and handle-count sampling — leak detection.

---

## H. Performance strategy and benchmarking

### H.1 How we realistically outperform a basic downloader
1. Correct concurrency **discrimination** (§D.5) rather than a fixed connection count.
2. **Guaranteed distinct TCP connections** (§D.2) — the thing most naive implementations get wrong.
3. **Tuned h2 flow-control windows** — worth multiples on high-BDP paths.
4. **Straggler shrink-and-steal** — removes the tail-latency penalty that fixed-segment downloaders
   pay on every heterogeneous path.
5. **Connection reuse across claims** — no handshake per chunk.
6. **Learned host profiles** — the second download from a host starts at the right concurrency
   instead of re-probing.
7. **Backpressured single writer** — never disk-stalled, never memory-ballooning.

### H.2 The benchmark matrix

**Treatments:** `1, 4, 8, 16, 32, adaptive` connections.
**Scenarios:** per-conn throttle (2 MB/s/conn) · total throttle (20 MB/s aggregate) · high RTT
(200 ms) · lossy (0.5%) · flaky (resets) · straggler · `maxconn=6` · real CDN (3 public files).
**Sizes:** 10 MB, 200 MB, 2 GB.
**Measured:** wall time, bytes, mean throughput, **peak (1s)**, TTFB, retries, connection failures,
HTTP status histogram, CPU ms, peak working set, disk bytes written.

**Plus a `url_refresh` scenario (§D.10.9)** — the feature's whole point is *not* re-transferring
data, so it gets its own measurement rather than an assertion. Download a 10 GB file to 65%, expire
the link, refresh, resume to completion, and report:

- **bytes transferred after the swap** vs. the theoretical minimum (3.5 GB) — overhead must be
  < 0.01%,
- **Tier-1 validation bytes** (target < 1 MB) and **wall-clock added by validation** (target < 2 s),
- the same figures for Tier 2, so the fast-vs-strong trade-off in §D.10.3 is a published number
  rather than a claim.

Server-side byte counters from `/stats` are the source of truth here, not client-side accounting.

### H.3 Repeatability rules (these make the numbers mean something)
- **≥5 repetitions**, report **median + p10/p90**, discard the first (warm-up).
- **Interleave treatments A/B/A/B, never block them.** Blocked runs conflate time-of-day network
  variance with the treatment — the most common way benchmark results become fiction.
- Pin machine, record git SHA, close other network apps, disable Windows Update during runs.
- **Loopback caveat, stated in the report:** over loopback there is no real network, so raw parallel
  runs mostly measure syscall overhead. It is the **shaped** modes (per-conn token bucket + injected
  RTT + loss) that make loopback benchmarks meaningful for concurrency questions. Real-WAN numbers
  are reported separately and labelled as indicative.
- Optional real-network shaping: `clumsy` on Windows, or `netem` in a Linux CI container.

### H.4 Baselines, including IDM
- **SwiftLoad at k=1** — the honest "single-stream" baseline: same stack, same disk path, so the
  measured delta is attributable to concurrency and nothing else. This is the primary comparison.
- `curl` and PowerShell `Invoke-WebRequest` — sanity checks that our single-stream path isn't slow.
- **IDM** — has no scriptable API, so it is measured manually on 3 real public files, ≥5 interleaved
  runs, and reported as **indicative, not rigorous**, with that caveat printed in the table itself.

**The report must include a "when parallelism does NOT help" section with the losing rows.** A
benchmark report that only shows wins is marketing, and it destroys credibility the moment a user
reproduces a neutral result.

---

## I. Risks

| # | Risk | Impact | Mitigation |
|---|---|---|---|
| 1 | **h2 multiplexing collapses all workers onto one TCP conn** (§D.2) | Silently caps throughput at 1-connection levels; looks like server saturation; would invalidate every benchmark | Per-worker Client from commit one + `distinct_connections.rs` asserting TCP accept counts |
| 2 | **Range + Content-Encoding mismatch** (§D.1.5) | **Silent file corruption** — the worst possible bug | `Accept-Encoding: identity` everywhere; detect non-identity responses and force single-stream; regression test |
| 3 | **Server lies about Range support** | Corrupt file that passes a size check | Verify `206` + `Content-Range` start **before writing any byte**; keep only the offset-0 prefix on detection; blacklist the host profile |
| 4 | **Checkpoint claims undurable bytes** | Corrupt file after crash resume | Strict `sync_data` → commit ordering; `clean_shutdown` flag; `rewind_tails(1 MiB)` paranoid recovery |
| 5 | Governor oscillation / mutual interference | Thrashing connections, worse throughput | Min dwell, 15% hysteresis, one **global probe token**, exponential re-probe backoff, pure-function unit tests over synthetic traces |
| 6 | **Benchmarks meaningless** because the tester's link is the bottleneck | The core objective goes unvalidated | The shaped test server is the primary environment; ground truth is known; real-WAN reported separately with caveats |
| 7 | **Windows Defender real-time scanning** of `.slpart` writes | Large throughput loss on some machines | Large coalesced writes; measure with/without an exclusion; document the exclusion as an optional user tip |
| 8 | Unsigned installer → SmartScreen warning | Users bounce at install | Publish SHA-256 + reproducible builds; recommend an OV/EV cert before any wide release; document it |
| 9 | Async cancellation dropping a future mid-write | Torn state | All writes go through the writer task; explicit `CancellationToken`; structured shutdown; never `abort()` a worker mid-claim |
| 10 | UI event storms burning CPU | Contradicts the "lightweight" pitch | 4 Hz batched `progress-tick`; connection detail only while subscribed; enforced by review + an explicit test |
| 11 | SQLite lock contention / write amplification | Stalls under many active downloads | Single writer connection in a store task; WAL; checkpoints batched at 8 MiB / 5 s; never per-chunk |
| 12 | **Scope creep** (IDM has 200 features) | Missing the 3-session budget | §K roadmap is explicit; anything not in §F is out of MVP, full stop |
| 13 | Tauri/WebView2 packaging surprises | Late-breaking blocker | Installer is built and VM-tested **in Session 2**, not deferred to Session 3 |
| 14 | **Resuming onto a genuinely different file** after a URL refresh | Silent corruption of a multi-GB download — the worst outcome this feature could cause | Hard size gate; Tier-1 content comparison at 4 unpredictable windows inside the completed region; **content mismatch is a hard reject with no user override**; §G.4 test 5 (decoy) and test 10 (corrupt local) are release gates |
| 15 | **Signed URLs leaking** into logs, the events ring, diagnostics or benchmark files | Credential disclosure — a signed URL *is* a bearer token | Default-deny redaction (§D.10.8): query *values* are stripped everywhere except the three columns that must fetch; redaction fuzz target over real-world signed-URL shapes; `reveal_full_url` is explicit and audited |
| 16 | Users over-trusting the "confirm" path and clicking through warnings | Corruption via consent | The confirm card is only reachable **after** Tier 1 has already passed; a *failed* check has no confirm path at all, only "Start over" |

---

## J. Definition of Done

**This checklist — not the session count — decides when the product is finished.** Work continues
across session boundaries until every box is ticked.

**Engine correctness**
- [ ] 2 GB @ 32 conns from the test server is byte-exact vs. the deterministic content function
- [ ] `kill -9` at 10 random offsets × {1, 4, 16 conns} → resume → byte-exact, every time
- [ ] `distinct_connections.rs` proves k workers ⇒ k TCP connections on an h2 server
- [ ] Every §G.2 server mode has a passing integration test
- [ ] Governor unit tests cover all five §D.5 regimes with no oscillation
- [ ] `RangeSet` proptests and filename fuzzing pass; containment invariant never violated

**Reliability**
- [ ] Network flap mid-download → `waiting_network` → clean resume, no retry-budget burn
- [ ] App killed and relaunched → resumes; Windows restart → resumes
- [ ] Disk full → clean pause with a clear message, no corrupt file, no spin
- [ ] Changed resource → `needs_attention`, both user choices work correctly
- [ ] Expired signed URL → auto re-resolve, or a preserved partial + "paste fresh link"
- [ ] 24 h soak: no RSS growth trend, no handle leak

**Resume with a replaced URL (§D.10)**
- [ ] A 10 GB download interrupted at 6.5 GB with an expired link resumes from 6.5 GB on a fresh
      URL — **transferring ≈3.5 GB plus < 1 MB of validation**, never restarting from zero
- [ ] All ten §G.4 tests pass, each ending in a byte-exact final file
- [ ] Decoy (same name + same size, different content) is **rejected**; size mismatch is
      **rejected**; a corrupt local partial is **detected**, not resumed over
- [ ] ETag rotation with identical content resumes after one confirmation, and **adopts the new
      validator** so later pause/resume cycles do not re-prompt
- [ ] A non-resumable replacement URL never deletes the partial file
- [ ] Chain of ≥4 refreshes keeps one `DownloadId`, one `.slpart`, monotonically growing
      `completed_ranges`
- [ ] Duplicate detection offers to resume an existing partial instead of starting a second copy,
      and routes through the same validation pipeline
- [ ] **No full URL with query values appears in `url_history`, `events`, logs, or benchmark output**
      (verified by the redaction fuzz corpus)
- [ ] The refresh UI never shows the words *ETag*, *Range*, *206*, or *byte offset*

**Application**
- [ ] All four views functional; queue reorder, simultaneous limit, per-download conn limit work
- [ ] Download row shows filename, progress, done/total, current, average, **peak (1s)**, ETA,
      status, pause/resume/cancel
- [ ] Details drawer shows connections, per-connection speeds, retries, server capabilities,
      timeline, concurrency-over-time — and none of it clutters the main list
- [ ] Categories → destination folders; history with search and re-download
- [ ] Settings persist and take effect without restart

**Security**
- [ ] No path escapes the configured directory under any fuzzed filename
- [ ] ADS (`:`), reserved device names, bidi overrides, traversal all rejected
- [ ] TLS validation always on; **no** "ignore certificate errors" setting exists anywhere
- [ ] https→http redirect refused by default; auth headers stripped cross-origin
- [ ] MOTW applied to every completed file; nothing is ever auto-executed
- [ ] `cargo audit` and `cargo deny` clean

**Performance**
- [ ] Benchmark matrix executed; `benchmarks/REPORT.md` generated with medians and p10/p90
- [ ] Report demonstrates near-linear gain in the per-conn-throttle scenario **and** explicitly
      documents the scenarios where parallelism does **not** help
- [ ] Adaptive mode is within 10% of the best fixed level in **every** scenario — this is the real
      test of the Governor: it must never be much worse than a good fixed choice
- [ ] Idle RSS < 120 MB; RSS < 150 MB at 32 conns; CPU < 10% of one core at 100 MB/s
- [ ] Governor constants set from measured data, not guesses

**Shipping**
- [ ] NSIS + MSI installers build in CI; clean-Win-11-VM install → download → uninstall verified
- [ ] README carries the results table **with its caveats**, and does not claim "always faster
      than IDM"
- [ ] `ARCHITECTURE.md`, `ENGINE.md`, `BENCHMARKS.md`, `SECURITY.md`, `EDGE_CASES.md` written

---

## K. Edge-case behaviour matrix

| Situation | Behaviour |
|---|---|
| Server doesn't support Range | Single connection, streamed; UI marks "not resumable"; no segmentation ever attempted |
| **Server lies about Range** | Detected on the first non-zero-offset response (`200` where `206` expected, or wrong `Content-Range` start) **before any byte is written**; keep only the contiguous prefix from 0; degrade to single-stream; flag `accept_ranges=3` on the host profile |
| Content-Length missing | Single connection, progress shown in bytes only (no % / ETA); size learned on completion; still resumable if Range works |
| Redirects | Max 10 hops; http/https only; https→http refused by default; auth/cookie stripped cross-origin; full chain shown in Details; `final_url` used for segments, `original_url` retained for re-resolution |
| URL expires mid-download | §D.10.5 ladder: retry `current_url` → re-probe `original_url` (silent swap on a Tier-0 pass) → extension (roadmap) → `needs_attention` + "Provide New URL". Partial file **always** preserved |
| **Replacement URL, same file** | Tier 0 passes → swapped silently, resumes from the persisted gaps, ≈0 wasted bytes |
| **Replacement URL, ETag rotated** | Not treated as disproof (§D.10.2). Tier-1 spot-check (256 KB) → on pass, one plain-language confirmation, then resume and adopt the new validator |
| **Replacement URL, different size** | Hard reject before any fetch. Offer a separate new download; existing partial untouched |
| **Replacement URL, different content, same name + size** | Tier-1 mismatch → hard reject, **no override path**. This is the corruption case the whole ladder exists to stop |
| **Replacement URL lacks Range support** | Honest message: resuming is impossible, restarting costs the full file. Partial preserved regardless of choice |
| **Replacement URL on a different host** | Accepted. `host_profiles` rebinds to the new host; Governor resets to Phase A since the old edge's concurrency conclusion doesn't transfer |
| **Replacement URL needs auth** | Probe 401 → `rejected_auth`, surfaced plainly; cookies/custom headers are roadmap (§L), not a silent failure |
| **Local partial is corrupt** | Tier 1 catches it on the next refresh; message stays neutral about which side is wrong; offers restart, never a silent overwrite |
| **Same file added twice via different links** | Duplicate-detection dialog offers "Resume Existing", which routes through the same validation pipeline — never an automatic merge |
| **Four or more successive URL refreshes** | One `DownloadId`, one `.slpart`; `url_history` records the redacted chain; `completed_ranges` only ever grows |
| 403 | On first request: fatal, surfaced clearly. Mid-download on a previously-working URL: treated as expiry (above) |
| 404 / 410 / 451 | Terminal; partial file retained until the user removes it |
| **429** | Honour `Retry-After` (cap 120 s), **halve concurrency**, set a host ceiling, persist `last_429_at` so the *next* download from that host starts conservatively. Never route around it with more connections |
| 5xx | Retry with full-jitter backoff, max 8 per segment; 503+Retry-After treated as rate limiting |
| Server throttles connections | Governor's per-conn-throughput discriminator detects it, backs off, records `saturation_detected`; `/maxconn` mode is tested explicitly |
| Connection becomes extremely slow | < 25% of median for 5 s → shrink its claim and redistribute; 0 bytes for 20 s → kill and respawn as a new TCP connection (≤3/min) |
| File changes on server | `If-Range` returns 200, or Content-Length/ETag differ → `needs_attention`; user chooses restart or keep-partial; never silently mixes old and new bytes |
| Disk becomes full | Preallocation fails fast at start; mid-download ENOSPC → pause with `LocalFatal`, clear message, no corruption, no retry spin |
| Destination file exists | Policy setting: auto-rename `file (2).ext` (default) / overwrite / ask. Never silently overwrites a file we didn't create |
| User pauses during active writes | Workers stop requesting new claims and drain in-flight chunks; writer flushes; `sync_data`; final checkpoint; sockets closed. Pause is always a consistent state |
| Application crashes | `clean_shutdown` stays 0 → on restart, `rewind_tails(1 MiB)` then resume from the checkpoint |
| Computer shuts down | `WM_QUERYENDSESSION`/`WM_ENDSESSION` handler performs a clean pause + checkpoint. Power loss falls back to the crash path above |
| Network changes (Wi-Fi ↔ Ethernet, VPN) | All workers fail together → `NetworkDown` (does **not** consume the retry budget) → `waiting_network` → backoff poll (Session 3: `INetworkListManager` events) → re-probe validators → resume |

---

## L. MVP vs. future roadmap

**In the MVP (§F):** HTTP/HTTPS · URL paste · metadata detection · Range · segmented parallel
download · adaptive concurrency · pause/resume · restart recovery · auto retry · queue · simultaneous
limit · per-download connection limit · current/average/peak speed · ETA · history · destination
folders · categories · settings · modern Windows UI · large files · integrity verification ·
**resume with a replaced URL, including identity validation and duplicate detection (§D.10)** ·
Windows installer.

**Explicitly out of the MVP — with the hook that makes each cheap later:**

| Future feature | Hook already in place |
|---|---|
| Chrome/Edge/Firefox extension | `RequestSpec { headers, cookies, referer, user_agent }` threaded through Probe + Worker (§B.3). Extension cancels the browser download and hands URL + cookies to a **native-messaging host** — no engine change |
| Browser download interception | Same as above |
| **Automatic URL refresh** (browser silently supplies a fresh signed link for an expired download) | ★ Already fully designed: the extension calls the *same* `commit_replacement_url(id, url, source: browser_extension)` and goes through the *same* §D.10.4 validation. Step 3 of the §D.10.5 ladder is already reserved for it. MVP is manual-only by choice, not by limitation |
| Encrypted local state (signed URLs at rest) | SQLCipher with the key in the Windows credential store. Deliberately **not** in MVP — see §D.10.8 on why key-beside-ciphertext would be theatre |
| Clipboard monitoring | MVP already reads the clipboard on explicit button click; monitoring is a background watcher + a setting |
| Scheduled downloads | Scheduler already gates on start conditions; add a time predicate |
| Bandwidth scheduler / speed limiter | `RateLimiter` trait already in the worker read loop, defaulting to `Unlimited` |
| Proxy support | Field already on `RequestSpec`; `reqwest` supports it natively |
| Authentication / cookies / custom headers | Already on `RequestSpec`; needs UI only |
| Torrent / magnet | `Protocol` trait boundary (`probe` / `fetch_range`); a new impl doesn't touch scheduler, store, or UI |
| Remote control | `Manager` is already an async command/event API; add a local HTTP/WS front end |
| Plugins | Same boundary; deliberately not designed further until there's a real second protocol |

Nothing above gets built in the MVP. The hooks cost near-zero now and prevent a rewrite later.

---

## M. Requirements I'd push back on

1. **"Benchmark 4/8/12/16/24/32 and pick the best."** Right instinct, wrong mechanism if taken
   literally. A ramp costs ~1.2 s of measurement per level, so exhaustively testing six levels burns
   ~8 s per download and is worthless on a saturated link. §D.5 uses **doubling with a midpoint
   refinement plus the per-connection-throughput discriminator** — it converges in 2–3 steps and,
   critically, knows *why* it stopped. The exhaustive sweep belongs in the offline benchmark harness
   (§H), not in the runtime.
2. **"Peak speed"** is undefined without a window. Specified as max-of-1-second-windows (§C.3);
   instantaneous peaks measure the OS receive buffer, not the network.
3. **"Reliable file integrity"** cannot mean "the bytes are what the publisher intended" without a
   server checksum. We report `integrity_state` honestly in three tiers (§D.9) rather than showing a
   green check we can't justify.
4. **32 connections as a default** would be antisocial and often counterproductive. Default cap:
   **8 per download, 24 globally, 8 per host**, adaptive within that. 32 remains available as a
   manual override and as a benchmark data point.
5. **"Persist segment states"** as one row per segment is the wrong shape under a shrink-and-steal
   scheduler. A single `RangeSet` BLOB is strictly better: atomic in one row-update, compact, and
   trivially recoverable (§C.1).
6. **On URL refresh — three points where I'd push back on the requirement as written:**
   - *"Consider ETag ... in appropriate order"* implies a validator hierarchy where a mismatch
     counts against identity. It shouldn't: **ETag inequality is not evidence of a different file**
     (§D.10.2), and treating it as such would reject the majority of legitimate CDN re-signings.
     Only **Content-Length inequality** is a genuine gate.
   - *"user confirmation when automatic verification is inconclusive"* is right — but confirmation
     must sit **after** a passing content check, never after a failing one. A user clicking past a
     proven byte mismatch is the exact corruption path the feature is meant to close, so §D.10.3
     gives a failed check no override at all.
   - *Suggested Session 3 placement for validation* would force a schema migration through the
     engine's most safety-critical code after the UI already shipped against it. Identity is an
     engine concern; §F moves it to Session 1 and leaves only dialogs downstream.
7. **"2–3 sessions"** is achievable **only** if Session 1 is engine-only with no UI work. Attempting
   UI in Session 1 is the most likely way this project ends up with a pretty app that corrupts large
   files. Per the locked decision, §J governs completion rather than the session count — but the
   ordering is not negotiable: if time is lost, cut UI polish and roadmap hooks first, then Session 3
   tuning. **Never cut range verification, checkpoint ordering, or crash recovery** — those three are
   the difference between a download manager and a file corrupter.

---

## N. Verification (how to check the work end-to-end)

```powershell
# Engine (Session 1) — no UI required
cargo test --workspace                       # unit + integration vs. the test server
cargo test -p swiftload-core --test crash_recovery -- --ignored   # the kill -9 matrix
cargo run -p swiftload-testserver -- --port 8080
cargo run -p swiftload-cli -- get http://127.0.0.1:8080/throttle/2GiB?bps=2000000&per=conn --conns auto
cargo run -p swiftload-cli -- verify <path>  # byte-exact vs. the deterministic content fn

# URL refresh, headless — the 10 GB/6.5 GB scenario in miniature (Session 1)
cargo run -p swiftload-cli -- get "http://127.0.0.1:8080/signed/seedA/2GiB?token=ABC&exp=30"
#  → interrupt at ~60%, then:
curl -s http://127.0.0.1:8080/expire/seedA                  # invalidate the link
NEW=$(curl -s http://127.0.0.1:8080/mint/seedA)             # user obtains a fresh link
cargo run -p swiftload-cli -- refresh-url <id> "$NEW"       # validates, then resumes from ~60%
curl -s http://127.0.0.1:8080/stats                         # assert validation bytes < 1 MB
cargo run -p swiftload-cli -- verify <path>                 # must be byte-exact
#  → negative controls, both must REFUSE and leave the partial intact:
cargo run -p swiftload-cli -- refresh-url <id> "http://127.0.0.1:8080/decoy/2GiB?name=same"
cargo run -p swiftload-cli -- refresh-url <id> "http://127.0.0.1:8080/resized/seedA/3GiB"

# App (Session 2)
cargo tauri build                            # produces NSIS + MSI
#  → install on a clean Win 11 VM, download a real 1 GB file, pause/resume,
#    kill the app mid-download, relaunch, confirm it resumes, then uninstall

# Performance (Session 3)
cargo run -p swiftload-bench -- --matrix benchmarks/scenarios/full.toml --reps 5
#  → benchmarks/results/*.json + benchmarks/REPORT.md
```

CI (`.github/workflows/ci.yml`): `cargo fmt --check`, `cargo clippy -- -D warnings`,
`cargo test --workspace`, `cargo deny check`, ts-rs staleness check, and `bench-smoke` (fails on
>20% throughput regression or >30% RSS growth vs. the recorded baseline).
