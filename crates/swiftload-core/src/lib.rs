//! SwiftLoad download engine.
//!
//! This crate is deliberately free of any UI dependency: it is driven identically by the
//! CLI, the benchmark harness, and (later) the desktop shell. See `docs/PLAN.md`.

pub mod config;
pub mod fsx;
pub mod http;
pub mod store;
pub mod task;
pub mod util;
