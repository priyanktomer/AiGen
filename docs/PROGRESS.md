# SwiftLoad — implementation progress

**Last updated:** end of Session 2 work. Branch `claude/session-verify-continue-neeav9`.

Read this first if you are resuming in a fresh session. The full architecture plan is
[`docs/PLAN.md`](PLAN.md); this file records what is actually built, what has been measured,
what is deliberately different from the plan, and precisely what to do next.

---

## Status at a glance

| | |
|---|---|
| Code | ~15,900 lines of Rust (4 engine crates + the shell), ~1,800 TypeScript/TSX, ~320 CSS, plus ~220 generated binding lines |
| Tests | **281 passing**, 0 failing |
| Lint | `cargo clippy --workspace --all-targets -D warnings` clean, `cargo fmt --check` clean |
| UI | `tsc --noEmit` clean, production bundle builds (192 KB, 60 KB gzipped) |
| Platform | Windows is the release target. The engine, the shell and the UI all build and run on Linux, which is how CI and this container exercise them |
| Session 1 (engine) | **Complete and re-verified** |
| Session 2 (app + UI) | **Functionally complete; the Windows-only half is not done** — see below |
| Session 3 (perf/hardening) | Benchmark harness exists and has been run |

Quick verification:

```bash
cargo test --workspace          # 281 tests
cargo clippy --workspace --all-targets -- -D warnings
cd app/ui && npm ci && npm run build
cd app/src-tauri && cargo build
```

---

## Session 1 was re-verified before Session 2 began

Not taken on trust from the previous session's notes:

- `cargo test --workspace` — 252 passing, 0 failing, matching what was recorded.
- `cargo clippy --workspace --all-targets` and `cargo fmt --check` — both clean.
- A real end-to-end download through the built binary against the test server: 32 MB,
  byte-exact, 12.5 MB/s.

---

## What is built

```
crates/
├─ swiftload-core/          the engine — no UI dependencies (CI-enforced)
│  ├─ util/                 RangeSet, filename sanitizer, URL redaction, speed meter, backoff
│  ├─ http/                 per-worker clients, h2 windows, manual redirects, error classes
│  ├─ fsx/                  positional writes, preallocation, sparse files, Mark-of-the-Web
│  ├─ store/                SQLite (WAL), checkpoints, URL history, host profiles, settings
│  ├─ task/                 probe · plan · governor · worker · writer · identity · orchestration
│  ├─ events.rs             ★ the engine → UI contract
│  ├─ scheduler.rs          ★ queue, N-concurrent, host budget, app-wide probe token
│  └─ manager.rs            ★ the application API the shell drives
├─ swiftload-testserver/    deterministic + adversarial HTTP server
├─ swiftload-cli/           headless driver (get / resume / refresh-url / list / links / …)
└─ swiftload-bench/         benchmark matrix + report generator

app/
├─ src-tauri/               ★ Tauri v2 shell: commands, EventPump, capabilities, icons
└─ ui/                      ★ React + TypeScript: views, dialogs, details drawer
```

### Behaviour that is verified by tests, not just written

Everything from Session 1 still holds — byte-exact downloads, `SIGKILL` crash recovery, range
liars caught before a byte is written, URL refresh with decoys rejected, redaction, path safety.
Session 2 adds:

- **The queue actually throttles.** Four downloads against a limit of two never exceed two
  concurrently, and all four still finish.
- **Pause keeps progress**, and resume finishes byte-exact.
- **Cancel keeps the partial; remove deletes it only when asked.** Removal of a *running*
  download deletes after the writer has stopped, never under it.
- **An interrupted `Active` row comes back as `Paused`**, not `Failed` — nothing went wrong with
  it and the partial is intact.
- **Progress is one batched event** covering every active download, and an idle app emits
  nothing at all.
- **Connection tables reach only subscribers**, and stop when the drawer closes.
- **The whole §D.10 refresh flow through the manager**: expired link → fresh link → verified in
  under a megabyte → resumed → byte-exact, with no signed token reaching the stored history.

### Verified by running the app, not only by testing it

Under a virtual display, the built shell launches, opens its store, and drives a real 100 MB
download from the adversarial test server through the actual IPC: the probe preview returns the
server's true metadata, eight connections appear in the details drawer each holding a distinct
byte range, progress and ETA update live, the completion notice fires, and the finished file is
**byte-exact against an independent fetch of the same content**.

---

## A real bug found and fixed in Session 1 code

One new manager test failed roughly one run in five, rejecting a URL refresh — the engine
claiming the bytes on disk did not match a replacement link that was in fact the same file.

`choose_windows` located a verification window's start inside the completed set but never
clamped its **length** to the span it landed in. Completed bytes are not a contiguous prefix — a
segmented download leaves gaps — and the part file is preallocated, so a window running past the
end of a span read preallocated padding instead of short-reading. That padding was compared
against real content from the server and reported as a content mismatch.

The user-visible effect: someone with a fragmented partial download supplies a perfectly valid
fresh link, is told it is a different file, and is refused — and a rejection deliberately has no
"resume anyway", so they restart from zero. Exactly the outcome §D.10 exists to prevent.

It survived Session 1 because the existing coverage used two wide spans and a single download
id, a shape where a pick almost never lands within a window's length of a boundary. The
replacement tests use a realistic fragmented layout and sweep 2,000 ids; both fail against the
old code and pass against the fixed code, while the old test stays green either way.

Fixed alongside it: a non-206 answer to a verification fetch was reported as `ContentMismatch`,
so a server hiccup accused the link of being a different file. "Could not check" is now its own
reject reason with its own wording.

**The lesson worth carrying forward:** a flaky test was a real bug, not a timing artifact. The
temptation to re-run it until it passed would have shipped this.

---

## What the benchmarks measured

Unchanged from Session 1 (`benchmarks/REPORT.md`). 64 MB per run, 3 interleaved repetitions:

| scenario | 1 conn | 8 | 16 | adaptive (cold) | adaptive (warm) |
|---|---|---|---|---|---|
| **per-connection cap** — parallelism should scale | 1.00× | 8.27× | **17.17×** | 9.42× | **17.13×** |
| **shared total cap** — parallelism should do nothing | 1.00× | 0.98× | 0.98× | 0.98× | 0.98× |
| **unshaped loopback** — measures overhead only | 1.00× | ~0.96× | — | ~1.0× | ~1.0× |

The thesis holds in both directions. Warm adaptive meets the bar; the residual cold-start cost is
intrinsic — the governor must try a level before it can know it is better.

---

## Deliberate deviations from the plan

Recorded here rather than silently absorbed:

1. **§B — `Manager` is an `Arc<Manager>` with methods, not a task behind a command mpsc.** The
   serialisation a channel buys is already provided by two short-lived locks; the registry and
   event fan-out it was drawn for are both present. Only the indirection — a request enum, a
   reply channel and timeout semantics per call — is gone.
2. **§E — the shell lives in `app/src-tauri` and is *excluded* from the cargo workspace.**
   Building it needs the WebView system libraries, which the engine, CLI and benchmarks do not.
   Excluding it keeps `cargo test --workspace` runnable anywhere and lets CI install those
   libraries only for the job that needs them.
3. **§F.6 — the Details drawer has no Timeline tab.** It would render the `events` diagnostics
   table, which is not implemented. An empty tab is worse than no tab; it returns with the table.
4. **`u64` crosses the IPC as TypeScript `number`, not `bigint`.** `ts-rs` defaults to `bigint`,
   which is right for a binary channel and wrong for this one: the IPC is JSON, so `JSON.parse`
   yields a `number` whatever the type file claims. The ceiling is 2^53 bytes — nine petabytes.
5. **Queue priority is per-session, not persisted.** Restarting re-queues by creation time.
   Persisting it needs a schema migration and is not worth one yet.

---

## What is left

### Session 2 items not done — all of them Windows-only

These cannot be done or checked from this container, and none is started:

1. **Installer.** NSIS + MSI are configured in `tauri.conf.json` but have never been built.
   `tauri-bundler` produces Windows installers only on Windows.
2. **Clean-VM install test** — install, download 1 GB, pause/resume/cancel, kill and relaunch,
   uninstall leaving no orphans. This is the §F Session 2 exit criterion and it is unmet.
3. **Mark-of-the-Web on a real file.** The code path exists and is unit-tested; that it makes
   SmartScreen behave has not been observed.
4. **Idle RSS < 120 MB and CPU < 2% idle** — measured on Windows, against the real WebView2.

### Session 2 items not done for other reasons

5. **Security pass** (§F.10): `cargo-fuzz` over `filename.rs`, `cargo audit`, `cargo deny`. The
   Tauri CSP and the capability allowlist *are* written and deliberately narrow — the app's file
   and network access all goes through engine commands, and the plugin surface is limited to
   picking a folder and handing a path to the shell.
6. **Tray icon, notifications, autostart, categories as a user-editable setting.** The Settings
   view covers the engine's own settings; app-level preferences (theme override, notifications,
   start-with-Windows) have no storage yet.

### Still unimplemented from the plan generally

- The `events` diagnostics table (and so the Timeline tab).
- `INetworkListManager` connectivity events — Wi-Fi reconnect currently waits for a retry rather
  than resuming in ~1 s.
- Mid-flight rebalancing of a running download's connection ceiling. The scheduler computes a
  fair share at admission and cannot revise it, because `task::download` takes its ceiling once.
  When one of two downloads on a host finishes, the other keeps its half share.
- Session 3 in full: the complete benchmark matrix, governor tuning from measured data, the 24 h
  soak, the §K edge-case matrix, Tier 2/3 verification, redaction fuzzing.

---

## Gotchas worth knowing

- **A flaky test here has twice been a real bug.** Investigate before re-running.
- **Timing-based tests are flaky under parallel load.** Key off observed checkpoint progress,
  never `sleep`-then-cancel.
- **Progress is only observable at checkpoint granularity** (8 MiB or 5 s, whichever first). A
  test that wants to interrupt a download *in the middle* needs a file several checkpoints long —
  otherwise the first checkpoint it sees is also the last. `BIG` in `tests/manager.rs` exists for
  exactly this.
- **Shape interrupt tests with `per=total`, not `per=conn`.** Under a per-connection cap the
  aggregate rate depends on how far the governor has ramped, so the download's duration is
  unpredictable and the test flakes.
- **`RangeSet::spans()` returns `(start, len)`, not `(start, end)`.** Reading it as the latter
  underflows.
- **Preallocation means file length cannot validate a checkpoint** — and it means a read past a
  completed span returns padding rather than short-reading. That is what caused the bug above.
- **The container has an HTTPS proxy configured.** Loopback bypasses it via `no_proxy` in
  `http/client.rs`; tests building their own `reqwest::Client` must call `.no_proxy()`.
- **Benchmarks are debug-slow**; always run `swiftload-bench` with `--release`.
- **The TypeScript bindings are generated and checked in.** After changing any type that crosses
  the IPC, regenerate them or CI fails:
  ```bash
  TS_RS_EXPORT_DIR="$PWD/app/ui/src/api/bindings" \
    cargo test -p swiftload-core --features ts export_bindings
  ```
- **Building the shell on Linux needs** `libwebkit2gtk-4.1-dev libayatana-appindicator3-dev
  librsvg2-dev libxdo-dev patchelf`. CI installs these in the `desktop` job only.
- **To run the app headlessly** (how the end-to-end check above was done):
  ```bash
  Xvfb :99 -screen 0 1280x820x24 &
  cd app/ui && npx vite preview --port 1420 --strictPort &   # debug builds load devUrl
  DISPLAY=:99 WEBKIT_DISABLE_COMPOSITING_MODE=1 app/src-tauri/target/debug/swiftload-desktop
  ```
  Software rendering under Xvfb leaves occasional repaint artifacts in screenshots. They are not
  UI bugs and do not appear once the region repaints.
