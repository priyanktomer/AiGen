//! Error classification.
//!
//! Retry policy is only as good as the classification feeding it: retrying a 404 wastes time,
//! and giving up on a connection reset throws away a download. The distinctions that matter
//! most are `RateLimited` (back off *and* reduce concurrency — never route around a limit
//! with more connections) and `NetworkDown` (do not consume the retry budget while the
//! machine has no connectivity at all).

use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorClass {
    /// Reset, timeout, DNS hiccup, TLS handshake failure. Retry with backoff.
    TransientNetwork,
    /// 5xx, 408, 425. Retry with backoff.
    TransientServer,
    /// 429, or 503 with Retry-After. Back off *and* halve concurrency.
    RateLimited { retry_after: Option<Duration> },
    /// 403 on a URL that previously worked — typically an expired signature.
    UrlExpired,
    /// 401/404/410/451, or 403 on the very first request.
    Fatal,
    /// Server ignored `Range` on the initial request: no segmentation, but still downloadable.
    RangeUnsupported,
    /// Server advertised range support, then ignored it mid-download. The dangerous one.
    RangeLied,
    /// Validators or size changed: the resource is not what we started downloading.
    ResourceChanged,
    /// Disk full, permission denied, path too long.
    LocalFatal,
    /// Every worker failed to connect at once — the machine is offline.
    NetworkDown,
}

impl ErrorClass {
    /// Whether a retry could plausibly succeed without user intervention.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::TransientNetwork | Self::TransientServer | Self::RateLimited { .. } | Self::NetworkDown
        )
    }

    /// Whether this should reduce the connection count. A rate limit answered with *more*
    /// connections is both abusive and slower.
    pub fn should_reduce_concurrency(&self) -> bool {
        matches!(self, Self::RateLimited { .. })
    }

    /// Whether the partial file must be preserved for a possible resume.
    pub fn preserves_partial(&self) -> bool {
        !matches!(self, Self::ResourceChanged)
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::TransientNetwork => "transient_network",
            Self::TransientServer => "transient_server",
            Self::RateLimited { .. } => "rate_limited",
            Self::UrlExpired => "url_expired",
            Self::Fatal => "fatal",
            Self::RangeUnsupported => "range_unsupported",
            Self::RangeLied => "range_lied",
            Self::ResourceChanged => "resource_changed",
            Self::LocalFatal => "local_fatal",
            Self::NetworkDown => "network_down",
        }
    }
}

/// Classify an HTTP status.
///
/// `url_previously_worked` is what separates "this link has expired, go get a fresh one" from
/// "you never had access". Same status code, completely different remedy.
pub fn classify_status(
    status: u16,
    retry_after: Option<Duration>,
    url_previously_worked: bool,
) -> ErrorClass {
    match status {
        429 => ErrorClass::RateLimited { retry_after },
        503 => ErrorClass::RateLimited { retry_after },
        500 | 502 | 504 | 408 | 425 => ErrorClass::TransientServer,
        403 if url_previously_worked => ErrorClass::UrlExpired,
        416 => ErrorClass::ResourceChanged,
        401 | 403 | 404 | 410 | 451 => ErrorClass::Fatal,
        s if (500..600).contains(&s) => ErrorClass::TransientServer,
        _ => ErrorClass::Fatal,
    }
}

/// Classify a transport-level failure.
pub fn classify_transport(err: &reqwest::Error) -> ErrorClass {
    if err.is_timeout() || err.is_connect() || err.is_request() || err.is_body() || err.is_decode() {
        return ErrorClass::TransientNetwork;
    }
    ErrorClass::TransientNetwork
}

/// Classify a local I/O failure. Disk-full must never be retried in a loop.
pub fn classify_io(err: &std::io::Error) -> ErrorClass {
    use std::io::ErrorKind::*;
    match err.kind() {
        PermissionDenied | NotFound | InvalidInput | AlreadyExists => ErrorClass::LocalFatal,
        StorageFull => ErrorClass::LocalFatal,
        _ => {
            // ENOSPC / ERROR_DISK_FULL may arrive as Other on some platforms.
            let msg = err.to_string().to_lowercase();
            if msg.contains("no space") || msg.contains("disk full") {
                ErrorClass::LocalFatal
            } else {
                ErrorClass::TransientNetwork
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limits_back_off_and_reduce_concurrency() {
        let c = classify_status(429, Some(Duration::from_secs(5)), false);
        assert!(c.is_retryable());
        assert!(c.should_reduce_concurrency(), "429 must reduce, never increase, concurrency");
        assert_eq!(c, ErrorClass::RateLimited { retry_after: Some(Duration::from_secs(5)) });

        assert!(classify_status(503, None, false).should_reduce_concurrency());
    }

    #[test]
    fn forbidden_means_expiry_only_if_the_url_used_to_work() {
        // The distinction that makes URL refresh possible: same status, different remedy.
        assert_eq!(classify_status(403, None, true), ErrorClass::UrlExpired);
        assert_eq!(classify_status(403, None, false), ErrorClass::Fatal);
    }

    #[test]
    fn terminal_statuses_are_not_retried() {
        for s in [401, 404, 410, 451] {
            let c = classify_status(s, None, true);
            assert_eq!(c, ErrorClass::Fatal, "status {s}");
            assert!(!c.is_retryable(), "status {s} must not be retried");
        }
    }

    #[test]
    fn server_errors_are_transient() {
        for s in [500, 502, 504, 408, 425, 507] {
            assert!(classify_status(s, None, false).is_retryable(), "status {s}");
        }
    }

    #[test]
    fn range_not_satisfiable_means_the_resource_changed() {
        // The file shrank or was replaced; resuming into it would corrupt the result.
        let c = classify_status(416, None, true);
        assert_eq!(c, ErrorClass::ResourceChanged);
        assert!(!c.preserves_partial(), "stale partial must not be silently reused");
    }

    #[test]
    fn partial_file_is_preserved_for_everything_except_a_changed_resource() {
        for c in [
            ErrorClass::UrlExpired,
            ErrorClass::Fatal,
            ErrorClass::NetworkDown,
            ErrorClass::LocalFatal,
            ErrorClass::RangeUnsupported,
        ] {
            assert!(c.preserves_partial(), "{c:?} must keep the partial file");
        }
    }

    #[test]
    fn disk_full_is_locally_fatal_not_a_retry_loop() {
        let e = std::io::Error::new(std::io::ErrorKind::Other, "No space left on device");
        assert_eq!(classify_io(&e), ErrorClass::LocalFatal);
        assert!(!classify_io(&e).is_retryable());

        let e = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert_eq!(classify_io(&e), ErrorClass::LocalFatal);
    }

    #[test]
    fn network_down_is_retryable_but_handled_separately() {
        // Retryable so we keep waiting, but the caller must not spend the retry budget on it.
        assert!(ErrorClass::NetworkDown.is_retryable());
        assert!(ErrorClass::NetworkDown.preserves_partial());
    }
}
