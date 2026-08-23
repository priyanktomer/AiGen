# SwiftLoad — implementation progress

**Last updated:** end of Session 1 work. Branch `claude/swiftload-plan-oxczwz`.

Read this first if you are resuming in a fresh session. The full architecture plan is
[`docs/PLAN.md`](PLAN.md); this file records what is actually built, what the benchmarks
measured, and precisely what to do next.

---

## Status at a glance

| | |
|---|---|
| Code | ~12,400 lines, 4 crates |
| Tests | **252 passing**, 0 failing |
| Lint | `cargo clippy --workspace --all-targets` clean, `cargo fmt --check` clean |
| Platform | Windows is the release target; Unix arms exist in `fsx/` so CI and this container can build and test |
| Session 1 (engine) | **Complete** — adaptive concurrency now meets its target on a warm host |
| Session 2 (Tauri + React UI) | **Not started** |
| Session 3 (perf/hardening) | Partially done early — benchmark harness exists and has been run |

Quick verification:

```bash
cargo test --workspace          # 252 tests
cargo clippy --workspace --all-targets
cargo run -p swiftload-testserver -- --port 8080
cargo run -p swiftload-cli -- get "http://127.0.0.1:8080/plain/alpha/33554432" --out /tmp/dl
cargo run --release -p swiftload-bench -- --reps 3 --size 67108864
```

---

## What is built

```
crates/
├─ swiftload-core/          the engine — no UI dependencies
│  ├─ util/                 RangeSet, filename sanitizer, URL redaction, speed meter, backoff
│  ├─ http/                 per-worker clients, h2 windows, manual redirects, error classes
│  ├─ fsx/                  positional writes, preallocation, sparse files, Mark-of-the-Web
│  ├─ store/                SQLite (WAL), checkpoints, URL history, host profiles
│  └─ task/                 probe · plan · governor · worker · writer · identity · orchestration
├─ swiftload-testserver/    deterministic + adversarial HTTP server
├─ swiftload-cli/           headless driver (get / resume / refresh-url / list / links / …)
└─ swiftload-bench/         benchmark matrix + report generator
```

### Behaviour that is verified by tests, not just written

- **Byte-exact downloads.** Every integration test that produces a file compares it against the
  server's deterministic content function. Right-sized-but-wrong-content is the failure mode that
  matters, and only content checks catch it.
- **Crash recovery.** `SIGKILL` at four different points, plus a single-connection run, each
  followed by resume and a byte-exact result — driven through the real binary, so nothing gets a
  chance to flush on the way out.
- **Range liars.** A server that advertises `Accept-Ranges` then ignores `Range` is caught before
  any byte is written, because writing a full-file body at a segment offset silently corrupts.
- **URL refresh.** Expired signed link → fresh link → resume with progress preserved. Decoy
  (same name, same size, different bytes) rejected on content. Size mismatch rejected without
  spending traffic. Rotated ETag verified by content, then confirmed by the user. Corrupted local
  partial detected rather than resumed over.
- **Redaction.** Signed tokens cannot reach stored URL history or logs; parameter names survive.
- **Path safety.** Property test asserts no server-supplied filename can escape the destination.

### Notable design decisions already implemented

- One `reqwest::Client` **per worker**, so HTTP/2 cannot multiplex "parallel" segments onto a
  single TCP connection. A test counts server-side accepts to prove it.
- Single writer task per download, bounded channel, `write → fsync → checkpoint` ordering.
- Segment state persisted as one varint-encoded BLOB in one row — atomic by construction.
- Claim-based ranges with cooperative shrink-and-steal, not fixed N-way splits.
- Content verification runs on **every** URL refresh, even when headers match perfectly
  (deviation from the approved plan, recorded in `docs/PLAN.md` §D.10.3).

---

## What the benchmarks measured

64 MB per run, 3 interleaved repetitions, local shaped server (`benchmarks/REPORT.md`):

| scenario | 1 conn | 8 | 16 | adaptive (cold) | adaptive (warm) |
|---|---|---|---|---|---|
| **per-connection cap** — parallelism should scale | 1.00× | 8.27× | **17.17×** | 9.42× | **17.13×** |
| **shared total cap** — parallelism should do nothing | 1.00× | 0.98× | 0.98× | 0.98× | 0.98× |
| **unshaped loopback** — measures overhead only | 1.00× | ~0.96× | — | ~1.0× | ~1.0× |

The thesis holds in both directions: near-linear scaling where the server caps per connection,
and **nothing at all** where the cap is shared. In the shared-cap case adaptive correctly settles
at 8 rather than climbing to 16, and the warm run does not blow past it either — it remembered
that the pipe, not the server, was the limit.

Five real bugs were found by running these benchmarks:

1. The governor ramped while disk-bound.
2. A token bucket's idle burst inflated the 1-connection baseline.
3. The report hardcoded 8 connections as "the best fixed level", flattering adaptive whenever 16 won.
4. Measurement windows were fixed-length in time, so a download shorter than a full ramp finished
   before any decision was made.
5. The governor only sampled on `drive()`'s 250 ms UI tick, putting a hard floor of ~500 ms on
   every ramp step regardless of how fast its window was ready.

## The adaptive-concurrency gap — resolved

**Cold start:** 0.35× → **0.55×** of the best fixed level, via three changes:

- Measurement windows now close on **whichever comes first, time or data**
  (`WARMUP_BYTES_PER_CONN` / `DWELL_BYTES_PER_CONN` in `task/governor.rs`). A fixed 400 ms warmup
  is sensible on a slow link and pure waste on a fast one, where it was the dominant cost of
  exploring.
- Governor sampling decoupled from UI progress: 50 ms for decisions, 250 ms for the progress
  callback (`GOVERNOR_TICK` / `PROGRESS_EVERY` in `task/mod.rs`).
- Dwell scales to the remaining transfer.

**Warm start:** **1.00×** of the best fixed level — the bar is met. `HostHint` in `task/mod.rs`
carries what a previous download from the same host settled on; `initial_conns` starts there
instead of at four. The CLI reads it from `host_profiles` before a download and records
`settled_conns` / `saturation_detected` afterwards, but only when the governor was actually in
charge — a user-pinned `--conns` says nothing about what the server would allow.

The residual cold-start cost is **intrinsic, not a defect**: the governor must try a level before
it can know it is better, and on a one-second download the trying is most of the transfer. On a
256 MB file cold adaptive reaches ~0.80× unaided. The report states this plainly rather than
hiding it.

## Remaining work after that

1. **CI** — `.github/workflows/ci.yml`: fmt, clippy `-D warnings`, `cargo test --workspace`, and
   the smoke benchmark as a throughput/RSS regression gate.
2. **README** — the real numbers, including the shared-cap row where parallelism does nothing.
   Do not publish a table that only shows wins.
3. **Session 2** — Tauri v2 shell + React UI, per `docs/PLAN.md` §F. The engine API it needs
   (`download`, `Progress`, `validate_replacement_url`, `Store`) is already in place and is what
   the CLI drives today.
4. Not yet implemented from the plan: the scheduler/queue across multiple simultaneous downloads,
   the app-wide probe token, the `events` diagnostics table, and `INetworkListManager`
   connectivity events.

---

## Gotchas worth knowing

- **Timing-based tests are flaky under parallel load.** Two were rewritten to key off observed
  checkpoint progress instead of wall-clock sleeps. Do not reintroduce `sleep`-then-cancel.
- **Progress is only observable at checkpoint granularity** (8 MiB or 5 s, whichever first). A
  test that wants to stop at a finer boundary than that will overshoot.
- **Preallocation means file length cannot validate a checkpoint** — the part file is always full
  size. Existence is checked before opening instead; see the phantom-checkpoint handling in
  `task/mod.rs`.
- **The container has an HTTPS proxy configured.** Loopback requests bypass it via `no_proxy`
  in `http/client.rs`; tests that build their own `reqwest::Client` must call `.no_proxy()`.
- Benchmarks are debug-slow; always run `swiftload-bench` with `--release`.
