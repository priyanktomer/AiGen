# SwiftLoad

A free, modern Windows download manager — a genuinely fast, reliable alternative to Internet
Download Manager, whose performance claims you can reproduce rather than take on faith.

> **Status: engine complete and benchmarked. No GUI yet.**
> The download engine, an adversarial test server, a headless CLI and a benchmark harness are
> built and tested. The Tauri desktop UI is the next phase.

```bash
cargo test --workspace                                    # 252 tests
cargo run -p swiftload-testserver -- --port 8080          # adversarial server
cargo run -p swiftload-cli -- get <url> --out ./downloads
cargo run --release -p swiftload-bench                    # reproduce the numbers below
```

---

## Does segmented downloading actually help?

Sometimes. Here is when, measured rather than asserted — 64 MB per run, 3 interleaved
repetitions, against a locally shaped server where the constraint is ground truth
([full report](benchmarks/REPORT.md), [harness](crates/swiftload-bench)):

| Server behaviour | 1 conn | 8 conns | 16 conns | SwiftLoad (adaptive) |
|---|---|---|---|---|
| **Caps each connection** — common on file hosts and many CDNs | 1.00× | 8.27× | **17.17×** | **17.13×** ¹ |
| **Caps your IP in total** | 1.00× | 0.98× | 0.98× | 0.98× |
| **No bottleneck at all** (loopback) | 1.00× | 0.96× | — | 1.00× |

¹ On a host SwiftLoad has downloaded from before. Starting cold it reaches 9.42×, because it has
to try a connection count before it can know that one is better, and on a one-second download the
trying is most of the transfer. It remembers the answer per host, so the next download starts
where the last one finished.

**The middle row is the important one.** When a server caps your IP rather than each connection,
opening more connections buys you nothing at all — it just costs CPU, memory and server load.
Download managers that always open 32 connections are, in that very common case, doing harm for
no benefit. SwiftLoad detects which situation it is in and stops.

### What this does not claim

- **Not "faster than IDM".** Not measured, and unmeasurable in general: on a link you have already
  saturated, every downloader finishes at the same time.
- **Not a prediction about your download.** These are loopback numbers with synthetic shaping,
  chosen because the constraint is known exactly. Real networks vary by server, route and hour.
- **Not "more connections are better".** Two of the three rows above show them making no
  difference or a slight loss.

---

## How it works

```
Probe ──► Plan (claims) ──► Governor ◄── Metrics
              │                 │
              ▼ claim           ▼ spawn / shrink / retire
        Worker ×k  ──bounded channel──►  Writer ──► fsync ──► checkpoint
     (one TCP connection each)          (one per download)
```

**Adaptive concurrency.** A throughput plateau has several causes that demand opposite responses,
and aggregate throughput alone cannot tell them apart. So the governor watches *per-connection*
throughput too: if it holds steady as connections are added, the server caps per connection and
ramping pays; if it halves, the pipe is saturated and ramping is waste. It is a pure state
machine, tested against synthetic traces with no network involved.

**One client per worker.** With a shared HTTP client, an HTTP/2 server multiplexes every
"parallel" segment onto a *single* TCP connection — one congestion window, one loss-recovery
domain. That is strictly worse than a single-stream download, and it is invisible: throughput just
plateaus, exactly as a saturated server would. A test counts server-side TCP accepts to prove the
connections are real.

**Verify before writing.** A server that advertises `Accept-Ranges` and then ignores `Range`
returns the whole file with status 200. Writing that body at a segment offset produces a file of
exactly the right size and entirely wrong contents. Every response is checked before a byte
reaches disk.

**Crash-safe by construction.** One writer per download orders every cycle as
`write → fsync → checkpoint`, so a checkpoint can never claim bytes that are not durable. Segment
state is a single varint-encoded interval set in a single row, making each checkpoint one atomic
update. Tested by `SIGKILL`ing the real binary at four different points and requiring a byte-exact
file after resume.

**Expired links do not cost you the download.** Identity is the download, not the URL. When a
signed link expires at 6.5 GB of 10 GB, paste a fresh one: SwiftLoad compares four 64 KB samples
of what you already have against the new source — about 0.003% of the file — and resumes. A
changed `ETag` is not treated as proof of a different file, because CDN edges and S3 multipart
uploads change it routinely with identical bytes. A content mismatch is refused outright, with no
"resume anyway" option, because clicking past a proven mismatch is how a 10 GB file gets corrupted.

---

## Security

- TLS validation is always on. There is no setting to disable it, and none will be added.
- Filenames from servers are sanitized against path traversal, NTFS alternate data streams
  (`report.pdf:payload.exe`), bidi-override extension spoofing, and reserved device names. A
  property test asserts no input can produce a path outside the destination directory.
- Completed files get the Mark-of-the-Web, so SmartScreen treats them exactly as it would a
  browser download. Nothing is ever auto-executed.
- Signed URLs are bearer credentials, so stored history and logs keep parameter *names* and
  discard every value.
- `https → http` redirects are refused by default; credentials are stripped across origins.

SwiftLoad is an ordinary HTTP/HTTPS download manager. It does not and will not attempt to bypass
DRM, authentication, paywalls, access controls, or rate limits — `Retry-After` is obeyed, and a
rate limit is answered by *reducing* concurrency, never by opening more connections.

---

## Documentation

| | |
|---|---|
| [`docs/PLAN.md`](docs/PLAN.md) | Full architecture and implementation plan |
| [`docs/PROGRESS.md`](docs/PROGRESS.md) | What is built, what is measured, what is next |
| [`benchmarks/REPORT.md`](benchmarks/REPORT.md) | Generated benchmark report |

## Layout

```
crates/swiftload-core/        engine — no UI dependencies (CI-enforced)
crates/swiftload-testserver/  deterministic, deliberately adversarial HTTP server
crates/swiftload-cli/         headless driver
crates/swiftload-bench/       benchmark matrix and report generator
```

## Licence

MIT OR Apache-2.0
