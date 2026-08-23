//! Persisted record types.

use crate::util::intervals::RangeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadStatus {
    Queued,
    Active,
    Paused,
    Retrying,
    WaitingNetwork,
    /// Blocked on the user: an expired link, or a resource that changed.
    NeedsAttention,
    Completed,
    Failed,
    Cancelled,
}

impl DownloadStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Active => "active",
            Self::Paused => "paused",
            Self::Retrying => "retrying",
            Self::WaitingNetwork => "waiting_network",
            Self::NeedsAttention => "needs_attention",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
    /// Parse a value as stored in the database. Unknown values fall back to a safe default
    /// rather than failing, so an older or newer schema cannot make a row unreadable.
    pub fn from_db(s: &str) -> Self {
        match s {
            "active" => Self::Active,
            "paused" => Self::Paused,
            "retrying" => Self::Retrying,
            "waiting_network" => Self::WaitingNetwork,
            "needs_attention" => Self::NeedsAttention,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Queued,
        }
    }
    /// Whether this download still has work outstanding.
    pub fn is_resumable(&self) -> bool {
        matches!(
            self,
            Self::Queued
                | Self::Paused
                | Self::Failed
                | Self::NeedsAttention
                | Self::WaitingNetwork
                | Self::Retrying
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationState {
    NotRequired,
    AutoVerified,
    ContentVerified,
    UserConfirmed,
    Rejected,
}

impl ValidationState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::NotRequired => "not_required",
            Self::AutoVerified => "auto_verified",
            Self::ContentVerified => "content_verified",
            Self::UserConfirmed => "user_confirmed",
            Self::Rejected => "rejected",
        }
    }
    /// Parse a value as stored in the database. Unknown values fall back to a safe default
    /// rather than failing, so an older or newer schema cannot make a row unreadable.
    pub fn from_db(s: &str) -> Self {
        match s {
            "auto_verified" => Self::AutoVerified,
            "content_verified" => Self::ContentVerified,
            "user_confirmed" => Self::UserConfirmed,
            "rejected" => Self::Rejected,
            _ => Self::NotRequired,
        }
    }
}

/// Where a URL came from. Browser integration will supply the same command with a different
/// source, which is why this exists before any extension does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrlSource {
    User,
    BrowserExtension,
    AutoReresolve,
}

impl UrlSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::User => "user",
            Self::BrowserExtension => "browser_extension",
            Self::AutoReresolve => "auto_reresolve",
        }
    }
}

#[derive(Debug, Clone)]
pub struct DownloadRecord {
    pub id: String,
    /// The first URL the user gave. Never overwritten: it is what a signed link is
    /// re-resolved from when the current one expires.
    pub original_url: String,
    /// The URL actively fetched from. Replaced by a refresh.
    pub current_url: String,
    /// `current_url` after redirects.
    pub final_url: String,
    pub url_refresh_count: i64,
    /// Filename plus size. Weak by design — only used to *offer* a duplicate resume.
    pub identity_hint: String,
    pub filename: String,
    pub dest_dir: String,
    pub part_path: String,
    pub category: String,
    pub total_size: Option<u64>,
    pub bytes_done: u64,
    /// The authoritative resume state.
    pub completed_ranges: RangeSet,
    pub status: DownloadStatus,
    pub accept_ranges: i64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub http_version: Option<String>,
    pub validation_state: ValidationState,
    pub max_connections: Option<usize>,
    pub retry_count: u64,
    pub created_at: i64,
    pub completed_at: Option<i64>,
    pub error_message: Option<String>,
    /// False on load means the last run did not shut down cleanly.
    pub clean_shutdown: bool,
}

impl DownloadRecord {
    pub fn new(url: &str, filename: &str, dest_dir: &str, part_path: &str) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            original_url: url.to_string(),
            current_url: url.to_string(),
            final_url: url.to_string(),
            url_refresh_count: 0,
            identity_hint: String::new(),
            filename: filename.to_string(),
            dest_dir: dest_dir.to_string(),
            part_path: part_path.to_string(),
            category: "other".into(),
            total_size: None,
            bytes_done: 0,
            completed_ranges: RangeSet::new(),
            status: DownloadStatus::Queued,
            accept_ranges: 0,
            etag: None,
            last_modified: None,
            content_type: None,
            http_version: None,
            validation_state: ValidationState::NotRequired,
            max_connections: None,
            retry_count: 0,
            created_at: super::now(),
            completed_at: None,
            error_message: None,
            clean_shutdown: false,
        }
    }

    pub fn remaining(&self) -> Option<u64> {
        self.total_size.map(|t| t.saturating_sub(self.bytes_done))
    }

    pub fn percent(&self) -> Option<f64> {
        self.total_size
            .filter(|t| *t > 0)
            .map(|t| self.bytes_done as f64 / t as f64 * 100.0)
    }
}

#[derive(Debug, Clone)]
pub struct UrlHistoryEntry {
    pub seq: i64,
    /// Always redacted: a signed URL is a bearer credential.
    pub url_redacted: String,
    pub host: String,
    pub source: String,
    pub outcome: String,
    pub bytes_done_at_swap: u64,
    pub added_at: i64,
}

#[derive(Debug, Clone)]
pub struct HostProfile {
    pub host: String,
    pub best_observed_conns: Option<usize>,
    pub best_observed_bps: Option<u64>,
    pub saturation_detected: bool,
    pub samples: u64,
}
