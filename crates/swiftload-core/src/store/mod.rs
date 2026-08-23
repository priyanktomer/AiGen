//! Persistence.
//!
//! SQLite in WAL mode, one file. The two decisions worth explaining:
//!
//! **Segment state is one BLOB in one row, not a row per segment.** Under a shrink-and-steal
//! scheduler segments are not stable entities — they split, merge and get reassigned
//! constantly, so rows would thrash. What actually has to survive a crash is a single fact:
//! which byte ranges are durably on disk. That is a `RangeSet`, which varint-encodes to a few
//! dozen bytes even when heavily fragmented, so a checkpoint is one single-row UPDATE inside
//! one transaction — atomic by construction, with no multi-row consistency problem for
//! recovery to reason about.
//!
//! **URLs are attributes of a download, not its identity.** A signed link expires and gets
//! replaced; the download is the same download. So identity is the row id, and the URL columns
//! are just current state. History is kept redacted, because a signed URL is a bearer
//! credential.

pub mod models;

use crate::util::{intervals::RangeSet, redact};
use models::*;
use rusqlite::{params, Connection, OptionalExtension};
use std::{path::Path, sync::Mutex};

const SCHEMA_VERSION: i64 = 2;
/// Timeline entries kept per download.
const EVENT_RING: i64 = 200;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("corrupt range set: {0}")]
    Ranges(#[from] crate::util::intervals::DecodeError),
    #[error("no such download: {0}")]
    NotFound(String),
    #[error("settings could not be serialised: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("database schema version {0} is newer than this build understands")]
    Schema(i64),
}

type Result<T> = std::result::Result<T, StoreError>;

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        // WAL with synchronous=NORMAL: losing the last few seconds of progress on an OS crash
        // is fine because resume re-verifies anyway, and it never corrupts the database.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // A fresh database is created at the current shape; an existing one is stepped forward
        // one version at a time. `SCHEMA` alone is not enough to migrate: it is written with
        // `CREATE TABLE IF NOT EXISTS`, so it silently does nothing to a table that already
        // exists but is missing a column.
        let mut version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version == 0 {
            conn.execute_batch(SCHEMA)?;
            version = SCHEMA_VERSION;
        }
        // A database written by a newer build. Refuse rather than guess: its columns and
        // invariants are unknown, and opening it read-write is how a user who downgrades once
        // loses their download history.
        if version > SCHEMA_VERSION {
            return Err(StoreError::Schema(version));
        }
        while version < SCHEMA_VERSION {
            match version {
                1 => conn.execute_batch(MIGRATE_1_TO_2)?,
                v => return Err(StoreError::Schema(v)),
            }
            version += 1;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn insert(&self, d: &DownloadRecord) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO downloads (
                id, original_url, current_url, final_url, url_refresh_count, identity_hint,
                filename, dest_dir, part_path, category,
                total_size, bytes_done, completed_ranges, status,
                accept_ranges, etag, last_modified, content_type, http_version,
                validation_state, verified_windows,
                max_connections, retry_count, created_at, clean_shutdown, priority
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,0,?25)",
            params![
                d.id, d.original_url, d.current_url, d.final_url, d.url_refresh_count,
                d.identity_hint, d.filename, d.dest_dir, d.part_path, d.category,
                d.total_size, d.bytes_done, d.completed_ranges.encode(), d.status.as_str(),
                d.accept_ranges, d.etag, d.last_modified, d.content_type, d.http_version,
                d.validation_state.as_str(), Vec::<u8>::new(),
                d.max_connections, d.retry_count, d.created_at, d.priority.as_str(),
            ],
        )?;
        drop(c);
        self.push_url_history(&d.id, &d.original_url, UrlSource::User, "initial", 0)?;
        Ok(())
    }

    /// The hot path: called by the writer after data is durable. One row, one transaction.
    pub fn checkpoint(&self, id: &str, ranges: &RangeSet, bytes_done: u64) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "UPDATE downloads SET completed_ranges = ?2, bytes_done = ?3 WHERE id = ?1",
            params![id, ranges.encode(), bytes_done],
        )?;
        if n == 0 {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(())
    }

    pub fn get(&self, id: &str) -> Result<Option<DownloadRecord>> {
        let c = self.conn.lock().unwrap();
        c.query_row(
            &format!("SELECT {COLS} FROM downloads WHERE id = ?1"),
            params![id],
            row_to_record,
        )
        .optional()?
        .transpose()
    }

    pub fn list(&self, filter: Option<DownloadStatus>) -> Result<Vec<DownloadRecord>> {
        let c = self.conn.lock().unwrap();
        let (sql, args): (String, Vec<String>) = match filter {
            Some(s) => (
                format!("SELECT {COLS} FROM downloads WHERE status = ?1 ORDER BY created_at DESC"),
                vec![s.as_str().to_string()],
            ),
            None => (
                format!("SELECT {COLS} FROM downloads ORDER BY created_at DESC"),
                vec![],
            ),
        };
        let mut stmt = c.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args), row_to_record)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    pub fn set_status(&self, id: &str, status: DownloadStatus, error: Option<&str>) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE downloads SET status = ?2, error_message = ?3,
                completed_at = CASE WHEN ?2 = 'completed' THEN ?4 ELSE completed_at END
             WHERE id = ?1",
            params![id, status.as_str(), error, now()],
        )?;
        Ok(())
    }

    /// Mark a download as cleanly stopped. Its absence on the next start is what tells us we
    /// crashed, which in turn triggers the paranoid rewind.
    pub fn mark_clean(&self, id: &str, clean: bool) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE downloads SET clean_shutdown = ?2 WHERE id = ?1",
            params![id, clean as i64],
        )?;
        Ok(())
    }

    /// Clear every clean-shutdown flag at startup, so a crash *right now* is still detected.
    pub fn clear_clean_flags(&self) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("UPDATE downloads SET clean_shutdown = 0", [])?;
        Ok(())
    }

    /// Replace the URL a download fetches from, keeping every byte of its state.
    ///
    /// Deliberately does not touch `completed_ranges`, `bytes_done`, `part_path`, `filename` or
    /// `created_at`: preserving those is the entire point of the feature.
    #[allow(clippy::too_many_arguments)]
    pub fn swap_url(
        &self,
        id: &str,
        new_url: &str,
        resolved: &str,
        etag: Option<&str>,
        last_modified: Option<&str>,
        total_size: Option<u64>,
        accept_ranges: i64,
        validation: ValidationState,
        outcome: &str,
    ) -> Result<()> {
        let bytes_done: i64 = {
            let c = self.conn.lock().unwrap();
            c.query_row(
                "SELECT bytes_done FROM downloads WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )?
        };
        {
            let c = self.conn.lock().unwrap();
            let n = c.execute(
                "UPDATE downloads SET
                    current_url = ?2, final_url = ?3, url_refresh_count = url_refresh_count + 1,
                    etag = ?4, last_modified = ?5, total_size = COALESCE(?6, total_size),
                    accept_ranges = ?7, validation_state = ?8, status = 'queued', error_message = NULL
                 WHERE id = ?1",
                params![id, new_url, resolved, etag, last_modified, total_size, accept_ranges, validation.as_str()],
            )?;
            if n == 0 {
                return Err(StoreError::NotFound(id.to_string()));
            }
        }
        self.push_url_history(id, new_url, UrlSource::User, outcome, bytes_done as u64)
    }

    /// Append to the redacted URL chain. Never stores query-parameter values.
    pub fn push_url_history(
        &self,
        id: &str,
        url: &str,
        source: UrlSource,
        outcome: &str,
        bytes_at_swap: u64,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let seq: i64 = c
            .query_row(
                "SELECT COALESCE(MAX(seq) + 1, 0) FROM url_history WHERE download_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        c.execute(
            "INSERT INTO url_history (download_id, seq, url_redacted, host, source, outcome, bytes_done_at_swap, added_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            params![
                id,
                seq,
                redact::redact(url).as_str(),
                redact::host_of(url),
                source.as_str(),
                outcome,
                bytes_at_swap as i64,
                now()
            ],
        )?;
        Ok(())
    }

    pub fn url_history(&self, id: &str) -> Result<Vec<UrlHistoryEntry>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(
            "SELECT seq, url_redacted, host, source, outcome, bytes_done_at_swap, added_at
             FROM url_history WHERE download_id = ?1 ORDER BY seq",
        )?;
        let rows = stmt.query_map(params![id], |r| {
            Ok(UrlHistoryEntry {
                seq: r.get(0)?,
                url_redacted: r.get(1)?,
                host: r.get(2)?,
                source: r.get(3)?,
                outcome: r.get(4)?,
                bytes_done_at_swap: r.get::<_, i64>(5)? as u64,
                added_at: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Candidate duplicates for a newly added URL.
    ///
    /// The hint is filename plus size, which is weak by construction — it exists so the UI can
    /// ask "you may already have this", after which the real identity check runs. It is never
    /// treated as proof.
    pub fn find_resumable_by_hint(&self, hint: &str) -> Result<Vec<DownloadRecord>> {
        let c = self.conn.lock().unwrap();
        let mut stmt = c.prepare(&format!(
            "SELECT {COLS} FROM downloads
             WHERE identity_hint = ?1
               AND status IN ('paused','failed','needs_attention','waiting_network','queued')
             ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map(params![hint], row_to_record)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .collect()
    }

    /// Load resume state, applying the paranoid rewind when the last shutdown was unclean.
    ///
    /// fsync is honoured by essentially every modern drive, but consumer SSDs with volatile
    /// write caches and some virtualised storage have been observed to lie. Re-fetching at most
    /// a megabyte per fragment is trivial insurance against a corruption class we could not
    /// otherwise detect.
    pub fn resume_state(&self, id: &str, paranoid: bool) -> Result<(RangeSet, bool)> {
        let rec = self
            .get(id)?
            .ok_or_else(|| StoreError::NotFound(id.to_string()))?;
        let mut ranges = rec.completed_ranges;
        let unclean = !rec.clean_shutdown;
        if unclean && paranoid {
            ranges.rewind_tails(crate::config::PARANOID_REWIND);
        }
        Ok((ranges, unclean))
    }

    pub fn record_host_profile(
        &self,
        host: &str,
        best_conns: usize,
        best_bps: u64,
        saturated: bool,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO host_profiles (host, best_observed_conns, best_observed_bps, saturation_detected, samples, updated_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5)
             ON CONFLICT(host) DO UPDATE SET
                best_observed_conns = ?2, best_observed_bps = ?3,
                saturation_detected = ?4, samples = samples + 1, updated_at = ?5",
            params![host, best_conns as i64, best_bps as i64, saturated as i64, now()],
        )?;
        Ok(())
    }

    pub fn host_profile(&self, host: &str) -> Result<Option<HostProfile>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT host, best_observed_conns, best_observed_bps, saturation_detected, samples
             FROM host_profiles WHERE host = ?1",
            params![host],
            |r| {
                Ok(HostProfile {
                    host: r.get(0)?,
                    best_observed_conns: r.get::<_, Option<i64>>(1)?.map(|v| v as usize),
                    best_observed_bps: r.get::<_, Option<i64>>(2)?.map(|v| v as u64),
                    saturation_detected: r.get::<_, i64>(3)? != 0,
                    samples: r.get::<_, i64>(4)? as u64,
                })
            },
        )
        .optional()?)
    }

    /// Load persisted settings, or `None` on a fresh database.
    ///
    /// A row that fails to parse is treated as absent rather than as an error: a settings file
    /// written by a newer version must not stop the app from opening. The caller falls back to
    /// defaults and the next save rewrites it.
    pub fn load_settings(&self) -> Result<Option<crate::config::Settings>> {
        let c = self.conn.lock().unwrap();
        let json: Option<String> = c
            .query_row("SELECT json FROM settings WHERE id = 1", [], |r| r.get(0))
            .optional()?;
        Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
    }

    pub fn save_settings(&self, s: &crate::config::Settings) -> Result<()> {
        let json = serde_json::to_string(s)?;
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO settings (id, json) VALUES (1, ?1)
             ON CONFLICT(id) DO UPDATE SET json = excluded.json",
            params![json],
        )?;
        Ok(())
    }

    /// Persist queue priority, so a reordered queue survives a restart.
    pub fn set_priority(&self, id: &str, priority: Priority) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE downloads SET priority = ?2 WHERE id = ?1",
            params![id, priority.as_str()],
        )?;
        Ok(())
    }

    /// Append to a download's timeline, trimming it to the most recent [`EVENT_RING`] entries.
    ///
    /// A ring rather than a log: this is kept for the person looking at the Timeline tab, and
    /// an unbounded history of a download that has been retried two hundred times is neither
    /// readable nor worth the rows.
    pub fn record_event(&self, id: &str, kind: &str, detail: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO events (download_id, at, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            params![id, now(), kind, detail],
        )?;
        c.execute(
            "DELETE FROM events WHERE download_id = ?1 AND seq NOT IN (
                 SELECT seq FROM events WHERE download_id = ?1 ORDER BY seq DESC LIMIT ?2
             )",
            params![id, EVENT_RING],
        )?;
        Ok(())
    }

    /// A download's timeline, oldest first — the order it is read in.
    pub fn events(&self, id: &str) -> Result<Vec<EventRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(
            "SELECT seq, at, kind, detail FROM events WHERE download_id = ?1 ORDER BY seq ASC",
        )?;
        let rows = st.query_map(params![id], |r| {
            Ok(EventRow {
                seq: r.get(0)?,
                at: r.get(1)?,
                kind: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM downloads WHERE id = ?1", params![id])?;
        Ok(())
    }
}

/// Adapts the store to the writer's checkpoint interface.
pub struct StoreSink {
    pub store: std::sync::Arc<Store>,
    pub id: String,
}

impl crate::task::writer::CheckpointSink for StoreSink {
    fn checkpoint(&self, ranges: &RangeSet, bytes_done: u64) -> std::io::Result<()> {
        self.store
            .checkpoint(&self.id, ranges, bytes_done)
            .map_err(|e| std::io::Error::other(e.to_string()))
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

const COLS: &str = "id, original_url, current_url, final_url, url_refresh_count, identity_hint, \
                    filename, dest_dir, part_path, category, total_size, bytes_done, \
                    completed_ranges, status, accept_ranges, etag, last_modified, content_type, \
                    http_version, validation_state, max_connections, retry_count, created_at, \
                    completed_at, error_message, clean_shutdown, priority";

fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<Result<DownloadRecord>> {
    let blob: Vec<u8> = r.get(12)?;
    let ranges = match RangeSet::decode(&blob) {
        Ok(v) => v,
        Err(e) => return Ok(Err(StoreError::Ranges(e))),
    };
    Ok(Ok(DownloadRecord {
        id: r.get(0)?,
        original_url: r.get(1)?,
        current_url: r.get(2)?,
        final_url: r.get(3)?,
        url_refresh_count: r.get(4)?,
        identity_hint: r.get(5)?,
        filename: r.get(6)?,
        dest_dir: r.get(7)?,
        part_path: r.get(8)?,
        category: r.get(9)?,
        total_size: r.get::<_, Option<i64>>(10)?.map(|v| v as u64),
        bytes_done: r.get::<_, i64>(11)? as u64,
        completed_ranges: ranges,
        status: DownloadStatus::from_db(&r.get::<_, String>(13)?),
        accept_ranges: r.get(14)?,
        etag: r.get(15)?,
        last_modified: r.get(16)?,
        content_type: r.get(17)?,
        http_version: r.get(18)?,
        validation_state: ValidationState::from_db(&r.get::<_, String>(19)?),
        max_connections: r.get::<_, Option<i64>>(20)?.map(|v| v as usize),
        retry_count: r.get::<_, i64>(21)? as u64,
        created_at: r.get(22)?,
        completed_at: r.get(23)?,
        error_message: r.get(24)?,
        clean_shutdown: r.get::<_, i64>(25)? != 0,
        priority: Priority::from_db(&r.get::<_, String>(26)?),
    }))
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS downloads (
    id                TEXT PRIMARY KEY,
    original_url      TEXT NOT NULL,
    current_url       TEXT NOT NULL,
    final_url         TEXT NOT NULL,
    url_refresh_count INTEGER NOT NULL DEFAULT 0,
    identity_hint     TEXT NOT NULL DEFAULT '',
    filename          TEXT NOT NULL,
    dest_dir          TEXT NOT NULL,
    part_path         TEXT NOT NULL,
    category          TEXT NOT NULL DEFAULT 'other',
    total_size        INTEGER,
    bytes_done        INTEGER NOT NULL DEFAULT 0,
    completed_ranges  BLOB NOT NULL DEFAULT x'',
    status            TEXT NOT NULL,
    accept_ranges     INTEGER NOT NULL DEFAULT 0,
    etag              TEXT,
    last_modified     TEXT,
    content_type      TEXT,
    http_version      TEXT,
    validation_state  TEXT NOT NULL DEFAULT 'not_required',
    verified_windows  BLOB NOT NULL DEFAULT x'',
    max_connections   INTEGER,
    retry_count       INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    completed_at      INTEGER,
    error_message     TEXT,
    clean_shutdown    INTEGER NOT NULL DEFAULT 0,
    priority          TEXT NOT NULL DEFAULT 'normal'
);
CREATE INDEX IF NOT EXISTS idx_dl_status   ON downloads(status);
CREATE INDEX IF NOT EXISTS idx_dl_created  ON downloads(created_at DESC);
CREATE INDEX IF NOT EXISTS idx_dl_identity ON downloads(identity_hint);

CREATE TABLE IF NOT EXISTS url_history (
    download_id        TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    seq                INTEGER NOT NULL,
    url_redacted       TEXT NOT NULL,
    host               TEXT NOT NULL,
    source             TEXT NOT NULL,
    outcome            TEXT NOT NULL,
    bytes_done_at_swap INTEGER NOT NULL DEFAULT 0,
    added_at           INTEGER NOT NULL,
    PRIMARY KEY (download_id, seq)
);

CREATE TABLE IF NOT EXISTS host_profiles (
    host                TEXT PRIMARY KEY,
    best_observed_conns INTEGER,
    best_observed_bps   INTEGER,
    saturation_detected INTEGER NOT NULL DEFAULT 0,
    samples             INTEGER NOT NULL DEFAULT 0,
    updated_at          INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS settings (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    download_id TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    at          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    detail      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_events_dl ON events(download_id, seq DESC);
"#;

/// Schema 1 -> 2: queue priority survives a restart, and downloads keep a timeline.
const MIGRATE_1_TO_2: &str = r#"
ALTER TABLE downloads ADD COLUMN priority TEXT NOT NULL DEFAULT 'normal';

CREATE TABLE IF NOT EXISTS events (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    download_id TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    at          INTEGER NOT NULL,
    kind        TEXT NOT NULL,
    detail      TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_events_dl ON events(download_id, seq DESC);
"#;

#[cfg(test)]
mod migration_tests {
    use super::*;

    /// The schema exactly as version 1 shipped.
    ///
    /// Pinned here as a literal rather than referenced from `SCHEMA`, because the whole point
    /// of a migration test is to start from the shape that is actually out in the world. A
    /// test that migrated from the *current* schema would pass forever while testing nothing.
    const SCHEMA_V1: &str = r#"
CREATE TABLE downloads (
    id                TEXT PRIMARY KEY,
    original_url      TEXT NOT NULL,
    current_url       TEXT NOT NULL,
    final_url         TEXT NOT NULL,
    url_refresh_count INTEGER NOT NULL DEFAULT 0,
    identity_hint     TEXT NOT NULL DEFAULT '',
    filename          TEXT NOT NULL,
    dest_dir          TEXT NOT NULL,
    part_path         TEXT NOT NULL,
    category          TEXT NOT NULL DEFAULT 'other',
    total_size        INTEGER,
    bytes_done        INTEGER NOT NULL DEFAULT 0,
    completed_ranges  BLOB NOT NULL DEFAULT x'',
    status            TEXT NOT NULL,
    accept_ranges     INTEGER NOT NULL DEFAULT 0,
    etag              TEXT,
    last_modified     TEXT,
    content_type      TEXT,
    http_version      TEXT,
    validation_state  TEXT NOT NULL DEFAULT 'not_required',
    verified_windows  BLOB NOT NULL DEFAULT x'',
    max_connections   INTEGER,
    retry_count       INTEGER NOT NULL DEFAULT 0,
    created_at        INTEGER NOT NULL,
    completed_at      INTEGER,
    error_message     TEXT,
    clean_shutdown    INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE url_history (
    download_id        TEXT NOT NULL REFERENCES downloads(id) ON DELETE CASCADE,
    seq                INTEGER NOT NULL,
    url_redacted       TEXT NOT NULL,
    host               TEXT NOT NULL,
    source             TEXT NOT NULL,
    outcome            TEXT NOT NULL,
    bytes_done_at_swap INTEGER NOT NULL DEFAULT 0,
    added_at           INTEGER NOT NULL,
    PRIMARY KEY (download_id, seq)
);
CREATE TABLE host_profiles (
    host                TEXT PRIMARY KEY,
    best_observed_conns INTEGER,
    best_observed_bps   INTEGER,
    saturation_detected INTEGER NOT NULL DEFAULT 0,
    samples             INTEGER NOT NULL DEFAULT 0,
    updated_at          INTEGER NOT NULL
);
CREATE TABLE settings (
    id   INTEGER PRIMARY KEY CHECK (id = 1),
    json TEXT NOT NULL
);
"#;

    /// Build a version-1 database holding one part-finished download.
    fn v1_database(path: &Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(SCHEMA_V1).unwrap();
        let ranges = RangeSet::from_pairs([(0, 4096)]);
        c.execute(
            "INSERT INTO downloads (id, original_url, current_url, final_url, identity_hint,
                filename, dest_dir, part_path, total_size, bytes_done, completed_ranges,
                status, created_at)
             VALUES ('keep-me','http://a/x','http://a/x','http://a/x','hint',
                'movie.mkv','/tmp','/tmp/movie.mkv.slpart', 999, 4096, ?1, 'paused', 42)",
            params![ranges.encode()],
        )
        .unwrap();
        c.pragma_update(None, "user_version", 1i64).unwrap();
    }

    #[test]
    fn a_version_1_database_migrates_without_losing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        v1_database(&path);

        let store = Store::open(&path).unwrap();
        let rec = store.get("keep-me").unwrap().expect("the row survived");

        assert_eq!(rec.filename, "movie.mkv");
        assert_eq!(rec.bytes_done, 4096);
        assert_eq!(rec.total_size, Some(999));
        assert_eq!(rec.completed_ranges.total(), 4096, "resume state is intact");
        assert_eq!(rec.status, DownloadStatus::Paused);
        assert_eq!(rec.created_at, 42);
        assert_eq!(
            rec.priority,
            Priority::Normal,
            "a column added by a migration takes its default, not a null"
        );
    }

    #[test]
    fn migrating_twice_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("v1.db");
        v1_database(&path);

        drop(Store::open(&path).unwrap());
        // Re-opening must not try to ALTER a column that now exists.
        let store = Store::open(&path).unwrap();
        assert!(store.get("keep-me").unwrap().is_some());
    }

    #[test]
    fn a_database_from_a_newer_build_is_refused_rather_than_guessed_at() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("future.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(SCHEMA).unwrap();
            c.pragma_update(None, "user_version", SCHEMA_VERSION + 5)
                .unwrap();
        }
        // Opening read-write with an unknown shape is how data gets lost.
        assert!(matches!(Store::open(&path), Err(StoreError::Schema(_))));
    }

    #[test]
    fn priority_survives_a_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.db");
        let id = {
            let store = Store::open(&path).unwrap();
            let rec = DownloadRecord::new("http://a/x", "f.bin", "/tmp", "/tmp/f.bin.slpart");
            store.insert(&rec).unwrap();
            store.set_priority(&rec.id, Priority::High).unwrap();
            rec.id
        };
        let store = Store::open(&path).unwrap();
        assert_eq!(store.get(&id).unwrap().unwrap().priority, Priority::High);
    }

    #[test]
    fn the_event_ring_keeps_the_most_recent_entries_and_no_more() {
        let store = Store::open_in_memory().unwrap();
        let rec = DownloadRecord::new("http://a/x", "f.bin", "/tmp", "/tmp/f.bin.slpart");
        store.insert(&rec).unwrap();

        for i in 0..(EVENT_RING + 50) {
            store
                .record_event(&rec.id, "tick", &format!("entry {i}"))
                .unwrap();
        }

        let events = store.events(&rec.id).unwrap();
        assert_eq!(events.len(), EVENT_RING as usize, "the ring is bounded");
        assert_eq!(
            events.first().unwrap().detail,
            format!("entry {}", 50),
            "the oldest entries are the ones dropped"
        );
        assert_eq!(
            events.last().unwrap().detail,
            format!("entry {}", EVENT_RING + 49),
            "and events read oldest-first"
        );
    }

    #[test]
    fn deleting_a_download_takes_its_timeline_with_it() {
        let store = Store::open_in_memory().unwrap();
        let rec = DownloadRecord::new("http://a/x", "f.bin", "/tmp", "/tmp/f.bin.slpart");
        store.insert(&rec).unwrap();
        store.record_event(&rec.id, "added", "x").unwrap();

        store.delete(&rec.id).unwrap();
        assert!(store.events(&rec.id).unwrap().is_empty());
    }
}
