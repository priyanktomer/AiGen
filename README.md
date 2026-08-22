# SwiftLoad

A free, modern Windows download manager — intended as a genuinely fast, reliable
alternative to Internet Download Manager, with performance that can be objectively
benchmarked rather than merely asserted.

> **Status: planning complete, implementation not started.**
> This repository currently contains the implementation plan only.

## The plan

**[`docs/PLAN.md`](docs/PLAN.md)** is the complete architecture and implementation
plan. It is written to be handed directly to an implementation session.

Contents:

| Section | |
|---|---|
| §0 | Framing: when parallel downloading actually helps, and when it doesn't |
| §A | Recommended stack (Rust engine + Tauri v2 + React) and why, incl. rejected options |
| §B | Architecture and the UI ↔ engine contract |
| §C | Data model (SQLite schema) |
| §D | Download algorithm — probe, segmentation, the adaptive Governor, retries, crash recovery, resume with a replaced URL |
| §E | Repository structure |
| §F | Session-by-session implementation plan |
| §G | Testing strategy |
| §H | Benchmark design and honest performance claims |
| §I | Risks |
| §J | Definition of Done |
| §K | Edge-case behaviour matrix |
| §L | MVP vs. future roadmap |
| §M | Requirements pushed back on, with reasoning |

## Design principles the plan commits to

- **Never silently corrupt a download.** Range responses are verified before any byte
  is written; checkpoints may only claim data already `fsync`ed; a replacement URL must
  prove it serves the same bytes before it is allowed to append to an existing file.
- **Adaptive, not brute-force, concurrency.** The engine measures whether additional
  connections actually improve throughput and distinguishes *why* a plateau occurred —
  a per-connection cap, a saturated link, a throttling server, or a disk bottleneck all
  demand different responses. It does not blindly open 32 connections.
- **No unfalsifiable performance claims.** The benchmark harness and its conditions ship
  with the product, including the scenarios where parallelism provides no benefit.
- **Security is not traded for convenience.** TLS validation is always on and there is no
  setting to disable it. Nothing is auto-executed. Downloads are marked with the
  Mark-of-the-Web so SmartScreen behaves as it would for a browser download.

## Non-goals

SwiftLoad is an ordinary HTTP/HTTPS download manager. It does not, and will not, attempt
to bypass DRM, authentication, paywalls, access controls, or server rate limits.
