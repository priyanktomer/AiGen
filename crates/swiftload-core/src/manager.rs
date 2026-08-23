//! The application API: everything a front end needs, and nothing about how it is drawn.
//!
//! `swiftload-cli` drives [`crate::task::download`] directly because it runs exactly one
//! download and then exits. An app cannot: it has a queue, a budget shared between transfers,
//! a store that outlives any one of them, and a UI that has to be told what changed. That is
//! what this module is.
//!
//! # Shape
//!
//! `Manager` is an `Arc<Manager>` with ordinary methods, not a task behind a command channel.
//! `docs/PLAN.md` §B draws a command mpsc; a channel buys serialisation of mutations, which
//! two short-lived locks already provide, at the cost of making every call a round trip with
//! its own request enum, reply channel and timeout semantics. The registry and event fan-out
//! it was drawn for are both here — only the indirection is gone.
//!
//! # Rules this module keeps
//!
//! - **The lock is never held across an `await`.** Every critical section reads or writes the
//!   registry and ends. Downloads run in their own spawned tasks and report back.
//! - **The store is the truth; the registry is a cache of what is running.** A status the UI
//!   sees always came from a row, so a crash cannot leave the two disagreeing.
//! - **Stopping is an intent, not a status.** Pause, cancel and remove all cancel the same
//!   token; what the download *becomes* when it stops is decided by the intent recorded when
//!   the user asked, because the engine reports all three as `Cancelled`.
//! - **A signed URL never leaves this module unredacted** except through
//!   [`Manager::reveal_full_url`], which exists so that one deliberate user action is auditable
//!   rather than routine.

use crate::{
    config::{CollisionPolicy, RequestSpec, Settings},
    events::{
        ConnectionSnapshot, ConnectionsSnapshot, EngineEvent, Notice, ProgressSnapshot,
        StateChanged,
    },
    http::{errors::ErrorClass, headers::Validator},
    scheduler::{Limits, Priority, Scheduler},
    store::{
        models::{DownloadRecord, DownloadStatus, UrlSource, ValidationState},
        Store, StoreSink,
    },
    task::{
        download,
        identity::{validate_replacement_url, ResourceSignals, Verdict},
        probe::{probe, ProbeError, RangeSupport},
        DownloadError, DownloadRequest, HostHint, Progress,
    },
    util::redact,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, RwLock},
    time::Duration,
};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

/// UI tick. Four a second is fast enough to look live and slow enough that a WebView does not
/// spend its time parsing JSON.
const TICK: Duration = Duration::from_millis(250);
/// Connection tables go out on every other tick — half the rate of progress, and only to
/// whoever asked for them.
const CONNECTIONS_EVERY: u64 = 2;
/// Events buffered per subscriber before the slowest one starts missing them. Progress is
/// absolute rather than incremental, so a dropped tick costs a frame, never accuracy.
const EVENT_BUFFER: usize = 256;

#[derive(Debug, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "kind", content = "message", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ManagerError {
    #[error("no download with id {0}")]
    NotFound(String),
    #[error("{0}")]
    Store(String),
    #[error("{0}")]
    Probe(String),
    #[error("{0}")]
    Refresh(String),
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Invalid(String),
}

impl From<crate::store::StoreError> for ManagerError {
    fn from(e: crate::store::StoreError) -> Self {
        ManagerError::Store(e.to_string())
    }
}
impl From<ProbeError> for ManagerError {
    fn from(e: ProbeError) -> Self {
        ManagerError::Probe(e.to_string())
    }
}

type Result<T> = std::result::Result<T, ManagerError>;

// ---------------------------------------------------------------------------------------
// Command DTOs
// ---------------------------------------------------------------------------------------

/// What the Add dialog collects.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AddRequest {
    pub url: String,
    /// Falls back to the configured download directory.
    pub dest_dir: Option<String>,
    /// Overrides the server-supplied name.
    pub filename: Option<String>,
    pub category: Option<String>,
    /// `None` lets the governor decide, which is almost always the right answer.
    pub max_conns: Option<usize>,
    #[serde(default)]
    pub priority: Priority,
    /// False means "add to queue": it is enqueued but not jumped to the front.
    #[serde(default)]
    pub start_now: bool,
    pub expected_sha256: Option<String>,
}

/// One row in the downloads list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DownloadSummary {
    pub id: String,
    pub filename: String,
    /// Redacted. Signed links are bearer credentials and the list is the last place one should
    /// be visible.
    pub url_redacted: String,
    pub host: String,
    pub dest_dir: String,
    pub category: String,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub total_size: Option<u64>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub bytes_done: u64,
    pub status: DownloadStatus,
    pub validation_state: ValidationState,
    /// Set only while queued.
    pub queue_position: Option<usize>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub created_at: i64,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub completed_at: Option<i64>,
    pub error: Option<String>,
    /// Whether the server supports picking up where this left off.
    pub resumable: bool,
}

/// One row of the redacted link history, for the Links tab.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UrlHistoryRow {
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub seq: i64,
    pub url_redacted: String,
    pub host: String,
    pub source: String,
    pub outcome: String,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub bytes_done_at_swap: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub added_at: i64,
}

/// Everything behind the Details drawer.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct DownloadDetails {
    pub summary: DownloadSummary,
    pub original_url_redacted: String,
    pub final_url_redacted: String,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub http_version: Option<String>,
    pub accept_ranges: RangeSupport,
    pub max_connections: Option<usize>,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub retry_count: u64,
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub url_refresh_count: i64,
    pub part_path: String,
    /// How many contiguous spans the completed bytes form. One means a clean prefix; more
    /// means segmented progress, which is what the user is paying connections for.
    pub completed_spans: usize,
    /// Live, and empty whenever the download is not running.
    pub connections: Vec<ConnectionSnapshot>,
    pub url_history: Vec<UrlHistoryRow>,
}

/// What the Add dialog shows before committing to anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ProbePreview {
    pub url_redacted: String,
    pub final_url_redacted: String,
    pub filename: String,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub total_size: Option<u64>,
    pub resumable: bool,
    pub segmentable: bool,
    pub content_type: Option<String>,
    pub http_version: String,
    pub redirects: usize,
    /// Incomplete downloads that look like this one (§D.10.7). Weak evidence by construction —
    /// enough to offer a resume, never enough to assume one.
    pub existing: Vec<DownloadSummary>,
}

/// Settings as the Settings view wants them.
///
/// [`Settings`] is the engine's own type and carries `Duration`s, which serde renders as
/// `{ "secs": 15, "nanos": 0 }` — correct, and useless to bind a number input to. This is the
/// same settings expressed for a form, and it is the only shape the UI ever sees.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct UiSettings {
    pub download_dir: String,
    pub max_concurrent_downloads: usize,
    pub max_conns_per_download: usize,
    pub max_total_conns: usize,
    pub max_conns_per_host: usize,
    pub adaptive_concurrency: bool,
    pub max_retries_per_segment: u32,
    pub connect_timeout_secs: u64,
    pub read_idle_timeout_secs: u64,
    pub stall_timeout_secs: u64,
    pub max_redirects: usize,
    pub block_insecure_redirect: bool,
    pub paranoid_recovery: bool,
    pub hash_on_complete: bool,
    pub apply_motw: bool,
    pub collision_policy: CollisionPolicy,
    pub user_agent: String,
}

impl From<&Settings> for UiSettings {
    fn from(s: &Settings) -> Self {
        Self {
            download_dir: s.download_dir.to_string_lossy().to_string(),
            max_concurrent_downloads: s.max_concurrent_downloads,
            max_conns_per_download: s.max_conns_per_download,
            max_total_conns: s.max_total_conns,
            max_conns_per_host: s.max_conns_per_host,
            adaptive_concurrency: s.adaptive_concurrency,
            max_retries_per_segment: s.max_retries_per_segment,
            connect_timeout_secs: s.connect_timeout.as_secs(),
            read_idle_timeout_secs: s.read_idle_timeout.as_secs(),
            stall_timeout_secs: s.stall_timeout.as_secs(),
            max_redirects: s.max_redirects,
            block_insecure_redirect: s.block_insecure_redirect,
            paranoid_recovery: s.paranoid_recovery,
            hash_on_complete: s.hash_on_complete,
            apply_motw: s.apply_motw,
            collision_policy: s.collision_policy,
            user_agent: s.user_agent.clone(),
        }
    }
}

impl UiSettings {
    /// Fold this form back onto engine settings.
    ///
    /// Every limit is floored at one. A zero here would not mean "no limit" — it would mean a
    /// scheduler that admits nothing and an app that silently stops downloading.
    pub fn apply_to(&self, s: &mut Settings) {
        s.download_dir = PathBuf::from(&self.download_dir);
        s.max_concurrent_downloads = self.max_concurrent_downloads.max(1);
        s.max_conns_per_download = self.max_conns_per_download.max(1);
        s.max_total_conns = self.max_total_conns.max(1);
        s.max_conns_per_host = self.max_conns_per_host.max(1);
        s.adaptive_concurrency = self.adaptive_concurrency;
        s.max_retries_per_segment = self.max_retries_per_segment;
        s.connect_timeout = Duration::from_secs(self.connect_timeout_secs.max(1));
        s.read_idle_timeout = Duration::from_secs(self.read_idle_timeout_secs.max(1));
        s.stall_timeout = Duration::from_secs(self.stall_timeout_secs.max(1));
        s.max_redirects = self.max_redirects;
        s.block_insecure_redirect = self.block_insecure_redirect;
        s.paranoid_recovery = self.paranoid_recovery;
        s.hash_on_complete = self.hash_on_complete;
        s.apply_motw = self.apply_motw;
        s.collision_policy = self.collision_policy;
        if !self.user_agent.trim().is_empty() {
            s.user_agent = self.user_agent.clone();
        }
    }
}

/// The three result cards of the refresh dialog, plus the non-resumable case.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum RefreshOutcome {
    /// Same file, proven. Resume without asking.
    Verified,
    /// Same file by content, but the server's own tags changed. Ask.
    Confirm,
    /// Demonstrably a different file. There is no "resume anyway".
    Reject,
    /// Same file, but this link cannot resume — using it re-transfers everything.
    RestartOnly,
}

/// The read-only half of a URL swap. Produced without mutating anything.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RefreshReport {
    pub outcome: RefreshOutcome,
    /// Plain language, for the card. Never mentions ETags, 206s or byte offsets.
    pub message: String,
    pub windows_checked: usize,
    /// What proving identity cost. The feature exists to avoid re-transferring data, so this
    /// number is reported rather than buried.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub bytes_verified: u64,
    /// What resuming would keep.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub preserved_bytes: u64,
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub total_size: Option<u64>,
    pub resumable: bool,
}

// ---------------------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------------------

/// Why a running download is being stopped. The engine reports every stop as `Cancelled`, so
/// the difference between "pause" and "delete this" has to be recorded when the user asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Run,
    Pause,
    Cancel,
    Remove { delete_file: bool },
}

struct TaskHandle {
    cancel: CancellationToken,
    host: String,
    /// Latest reading from the download's own progress callback. The pump reads it; nobody
    /// waits on it.
    live: Arc<Mutex<Option<Progress>>>,
    intent: Arc<Mutex<Intent>>,
    /// True when the user pinned the connection count, so what the governor "settled on" says
    /// nothing about the host and must not be learned from.
    pinned_conns: bool,
}

struct Inner {
    sched: Scheduler,
    tasks: HashMap<String, TaskHandle>,
}

pub struct Manager {
    store: Arc<Store>,
    settings: RwLock<Settings>,
    inner: Mutex<Inner>,
    events: broadcast::Sender<EngineEvent>,
    /// Ids whose Details drawer is open. The connection table is the biggest payload the
    /// engine produces, so it is sent to nobody by default.
    conn_subs: Mutex<HashSet<String>>,
    stopping: CancellationToken,
}

impl Manager {
    /// Build a manager and start its event pump.
    ///
    /// Must be called from inside a Tokio runtime: the pump is a spawned task.
    pub fn new(store: Arc<Store>, settings: Settings) -> Arc<Self> {
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let me = Arc::new(Self {
            inner: Mutex::new(Inner {
                sched: Scheduler::new(Limits::from_settings(&settings)),
                tasks: HashMap::new(),
            }),
            settings: RwLock::new(settings),
            store,
            events,
            conn_subs: Mutex::new(HashSet::new()),
            stopping: CancellationToken::new(),
        });
        me.clone().spawn_pump();
        me
    }

    /// Open the store, recover from however the last run ended, and build a manager.
    pub fn open(db: &Path) -> Result<Arc<Self>> {
        let store = Arc::new(Store::open(db)?);
        // Any row still flagged clean is from the previous run; clearing them now means a
        // crash during *this* run is still detected as unclean.
        store.clear_clean_flags()?;
        let settings = store.load_settings()?.unwrap_or_default();
        let me = Self::new(store, settings);
        me.recover()?;
        Ok(me)
    }

    /// Reconcile the store with the fact that nothing is running yet.
    ///
    /// A row left `Active` means the app died mid-download. It becomes `Paused` rather than
    /// `Failed`: nothing went wrong with it, and the partial is intact and resumable.
    fn recover(&self) -> Result<()> {
        for rec in self.store.list(Some(DownloadStatus::Active))? {
            self.store
                .set_status(&rec.id, DownloadStatus::Paused, None)?;
        }
        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<EngineEvent> {
        self.events.subscribe()
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub fn settings(&self) -> Settings {
        self.settings.read().unwrap().clone()
    }

    pub fn ui_settings(&self) -> UiSettings {
        UiSettings::from(&*self.settings.read().unwrap())
    }

    /// Apply a settings form. Reads the current settings, folds the form onto them, and saves,
    /// so a field the form does not expose keeps its value rather than reverting to default.
    pub fn set_ui_settings(&self, ui: &UiSettings) -> Result<()> {
        let mut s = self.settings();
        ui.apply_to(&mut s);
        self.set_settings(s)
    }

    /// Persist new settings and apply the scheduling limits to future admissions.
    pub fn set_settings(&self, s: Settings) -> Result<()> {
        self.store.save_settings(&s)?;
        {
            let mut inner = self.inner.lock().unwrap();
            inner.sched.set_limits(Limits::from_settings(&s));
        }
        *self.settings.write().unwrap() = s;
        Ok(())
    }

    // -- reads ---------------------------------------------------------------------------

    pub fn list(&self, status: Option<DownloadStatus>) -> Result<Vec<DownloadSummary>> {
        let recs = self.store.list(status)?;
        let inner = self.inner.lock().unwrap();
        Ok(recs
            .iter()
            .map(|r| summarise(r, inner.sched.queue_position(&r.id)))
            .collect())
    }

    pub fn details(&self, id: &str) -> Result<DownloadDetails> {
        let rec = self.record(id)?;
        let (queue_pos, connections) = {
            let inner = self.inner.lock().unwrap();
            let conns = inner
                .tasks
                .get(id)
                .and_then(|t| t.live.lock().unwrap().clone())
                .map(|p| p.connections.iter().map(ConnectionSnapshot::from).collect())
                .unwrap_or_default();
            (inner.sched.queue_position(id), conns)
        };
        let history = self
            .store
            .url_history(id)?
            .into_iter()
            .map(|h| UrlHistoryRow {
                seq: h.seq,
                url_redacted: h.url_redacted,
                host: h.host,
                source: h.source,
                outcome: h.outcome,
                bytes_done_at_swap: h.bytes_done_at_swap,
                added_at: h.added_at,
            })
            .collect();

        Ok(DownloadDetails {
            original_url_redacted: redact::redact(&rec.original_url).as_str().to_string(),
            final_url_redacted: redact::redact(&rec.final_url).as_str().to_string(),
            etag: rec.etag.clone(),
            last_modified: rec.last_modified.clone(),
            content_type: rec.content_type.clone(),
            http_version: rec.http_version.clone(),
            accept_ranges: RangeSupport::from_code(rec.accept_ranges),
            max_connections: rec.max_connections,
            retry_count: rec.retry_count,
            url_refresh_count: rec.url_refresh_count,
            part_path: rec.part_path.clone(),
            completed_spans: rec.completed_ranges.spans().len(),
            connections,
            url_history: history,
            summary: summarise(&rec, queue_pos),
        })
    }

    /// The full, unredacted current URL.
    ///
    /// Deliberately its own command rather than a field on `details`: handing back a bearer
    /// credential should be something the user asked for once, and something an audit can see.
    pub fn reveal_full_url(&self, id: &str) -> Result<String> {
        let rec = self.record(id)?;
        self.store.push_url_history(
            id,
            &rec.current_url,
            UrlSource::User,
            "revealed",
            rec.bytes_done,
        )?;
        Ok(rec.current_url)
    }

    pub fn url_history(&self, id: &str) -> Result<Vec<UrlHistoryRow>> {
        Ok(self.details(id)?.url_history)
    }

    /// Look at a URL without committing to it. Mutates nothing.
    pub async fn probe_url(&self, url: &str) -> Result<ProbePreview> {
        let settings = self.settings();
        let pr = probe(url, &settings, &RequestSpec::default(), false).await?;
        let existing = self.find_existing_for(&pr.identity_hint())?;
        Ok(ProbePreview {
            url_redacted: redact::redact(url).as_str().to_string(),
            final_url_redacted: redact::redact(pr.final_url.as_str()).as_str().to_string(),
            filename: pr.filename.clone(),
            total_size: pr.total_size,
            resumable: pr.is_resumable(),
            segmentable: pr.is_segmentable(),
            content_type: pr.content_type.clone(),
            http_version: pr.http_version.clone(),
            redirects: pr.redirect_chain.len(),
            existing,
        })
    }

    /// Incomplete downloads that may be this same file (§D.10.7).
    pub fn find_existing_for(&self, identity_hint: &str) -> Result<Vec<DownloadSummary>> {
        let recs = self.store.find_resumable_by_hint(identity_hint)?;
        let inner = self.inner.lock().unwrap();
        Ok(recs
            .iter()
            .filter(|r| r.bytes_done > 0)
            .map(|r| summarise(r, inner.sched.queue_position(&r.id)))
            .collect())
    }

    // -- mutations -----------------------------------------------------------------------

    /// Add a download. Probes first, so the row carries a real filename and size before any
    /// byte moves and the list never shows an "unknown" row that later renames itself.
    pub async fn add(self: &Arc<Self>, req: AddRequest) -> Result<String> {
        let settings = self.settings();
        let pr = probe(&req.url, &settings, &RequestSpec::default(), false).await?;

        let dest_dir = req
            .dest_dir
            .map(PathBuf::from)
            .unwrap_or_else(|| settings.download_dir.clone());
        std::fs::create_dir_all(&dest_dir).map_err(|e| ManagerError::Io(e.to_string()))?;

        let filename = req.filename.clone().unwrap_or_else(|| pr.filename.clone());
        let part = crate::fsx::part_path(&dest_dir, &filename);

        let mut rec = DownloadRecord::new(
            &req.url,
            &filename,
            &dest_dir.to_string_lossy(),
            &part.to_string_lossy(),
        );
        rec.total_size = pr.total_size;
        rec.identity_hint = pr.identity_hint();
        rec.etag = pr.etag.as_ref().map(|e| e.as_header());
        rec.last_modified = pr.last_modified.clone();
        rec.content_type = pr.content_type.clone();
        rec.http_version = Some(pr.http_version.clone());
        rec.accept_ranges = pr.range_support.to_code();
        rec.max_connections = req.max_conns;
        rec.final_url = pr.final_url.to_string();
        if let Some(c) = &req.category {
            rec.category = c.clone();
        }
        self.store.insert(&rec)?;

        let id = rec.id.clone();
        self.enqueue(&id, &rec.current_url, req.priority, req.start_now);
        self.emit_state(&id, DownloadStatus::Queued, None);
        self.pump_queue();
        Ok(id)
    }

    /// Put a download back in the queue. The scheduler decides when it actually starts.
    pub fn resume(self: &Arc<Self>, id: &str) -> Result<()> {
        let rec = self.record(id)?;
        if rec.status == DownloadStatus::Completed {
            return Err(ManagerError::Invalid(format!(
                "{} is already complete",
                rec.filename
            )));
        }
        if self.is_running(id) {
            return Ok(());
        }
        self.enqueue(id, &rec.current_url, Priority::Normal, false);
        self.store.set_status(id, DownloadStatus::Queued, None)?;
        self.emit_state(id, DownloadStatus::Queued, None);
        self.pump_queue();
        Ok(())
    }

    /// Same as [`Manager::resume`], and clears the previous error so the row stops showing it.
    pub fn retry(self: &Arc<Self>, id: &str) -> Result<()> {
        self.store.set_status(id, DownloadStatus::Queued, None)?;
        self.resume(id)
    }

    /// Stop, keeping everything. The partial file stays exactly where it is.
    pub fn pause(&self, id: &str) -> Result<()> {
        if self.stop_running(id, Intent::Pause) {
            return Ok(());
        }
        // Not running: it was queued, or already stopped.
        self.dequeue(id);
        let rec = self.record(id)?;
        if rec.status.is_resumable() && rec.status != DownloadStatus::Paused {
            self.store.set_status(id, DownloadStatus::Paused, None)?;
            self.emit_state(id, DownloadStatus::Paused, None);
        }
        Ok(())
    }

    /// Stop and mark cancelled. The partial file is still not deleted — that is
    /// [`Manager::remove`]'s job, and only when asked.
    pub fn cancel(&self, id: &str) -> Result<()> {
        if self.stop_running(id, Intent::Cancel) {
            return Ok(());
        }
        self.dequeue(id);
        self.store.set_status(id, DownloadStatus::Cancelled, None)?;
        self.emit_state(id, DownloadStatus::Cancelled, None);
        Ok(())
    }

    /// Forget a download, optionally deleting what was downloaded.
    ///
    /// When it is running, deletion happens after the writer has stopped rather than under it:
    /// the intent is recorded, the token cancelled, and the cleanup runs on the way out.
    pub fn remove(&self, id: &str, delete_file: bool) -> Result<()> {
        if self.stop_running(id, Intent::Remove { delete_file }) {
            return Ok(());
        }
        self.dequeue(id);
        let rec = self.record(id)?;
        if delete_file {
            delete_artifacts(&rec);
        }
        self.store.delete(id)?;
        self.emit_state(id, DownloadStatus::Cancelled, None);
        Ok(())
    }

    pub fn set_priority(&self, id: &str, priority: Priority) -> bool {
        let mut inner = self.inner.lock().unwrap();
        inner.sched.set_priority(id, priority)
    }

    /// Move a queued download to the head of the queue.
    pub fn start_now(self: &Arc<Self>, id: &str) -> bool {
        let moved = {
            let mut inner = self.inner.lock().unwrap();
            inner.sched.start_now(id)
        };
        if moved {
            self.pump_queue();
        }
        moved
    }

    /// Pause everything and stop the pump. Called on window close.
    pub fn shutdown(&self) {
        self.stopping.cancel();
        let ids: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            inner.tasks.keys().cloned().collect()
        };
        for id in ids {
            let _ = self.pause(&id);
        }
    }

    // -- URL refresh (§D.10) -------------------------------------------------------------

    /// Check a replacement link against what is already on disk. Mutates nothing, so the UI
    /// can show the evidence before the user commits.
    pub async fn validate_replacement_url(&self, id: &str, url: &str) -> Result<RefreshReport> {
        let rec = self.record(id)?;
        let settings = self.settings();
        let old = ResourceSignals {
            total_size: rec.total_size,
            etag: rec.etag.as_deref().and_then(Validator::parse),
            last_modified: rec.last_modified.clone(),
            content_type: rec.content_type.clone(),
            filename: Some(rec.filename.clone()),
            range_support: RangeSupport::from_code(rec.accept_ranges),
        };
        let (ranges, _) = self.store.resume_state(id, settings.paranoid_recovery)?;

        let report = validate_replacement_url(
            url,
            &old,
            Path::new(&rec.part_path),
            &ranges,
            id,
            &settings,
            &RequestSpec::default(),
        )
        .await
        .map_err(|e| ManagerError::Refresh(e.to_string()))?;

        let (outcome, message) = match &report.verdict {
            Verdict::Resume(ev) => (RefreshOutcome::Verified, ev.user_summary()),
            Verdict::Confirm(ev) => (RefreshOutcome::Confirm, ev.user_summary()),
            Verdict::Reject(r) => (RefreshOutcome::Reject, r.user_message()),
            Verdict::RestartOnly => (
                RefreshOutcome::RestartOnly,
                "This link doesn't support resuming. Using it means downloading the whole file \
                 again."
                    .to_string(),
            ),
        };

        Ok(RefreshReport {
            outcome,
            message,
            windows_checked: report.windows_checked,
            bytes_verified: report.bytes_verified,
            preserved_bytes: ranges.total(),
            total_size: report.new_signals.total_size,
            resumable: report.new_signals.range_support == RangeSupport::Supported,
        })
    }

    /// Swap the URL and resume.
    ///
    /// Re-validates rather than trusting the report the UI is holding: between the two calls
    /// the link may have expired, and a swap is the one operation that must never proceed on
    /// stale evidence. A rejected replacement is refused here regardless of what the user
    /// clicked — there is no "resume anyway" path by design.
    pub async fn commit_replacement_url(
        self: &Arc<Self>,
        id: &str,
        url: &str,
        source: UrlSource,
        accept_confirm: bool,
    ) -> Result<()> {
        let rec = self.record(id)?;
        let settings = self.settings();
        let old = ResourceSignals {
            total_size: rec.total_size,
            etag: rec.etag.as_deref().and_then(Validator::parse),
            last_modified: rec.last_modified.clone(),
            content_type: rec.content_type.clone(),
            filename: Some(rec.filename.clone()),
            range_support: RangeSupport::from_code(rec.accept_ranges),
        };
        let (ranges, _) = self.store.resume_state(id, settings.paranoid_recovery)?;
        let report = validate_replacement_url(
            url,
            &old,
            Path::new(&rec.part_path),
            &ranges,
            id,
            &settings,
            &RequestSpec::default(),
        )
        .await
        .map_err(|e| ManagerError::Refresh(e.to_string()))?;

        let validation = match &report.verdict {
            Verdict::Resume(ev) if ev.etag_matches => ValidationState::AutoVerified,
            Verdict::Resume(_) => ValidationState::ContentVerified,
            Verdict::Confirm(_) if accept_confirm => ValidationState::UserConfirmed,
            Verdict::Confirm(_) => {
                return Err(ManagerError::Invalid(
                    "this link needs confirming before it can be used".into(),
                ))
            }
            Verdict::Reject(r) => return Err(ManagerError::Refresh(r.user_message())),
            Verdict::RestartOnly => {
                return Err(ManagerError::Refresh(
                    "this link cannot resume; the partial file has been left in place".into(),
                ))
            }
        };

        self.store.swap_url(
            id,
            url,
            &report.resolved_url,
            report
                .new_signals
                .etag
                .as_ref()
                .map(|e| e.as_header())
                .as_deref(),
            report.new_signals.last_modified.as_deref(),
            report.new_signals.total_size,
            report.new_signals.range_support.to_code(),
            validation,
            source.as_str(),
        )?;
        self.resume(id)
    }

    // -- connection subscriptions --------------------------------------------------------

    pub fn subscribe_connections(&self, id: &str) {
        self.conn_subs.lock().unwrap().insert(id.to_string());
    }

    pub fn unsubscribe_connections(&self, id: &str) {
        self.conn_subs.lock().unwrap().remove(id);
    }

    // -- internals -----------------------------------------------------------------------

    fn record(&self, id: &str) -> Result<DownloadRecord> {
        self.store
            .get(id)?
            .ok_or_else(|| ManagerError::NotFound(id.to_string()))
    }

    fn is_running(&self, id: &str) -> bool {
        self.inner.lock().unwrap().tasks.contains_key(id)
    }

    fn enqueue(&self, id: &str, url: &str, priority: Priority, front: bool) {
        let host = redact::host_of(url);
        let mut inner = self.inner.lock().unwrap();
        inner.sched.enqueue(id, host, priority);
        if front {
            inner.sched.start_now(id);
        }
    }

    fn dequeue(&self, id: &str) {
        self.inner.lock().unwrap().sched.remove(id);
    }

    /// Record why a running download is stopping and cancel it. Returns false when it was not
    /// running, so the caller can handle the queued or already-stopped case.
    fn stop_running(&self, id: &str, intent: Intent) -> bool {
        let inner = self.inner.lock().unwrap();
        match inner.tasks.get(id) {
            Some(t) => {
                *t.intent.lock().unwrap() = intent;
                t.cancel.cancel();
                true
            }
            None => false,
        }
    }

    fn emit(&self, ev: EngineEvent) {
        // An error here means nobody is listening, which is normal for a headless run.
        let _ = self.events.send(ev);
    }

    fn emit_state(&self, id: &str, status: DownloadStatus, error: Option<String>) {
        self.emit(EngineEvent::State(StateChanged {
            id: id.to_string(),
            status,
            error,
        }));
    }

    /// Start everything the scheduler will admit.
    fn pump_queue(self: &Arc<Self>) {
        let admitted = {
            let mut inner = self.inner.lock().unwrap();
            inner.sched.poll()
        };
        for a in admitted {
            if let Err(e) = self.clone().start(&a.id, a.max_conns) {
                // Never leave a download admitted but not running: the slot would be held by
                // something that does not exist and the queue would wedge.
                {
                    let mut inner = self.inner.lock().unwrap();
                    inner.sched.finished(&a.id);
                }
                let _ = self
                    .store
                    .set_status(&a.id, DownloadStatus::Failed, Some(&e.to_string()));
                self.emit_state(&a.id, DownloadStatus::Failed, Some(e.to_string()));
            }
        }
    }

    /// Spawn one download.
    fn start(self: Arc<Self>, id: &str, budget_conns: usize) -> Result<()> {
        let rec = self.record(id)?;
        let settings = self.settings();
        let (completed, _unclean) = self.store.resume_state(id, settings.paranoid_recovery)?;

        let host = redact::host_of(&rec.current_url);
        let host_hint = self.store.host_profile(&host)?.map(|p| HostHint {
            best_conns: p.best_observed_conns,
            saturated: p.saturation_detected,
        });

        // A user-pinned count is a ceiling the user chose; the scheduler's budget is a ceiling
        // the app must respect. Take the lower of the two.
        let pinned_conns = rec.max_connections.is_some();
        let max_conns = Some(match rec.max_connections {
            Some(k) => k.min(budget_conns),
            None => budget_conns,
        });

        let probe_token = {
            let inner = self.inner.lock().unwrap();
            inner.sched.probe_token()
        };

        let req = DownloadRequest {
            url: rec.current_url.clone(),
            dest_dir: PathBuf::from(&rec.dest_dir),
            filename: Some(rec.filename.clone()),
            max_conns,
            spec: RequestSpec::default(),
            completed,
            expected_sha256: None,
            host_hint,
            probe_token: Some(probe_token),
        };

        let cancel = CancellationToken::new();
        let live = Arc::new(Mutex::new(None));
        let intent = Arc::new(Mutex::new(Intent::Run));
        {
            let mut inner = self.inner.lock().unwrap();
            inner.sched.mark_running(id, &host);
            inner.tasks.insert(
                id.to_string(),
                TaskHandle {
                    cancel: cancel.clone(),
                    host: host.clone(),
                    live: live.clone(),
                    intent: intent.clone(),
                    pinned_conns,
                },
            );
        }

        self.store.set_status(id, DownloadStatus::Active, None)?;
        self.emit_state(id, DownloadStatus::Active, None);

        let sink = Arc::new(StoreSink {
            store: self.store.clone(),
            id: id.to_string(),
        });
        let on_progress: Option<Box<dyn Fn(Progress) + Send + Sync>> = {
            let live = live.clone();
            Some(Box::new(move |p| {
                *live.lock().unwrap() = Some(p);
            }))
        };

        let id_owned = id.to_string();
        tokio::spawn(async move {
            let outcome = download(req, settings, sink, cancel, on_progress).await;
            let intent = *intent.lock().unwrap();
            self.finish(&id_owned, outcome, intent).await;
        });
        Ok(())
    }

    /// Retire a finished download and let the queue move.
    async fn finish(
        self: Arc<Self>,
        id: &str,
        outcome: std::result::Result<crate::task::DownloadOutcome, DownloadError>,
        intent: Intent,
    ) {
        let handle = {
            let mut inner = self.inner.lock().unwrap();
            inner.sched.finished(id);
            inner.tasks.remove(id)
        };

        match outcome {
            Ok(o) => {
                let _ = self.store.set_status(id, DownloadStatus::Completed, None);
                let _ = self.store.mark_clean(id, true);
                // Learn what worked on this host — but only when the governor was actually in
                // charge. A pinned connection count says nothing about what the server allows.
                if let Some(h) = &handle {
                    if !h.pinned_conns {
                        let _ = self.store.record_host_profile(
                            &h.host,
                            o.settled_conns,
                            o.avg_bps,
                            o.saturation_detected,
                        );
                    }
                }
                let filename = self
                    .store
                    .get(id)
                    .ok()
                    .flatten()
                    .map(|r| r.filename)
                    .unwrap_or_default();
                self.emit_state(id, DownloadStatus::Completed, None);
                self.emit(EngineEvent::Notice(Notice::Completed {
                    id: id.to_string(),
                    filename,
                    path: o.path.to_string_lossy().to_string(),
                    sha256: o.sha256,
                }));
            }
            Err(e) => self.finish_error(id, e, intent),
        }

        self.pump_queue();
    }

    fn finish_error(self: &Arc<Self>, id: &str, e: DownloadError, intent: Intent) {
        // A cancelled download stops for a reason the user chose, so the intent decides what
        // it becomes. Everything else is decided by the error.
        let status = match (&e, intent) {
            (DownloadError::Cancelled, Intent::Pause) => DownloadStatus::Paused,
            (DownloadError::Cancelled, Intent::Cancel) => DownloadStatus::Cancelled,
            (DownloadError::Cancelled, Intent::Remove { .. }) => DownloadStatus::Cancelled,
            // Cancelled without anyone asking means shutdown; it is resumable.
            (DownloadError::Cancelled, Intent::Run) => DownloadStatus::Paused,
            (DownloadError::Failed(ErrorClass::UrlExpired), _) => DownloadStatus::NeedsAttention,
            (DownloadError::Failed(ErrorClass::NetworkDown), _) => DownloadStatus::WaitingNetwork,
            _ => DownloadStatus::Failed,
        };

        if let Intent::Remove { delete_file } = intent {
            if let Ok(Some(rec)) = self.store.get(id) {
                if delete_file {
                    delete_artifacts(&rec);
                }
            }
            let _ = self.store.delete(id);
            self.emit_state(id, DownloadStatus::Cancelled, None);
            return;
        }

        let msg = e.to_string();
        let _ = self.store.set_status(id, status, Some(&msg));
        // A clean stop even on failure: the partial stays exactly as recorded.
        let _ = self.store.mark_clean(id, true);

        let rec = self.store.get(id).ok().flatten();
        let filename = rec.as_ref().map(|r| r.filename.clone()).unwrap_or_default();
        self.emit_state(id, status, Some(msg.clone()));

        match (&e, status) {
            (_, DownloadStatus::NeedsAttention) => {
                self.emit(EngineEvent::Notice(Notice::UrlExpired {
                    id: id.to_string(),
                    filename,
                    bytes_done: rec.as_ref().map(|r| r.bytes_done).unwrap_or(0),
                    total: rec.as_ref().and_then(|r| r.total_size),
                }));
            }
            (DownloadError::Io(io), _) if is_disk_full(io) => {
                self.emit(EngineEvent::Notice(Notice::DiskFull {
                    id: id.to_string(),
                    filename,
                    needed_bytes: rec.as_ref().and_then(|r| r.remaining()).unwrap_or(0),
                }));
            }
            (_, DownloadStatus::Failed) => {
                self.emit(EngineEvent::Notice(Notice::Failed {
                    id: id.to_string(),
                    filename,
                    message: msg,
                }));
            }
            _ => {}
        }
    }

    /// The event pump: one batched progress event per tick, connection tables at half that,
    /// and nothing at all when nothing is running.
    fn spawn_pump(self: Arc<Self>) {
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(TICK);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut n: u64 = 0;
            loop {
                tokio::select! {
                    _ = self.stopping.cancelled() => break,
                    _ = ticker.tick() => {}
                }
                n = n.wrapping_add(1);

                let (snapshots, conns) = self.collect(n % CONNECTIONS_EVERY == 0);

                // Silence when idle. An app that emits into an empty room four times a second
                // is why Tauri apps get accused of burning CPU doing nothing.
                if !snapshots.is_empty() {
                    self.emit(EngineEvent::Progress {
                        downloads: snapshots,
                    });
                }
                for c in conns {
                    self.emit(EngineEvent::Connections(c));
                }
            }
        });
    }

    /// Read every running download's latest reading, and feed the scheduler the connection
    /// counts it needs to keep the global budget honest.
    fn collect(&self, want_conns: bool) -> (Vec<ProgressSnapshot>, Vec<ConnectionsSnapshot>) {
        let subs = if want_conns {
            self.conn_subs.lock().unwrap().clone()
        } else {
            HashSet::new()
        };

        let mut snapshots = Vec::new();
        let mut conns = Vec::new();
        let mut counts: Vec<(String, usize)> = Vec::new();

        {
            let inner = self.inner.lock().unwrap();
            for (id, t) in inner.tasks.iter() {
                let Some(p) = t.live.lock().unwrap().clone() else {
                    continue;
                };
                counts.push((id.clone(), p.conns));
                snapshots.push(ProgressSnapshot::from_progress(
                    id,
                    DownloadStatus::Active,
                    &p,
                ));
                if subs.contains(id) {
                    conns.push(ConnectionsSnapshot {
                        id: id.clone(),
                        connections: p.connections.iter().map(ConnectionSnapshot::from).collect(),
                    });
                }
            }
        }

        if !counts.is_empty() {
            let mut inner = self.inner.lock().unwrap();
            for (id, k) in counts {
                inner.sched.update_conns(&id, k);
            }
        }

        (snapshots, conns)
    }
}

fn summarise(r: &DownloadRecord, queue_position: Option<usize>) -> DownloadSummary {
    DownloadSummary {
        id: r.id.clone(),
        filename: r.filename.clone(),
        url_redacted: redact::redact(&r.current_url).as_str().to_string(),
        host: redact::host_of(&r.current_url),
        dest_dir: r.dest_dir.clone(),
        category: r.category.clone(),
        total_size: r.total_size,
        bytes_done: r.bytes_done,
        status: r.status,
        validation_state: r.validation_state,
        queue_position: queue_position.filter(|_| r.status == DownloadStatus::Queued),
        created_at: r.created_at,
        completed_at: r.completed_at,
        error: r.error_message.clone(),
        resumable: RangeSupport::from_code(r.accept_ranges) == RangeSupport::Supported,
    }
}

/// Whether an I/O error is "the disk is full" rather than some other local failure.
///
/// `ensure_space` raises `StorageFull` before a download starts; a disk that fills *during* a
/// write surfaces as the platform's own code instead, so both are checked. Getting this wrong
/// in either direction only changes which notice the user sees, never what is on disk.
fn is_disk_full(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::StorageFull {
        return true;
    }
    // ENOSPC on Unix, ERROR_DISK_FULL on Windows.
    matches!(e.raw_os_error(), Some(28) if cfg!(unix))
        || matches!(e.raw_os_error(), Some(112) if cfg!(windows))
}

/// Delete what a download produced. Best-effort: a file already gone is the desired state, and
/// a locked one must not stop the record from being removed.
fn delete_artifacts(rec: &DownloadRecord) {
    let _ = std::fs::remove_file(&rec.part_path);
    let _ = std::fs::remove_file(Path::new(&rec.dest_dir).join(&rec.filename));
}
