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
| Tests | **248 passing**, 0 failing |
| Lint | `cargo clippy --workspace --all-targets` clean, `cargo fmt --check` clean |
| Platform | Windows is the release target; Unix arms exist in `fsx/` so CI and this container can build and test |
| Session 1 (engine) | **Complete**, with one open performance gap (see below) |
| Session 2 (Tauri + React UI) | **Not started** |
| Session 3 (perf/hardening) | Partially done early — benchmark harness exists and has been run |

Quick verification:

```bash
cargo test --workspace          # 248 tests
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

| scenario | 1 conn | 8 conns | 16 conns | adaptive |
|---|---|---|---|---|
| **per-connection cap** — parallelism should scale | 1.00× | ~8.6× | **17.1×** | ~6× |
| **shared total cap** — parallelism should do nothing | 1.00× | 0.98× | 0.98× | ~0.97× |
| **unshaped loopback** — measures overhead only | 1.00× | ~0.96× | — | ~1.0× |

The core thesis holds in both directions: near-linear scaling where the server caps per
connection, and **nothing at all** where the cap is shared.

Three real bugs were found by running these benchmarks: the governor ramped while disk-bound; a
token bucket's idle burst inflated the 1-connection baseline; and the report itself hardcoded
8 connections as "the best fixed level", flattering adaptive whenever 16 won.

---

## ⚠ Open gap — start here

**Adaptive concurrency does not reach the best fixed level under a per-connection cap.**
It reaches roughly 0.35× of fixed-16. `docs/PLAN.md` §J requires within 10%, so this is a
release blocker, not a polish item. The report now says so in plain language rather than hiding it.

**Diagnosis.** The governor *does* ramp — peak connections reach 16 — but too late for the
throughput to pay off. Each level costs a warmup plus a dwell before it can be judged, so a full
ramp of 4 → 8 → 16 spends several seconds measuring while the download is already finishing.
Scaling the dwell to the remaining transfer (`Governor::effective_dwell` in
`crates/swiftload-core/src/task/governor.rs`) helped — peak connections went 4 → 7 → 16 — but did
not close the throughput gap.

**Suggested fix, in order of expected value:**

1. **Do not pay a full baseline dwell before the first ramp.** Stepping up from the starting
   level is nearly always safe. Spawn the next level almost immediately and judge both together,
   rather than measuring the start level in isolation first.
2. **Ramp more aggressively than doubling while evidence is good.** If per-connection throughput
   holds steady across a step, the server is per-connection capped and the next step can be
   larger than 2×.
3. **Consult the host profile.** `store::Store::host_profile` already persists
   `best_observed_conns`; `initial_conns` in `crates/swiftload-core/src/task/mod.rs` does not read
   it yet. Wiring it up means the second download from a host starts where the first finished
   instead of rediscovering it.
4. Re-run `swiftload-bench` and confirm the 10% bar in the generated report.

Governor tests live in `crates/swiftload-core/src/task/governor.rs` and run against synthetic
throughput traces with no network, so changes can be iterated quickly. Keep
`settles_rather_than_oscillating` and `adaptive_lands_near_the_best_fixed_level` passing.

---

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
