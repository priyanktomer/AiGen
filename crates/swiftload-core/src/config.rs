//! Engine configuration.
//!
//! Defaults are deliberately conservative. 32 connections is available as a manual override
//! and as a benchmark data point, but it is not a default: many servers treat a connection
//! flood as abuse, and on a link that is already saturated the extra connections buy nothing
//! while degrading the user's own interactive latency.

use serde::{Deserialize, Serialize};
use std::{path::PathBuf, time::Duration};

/// Never segment below this. Smaller pieces spend more on handshakes than they recover.
pub const MIN_SEGMENT: u64 = 2 * 1024 * 1024;
/// Below this size, segmentation is a net loss.
pub const SMALL_FILE_THRESHOLD: u64 = 4 * 1024 * 1024;
/// Streaming read granularity.
pub const CHUNK_SIZE: usize = 64 * 1024;
/// Bytes written between durability checkpoints.
pub const CHECKPOINT_BYTES: u64 = 8 * 1024 * 1024;
/// Wall-clock between durability checkpoints.
pub const CHECKPOINT_INTERVAL: Duration = Duration::from_secs(5);
/// How much of each completed span to give back after an unclean shutdown.
pub const PARANOID_REWIND: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    /// Downloads running at once.
    pub max_concurrent_downloads: usize,
    /// Ceiling for one download's connection count.
    pub max_conns_per_download: usize,
    /// Ceiling across all downloads, so 3 downloads x 32 connections cannot happen.
    pub max_total_conns: usize,
    /// Ceiling per remote host. Good citizenship, and most servers cap anyway.
    pub max_conns_per_host: usize,
    /// Let the governor pick the connection count. Off means "use max_conns_per_download".
    pub adaptive_concurrency: bool,
    pub max_retries_per_segment: u32,
    pub connect_timeout: Duration,
    /// Idle time between reads, not a whole-body deadline: a whole-body timeout on a 4 GB
    /// download is a bug, not a safety net.
    pub read_idle_timeout: Duration,
    /// Zero-byte duration after which a connection is killed and respawned.
    pub stall_timeout: Duration,
    pub max_redirects: usize,
    /// Refuse https -> http redirects.
    pub block_insecure_redirect: bool,
    /// Give back a margin of each span after a crash, in case fsync lied.
    pub paranoid_recovery: bool,
    /// Hash the finished file so integrity can be reported as verified rather than assumed.
    pub hash_on_complete: bool,
    /// Apply the Mark-of-the-Web so SmartScreen behaves as it would for a browser download.
    pub apply_motw: bool,
    pub collision_policy: CollisionPolicy,
    pub user_agent: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum CollisionPolicy {
    /// "file (2).ext", Explorer-style.
    Rename,
    Overwrite,
    Ask,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: default_download_dir(),
            max_concurrent_downloads: 3,
            max_conns_per_download: 8,
            max_total_conns: 24,
            max_conns_per_host: 8,
            adaptive_concurrency: true,
            max_retries_per_segment: 8,
            connect_timeout: Duration::from_secs(15),
            read_idle_timeout: Duration::from_secs(20),
            stall_timeout: Duration::from_secs(20),
            max_redirects: 10,
            block_insecure_redirect: true,
            paranoid_recovery: true,
            hash_on_complete: true,
            apply_motw: true,
            collision_policy: CollisionPolicy::Rename,
            user_agent: format!(
                "SwiftLoad/{} (+https://github.com/priyanktomer/AiGen)",
                env!("CARGO_PKG_VERSION")
            ),
        }
    }
}

fn default_download_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-request knobs. Present from the start even though no UI exposes them yet, so browser
/// integration, authenticated downloads, custom headers and proxying are later a UI change
/// rather than an engine change.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestSpec {
    pub headers: Vec<(String, String)>,
    pub cookies: Option<String>,
    pub referer: Option<String>,
    pub user_agent: Option<String>,
    pub proxy: Option<String>,
}

/// Governor tuning. Split out so the benchmark harness can sweep these without touching code.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct GovernorConfig {
    /// Discarded after a concurrency change, to skip TCP slow-start and TLS.
    pub warmup: Duration,
    /// Measurement window at each level.
    pub dwell: Duration,
    /// Aggregate improvement needed to keep an added level. Noise over a ~1.2 s window is
    /// easily +/-10%, so a lower bar just makes the governor chase noise.
    pub gain_threshold: f64,
    /// Minimum gap between opportunistic re-probes in steady state.
    pub reprobe_interval: Duration,
    /// Ratio of per-connection throughput below which we conclude the pipe is saturated
    /// rather than per-connection capped.
    pub saturation_ratio: f64,
    pub max_conns: usize,
}

impl Default for GovernorConfig {
    fn default() -> Self {
        Self {
            warmup: Duration::from_millis(400),
            dwell: Duration::from_millis(1200),
            gain_threshold: 0.15,
            reprobe_interval: Duration::from_secs(30),
            saturation_ratio: 0.6,
            max_conns: 32,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative_and_internally_consistent() {
        let s = Settings::default();
        assert!(
            s.max_conns_per_download <= 8,
            "default must not be a connection flood"
        );
        assert!(s.max_total_conns >= s.max_conns_per_download);
        assert!(s.max_conns_per_host <= s.max_conns_per_download);
        assert!(
            s.block_insecure_redirect,
            "downgrade must be refused by default"
        );
        assert!(s.paranoid_recovery);
        assert!(s.apply_motw);
    }

    #[test]
    fn user_agent_identifies_the_client_honestly() {
        // No spoofing a browser: rate limits are honoured, not evaded.
        let ua = Settings::default().user_agent;
        assert!(ua.starts_with("SwiftLoad/"), "{ua}");
        assert!(!ua.to_lowercase().contains("mozilla"), "{ua}");
    }

    #[test]
    fn settings_roundtrip_through_json() {
        let s = Settings::default();
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.max_conns_per_download, s.max_conns_per_download);
        assert_eq!(back.collision_policy, s.collision_policy);
    }

    #[test]
    fn partial_settings_json_fills_defaults() {
        // Forward compatibility: an older config file must not fail to load.
        let s: Settings = serde_json::from_str(r#"{"max_conns_per_download": 16}"#).unwrap();
        assert_eq!(s.max_conns_per_download, 16);
        assert_eq!(
            s.max_concurrent_downloads, 3,
            "unspecified fields take defaults"
        );
    }
}
