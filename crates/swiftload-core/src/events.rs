//! The engine → UI contract.
//!
//! Everything the UI can learn about a download while it runs arrives as one of these. The
//! types live here rather than in the shell so the contract is versioned with the engine that
//! produces it, and so the CLI and the benchmark harness can consume the same stream.
//!
//! # Why the shapes look like this
//!
//! **Progress is a batch, not a message per download.** One `Progress` event carries a
//! snapshot for *every* active download. A WebView charges for each IPC crossing, so N
//! downloads must not mean N times the traffic — the cost of a tick should depend on the tick
//! rate, not on how much is running.
//!
//! **Connections are a separate event.** The per-connection table is the largest payload the
//! engine produces and almost nobody is looking at it: it is only meaningful while a Details
//! drawer is open. Keeping it out of `Progress` means the common case stays small, and the
//! expensive case is opt-in via [`crate::manager::Manager::subscribe_connections`].
//!
//! **`Notice` is separate from `State`.** A status change is something the UI *renders*; a
//! notice is something the user must *answer* — an expired link, a full disk, a name
//! collision. Merging them would leave the UI guessing which transitions deserve a prompt.
//!
//! **No `Duration`, no `Instant`, no `PathBuf` on the wire.** Serde renders `Duration` as a
//! struct of seconds and nanos, which is a poor fit for TypeScript, and `Instant` cannot be
//! serialised at all. Times cross as whole seconds and paths as strings.

use crate::{
    store::models::DownloadStatus,
    task::{worker::WorkerState, ConnectionInfo, Progress},
};
use serde::{Deserialize, Serialize};

/// One download's state at a moment in time.
///
/// Cheap to produce and safe to drop: every field is absolute rather than a delta, so a UI
/// that misses a tick shows slightly stale numbers instead of drifting permanently.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ProgressSnapshot {
    pub id: String,
    pub status: DownloadStatus,
    pub bytes_done: u64,
    /// `None` when the server never told us the size. The UI must render an indeterminate bar
    /// rather than inventing a denominator.
    pub total: Option<u64>,
    pub current_bps: u64,
    pub avg_bps: u64,
    pub peak_bps: u64,
    /// Whole seconds remaining. `None` when there is no size, or when the current rate is zero
    /// and any estimate would be a fabrication.
    pub eta_secs: Option<u64>,
    pub conns: usize,
    pub retries: u64,
}

impl ProgressSnapshot {
    /// Build a snapshot from an engine [`Progress`] reading.
    pub fn from_progress(id: &str, status: DownloadStatus, p: &Progress) -> Self {
        Self {
            id: id.to_string(),
            status,
            bytes_done: p.bytes_done,
            total: p.total,
            current_bps: p.current_bps,
            avg_bps: p.avg_bps,
            peak_bps: p.peak_bps,
            eta_secs: p.eta.map(|d| d.as_secs()),
            conns: p.conns,
            retries: p.retries,
        }
    }

    /// A snapshot for a download that is not running, built from what the store already knows.
    ///
    /// The UI needs a row for a paused or queued download too, and it should not have to
    /// special-case the absence of live numbers.
    pub fn idle(id: &str, status: DownloadStatus, bytes_done: u64, total: Option<u64>) -> Self {
        Self {
            id: id.to_string(),
            status,
            bytes_done,
            total,
            current_bps: 0,
            avg_bps: 0,
            peak_bps: 0,
            eta_secs: None,
            conns: 0,
            retries: 0,
        }
    }
}

/// One worker's state, for the Details drawer's connection table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConnectionSnapshot {
    pub id: usize,
    pub bytes: u64,
    pub bps: u64,
    pub retries: u64,
    /// The half-open byte range this worker currently owns, if it holds a claim.
    pub claim_start: Option<u64>,
    pub claim_end: Option<u64>,
    pub state: WorkerState,
}

impl From<&ConnectionInfo> for ConnectionSnapshot {
    fn from(c: &ConnectionInfo) -> Self {
        Self {
            id: c.id,
            bytes: c.bytes,
            bps: c.bps,
            retries: c.retries,
            claim_start: c.claim.map(|(s, _)| s),
            claim_end: c.claim.map(|(_, e)| e),
            state: c.state,
        }
    }
}

/// The connection table for one download.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ConnectionsSnapshot {
    pub id: String,
    pub connections: Vec<ConnectionSnapshot>,
}

/// A status transition worth re-rendering for.
///
/// Low frequency by construction: a download changes status a handful of times in its life,
/// so the UI can afford to refetch the full record when one arrives.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct StateChanged {
    pub id: String,
    pub status: DownloadStatus,
    /// Present when the transition was a failure.
    pub error: Option<String>,
}

/// Something that needs the user, phrased in terms the user can act on.
///
/// Deliberately not an error type. `ErrorClass` says what went wrong technically; a `Notice`
/// says what the user is being asked to decide. The two do not map one-to-one — most errors
/// are retried silently and never surface here at all.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Notice {
    /// The signed link died mid-download. The partial file is intact and resumable — this is
    /// the entry point to the §D.10 refresh flow, not a failure.
    UrlExpired {
        id: String,
        filename: String,
        bytes_done: u64,
        total: Option<u64>,
    },
    /// Ran out of room. Reported with the shortfall so the UI can say how much to free.
    DiskFull {
        id: String,
        filename: String,
        needed_bytes: u64,
    },
    /// Terminal for this attempt. Retry is still available; the partial is kept.
    Failed {
        id: String,
        filename: String,
        message: String,
    },
    /// Finished and verified. Carries the final path so "Open Folder" needs no extra call.
    Completed {
        id: String,
        filename: String,
        path: String,
        /// Present when `hash_on_complete` was set.
        sha256: Option<String>,
    },
}

impl Notice {
    /// The download this notice concerns.
    pub fn id(&self) -> &str {
        match self {
            Notice::UrlExpired { id, .. }
            | Notice::DiskFull { id, .. }
            | Notice::Failed { id, .. }
            | Notice::Completed { id, .. } => id,
        }
    }
}

/// Everything the engine emits.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EngineEvent {
    /// Every active download, a few times a second.
    Progress { downloads: Vec<ProgressSnapshot> },
    /// A status transition.
    State(StateChanged),
    /// Per-connection detail, emitted only while someone is subscribed.
    Connections(ConnectionsSnapshot),
    /// Needs the user.
    Notice(Notice),
}
