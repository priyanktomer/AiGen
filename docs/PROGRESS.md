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
| Tests | **293 passing**, 0 failing |
| Lint | `cargo clippy --workspace --all-targets -D warnings` clean on Linux *and* Windows targets, `cargo fmt --check` clean |
| UI | `tsc --noEmit` clean, production bundle builds (196 KB, 61 KB gzipped) |
| Supply chain | `cargo deny` and `cargo audit` clean on both dependency graphs |
| Fuzzing | 2 targets, clean at 7.9M and 2.5M executions — after fixing the four defects they found |
| Platform | Windows is the release target. Everything cross-compiles to Windows, including a working NSIS installer; the Windows-only code paths are compile-checked and clippy-clean in CI |
| Session 1 (engine) | **Complete and re-verified** |
| Session 2 (app + UI) | **Complete except what needs a Windows machine** — see below |
| Session 3 (perf/hardening) | Benchmark harness exists and has been run |

Quick verification:

```bash
cargo test --workspace          # 293 tests
cargo clippy --workspace --all-targets -- -D warnings
cd app/ui && npm ci && npm run build
cd app/src-tauri && cargo build

# The shipped artefact, cross-built from Linux:
cd app/src-tauri && cargo tauri build --target x86_64-pc-windows-gnu --bundles nsis
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

## Five real bugs found in Session 1 code, and fixed

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

### And four more, from fuzzing `filename.rs`

`sanitize` is the highest-consequence pure function in the engine: it turns a name chosen by
whoever runs the server into a path we then write to. It had unit tests, property tests, and
four defects.

1. **A Windows reserved device name was reachable.** Device names ("CON", "COM5") were escaped
   *before* truncation, so "cOM5" followed by 200 spaces slipped past the check — its stem was
   the whole padded string — and truncation handed back exactly "cOM5", which Windows resolves
   to the serial port. A crafted `Content-Disposition` was enough. The check now runs last, and
   trims the stem before comparing, because "con     .txt" is CON too.
2. **Truncation could re-create a trailing space or dot**, the exact character the function
   removes earlier precisely because Windows strips it silently and the recorded name then
   stops matching the file on disk.
3. **`parse_content_disposition` returned unsanitised names.** Public, filename-shaped, and
   already being stored raw on `ProbeResult` by `probe.rs`. Nothing consumed that field yet,
   which is the only reason it was not live.
4. **`sanitize` was not idempotent** — and that is what let (1) hide. Removing a control
   character exposed trailing Unicode whitespace that only a second pass would trim, so a name
   compared against its own re-sanitised form did not match itself.

**The lesson:** property tests over random ASCII missed all four. Coverage-guided fuzzing over
arbitrary *bytes* found them in minutes. The invariant that did the most work was the least
obvious one — idempotence — because it turns "the output is fine" into "the output is a fixed
point", and fixed points are where this kind of ordering bug shows up.

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
5. **Closing the window quits; it does not hide to the tray.** Hiding on close is the
   convention and it is also how an app ends up running for weeks unnoticed. This product
   refuses to auto-open files or auto-resume downloads, and silently continuing to run is the
   same class of decision. The tray is for while it is running. Close-to-tray belongs behind an
   explicit setting.
6. **Interface preferences live in `localStorage`, not the database.** Theme and notification
   toggles change what this window looks like, not what a download does; storing them beside
   connection limits would imply they travel with the download history. Start-with-Windows is
   the exception and is not stored at all — it is read from and written to the real OS
   registration, so the toggle cannot drift out of step with what actually happens at login.

---

## Measured: memory and CPU

Taken from the release build running under a virtual display, downloading from the local test
server. RSS is summed across the process tree and double-counts shared pages, so PSS is given
too — it is the honest number for a multi-process WebView.

| | RSS | PSS | CPU | throughput |
|---|---|---|---|---|
| Idle | 350 MB | 144 MB | **0.3%** of one core | — |
| Downloading | 421 MB | 270 MB | 67.3% of one core | 100 MB/s |
| **Engine alone** (CLI, no window) | **21 MB** | — | **46.9%** of one core | 100 MB/s |

Against the §F Session 2 exit criteria: **idle CPU passes** (0.3% against a 2% budget).
**Idle RSS does not** (350 MB against 120 MB), and **CPU under load does not** (67% against
10% of one core at 100 MB/s). Both misses are reported rather than explained away, but the
split above says where the work actually is:

- **The memory is entirely the web view.** The engine holds 21 MB while moving 100 MB/s. The
  other ~400 MB is WebKitGTK, in two processes. On Windows the web view is WebView2, which is a
  separate shared runtime and is not accounted the same way, so the Windows figure is unknown
  rather than known-bad — and it is unknown until somebody measures it there.
- **The CPU is the engine.** 46.9% of a core for the engine alone versus 67.3% for the whole
  app: the window costs about twenty points, and the engine costs the rest. A target of 10%
  needs a profiling pass on the engine's read/write path, not on the UI.

Two caveats that inflate these numbers and cannot be removed from this environment: the
transfer is loopback, so the kernel copies every byte twice on one machine, and the test server
sharing those four cores is a debug build. A real network path would look different. What the
numbers establish is the *shape* of the problem, which is what Session 3's profiling pass needs.

---

## What is left

### Needs a Windows machine — cannot be done or checked from here

1. **Install on a clean Windows 11 VM**, download a real 1 GB file, pause/resume/cancel, kill
   the app mid-download and relaunch, uninstall and confirm no orphans. This is §F's Session 2
   exit criterion and it is the one thing standing between "built" and "releasable".
   The installer exists and is described below; nobody has run it.
2. **Mark-of-the-Web against SmartScreen.** The code path is written, unit-tested, and now
   compile-checked for Windows, but that it makes SmartScreen behave has not been observed.
3. **RSS and CPU on WebView2** rather than WebKitGTK — see above.
4. **Code signing.** The installer is unsigned, so SmartScreen will warn. Signing needs a
   certificate and a Windows host; publishing a SHA-256 alongside the download is the minimum
   until then.
5. **MSI.** `tauri.conf.json` configures NSIS and MSI. NSIS cross-builds from Linux; MSI needs
   WiX and a Windows host, so only the NSIS installer has ever been produced.

### Done this session, and how it was verified

- **The Windows binary and installer.** `SwiftLoad_0.1.0_x64-setup.exe`, cross-built from
  Linux, carrying `swiftload-desktop.exe`, `WebView2Loader.dll` and an uninstaller. Before this
  the Windows-only half of `fsx/` — positional writes, sparse files, Mark-of-the-Web, the
  atomic rename — had never been compiled by anything. It compiles, and clippy is clean on it,
  and CI now keeps it that way.
- **The security pass** (§F.10). `cargo deny` and `cargo audit` on both graphs, two fuzz
  targets, and a written review of the CSP and the capability allowlist. Fuzzing found four
  real defects in `filename.rs`, including a Windows reserved device name reachable through a
  crafted `Content-Disposition`. All fixed, with tests that fail against the old code.
- **Tray, notifications, autostart, theme.** Plus the `events` table and the Timeline tab, and
  queue priority that survives a restart.

### Still unimplemented from the plan generally

- `INetworkListManager` connectivity events — a Wi-Fi reconnect currently waits for a retry
  rather than resuming in ~1 s.
- Mid-flight rebalancing of a running download's connection ceiling. The scheduler computes a
  fair share at admission and cannot revise it, because `task::download` takes its ceiling once.
  When one of two downloads on a host finishes, the other keeps its half share.
- Close-to-tray as an explicit setting. Closing the window quits, deliberately.
- Session 3 in full: the complete benchmark matrix, governor tuning from measured data, the
  profiling pass the numbers above call for, the 24 h soak, the §K edge-case matrix, Tier 2/3
  verification, and redaction fuzzing.

---

## Gotchas worth knowing

- **A flaky test here has twice been a real bug.** Investigate before re-running.
- **`cargo build --release` does not embed the front end.** Only `cargo tauri build` does; a
  plain release build still points at `devUrl` and shows "connection refused" with no dev
  server running. Costly to diagnose from a screenshot, trivial once known.
- **`pkill -f swiftload-…` matches the shell running it** and kills your own session. Use
  `pkill -x`, and note `comm` is truncated to 15 characters, so the exact name is
  `swiftload-deskt`.
- **The part file is preallocated to full size**, so watching its size on disk tells you
  nothing about progress. Read `bytes_done` from the database instead.
- **`aka.ms` is blocked by this container's proxy**, so `cargo-xwin` cannot fetch the MSVC CRT.
  The `x86_64-pc-windows-gnu` target needs no Microsoft download and builds the whole app,
  installer included.
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
