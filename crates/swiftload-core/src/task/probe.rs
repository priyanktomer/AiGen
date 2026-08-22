//! Metadata detection.
//!
//! # Why a 1-byte ranged GET instead of HEAD
//!
//! HEAD is unreliable in the field: some servers answer 405, some return a `Content-Length`
//! that differs from the GET body, many CDNs do not reflect `Accept-Ranges` on HEAD, and
//! signed-URL gateways sometimes reject it outright. `GET` with `Range: bytes=0-0` is ground
//! truth — a `206` with `Content-Range: bytes 0-0/N` proves range support *and* the total size
//! in a single request, using one byte of transfer.

use crate::{
    config::{RequestSpec, Settings},
    http::{
        client,
        errors::{classify_status, classify_transport, ErrorClass},
        headers::{self, Validator},
        redirect::{self, Hop, RedirectError},
    },
    util::filename,
};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// What we learned about the server's range support. `Unknown` never occurs after a
/// successful probe; it exists for records loaded from an older schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RangeSupport {
    #[default]
    Unknown,
    /// Verified: the server answered a ranged request with 206 and a matching Content-Range.
    Supported,
    /// Verified absent: the server answered 200, or advertised `Accept-Ranges: none`.
    Unsupported,
    /// Advertised support, then ignored a range mid-download. Never segment this host again.
    Lied,
}

#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// The URL as resolved after redirects — what segment requests actually target.
    pub final_url: url::Url,
    pub filename: String,
    pub total_size: Option<u64>,
    pub range_support: RangeSupport,
    pub etag: Option<Validator>,
    pub last_modified: Option<String>,
    pub content_type: Option<String>,
    pub disposition_filename: Option<String>,
    /// Server applied a transforming Content-Encoding despite our `identity` request, so byte
    /// offsets are meaningless and the download must run as a single stream.
    pub transforming_encoding: bool,
    pub http_version: String,
    pub redirect_chain: Vec<Hop>,
}

impl ProbeResult {
    pub fn is_resumable(&self) -> bool {
        self.range_support == RangeSupport::Supported && !self.transforming_encoding
    }

    /// Whether this file is worth splitting at all.
    pub fn is_segmentable(&self) -> bool {
        self.is_resumable() && self.total_size.is_some_and(|s| s >= crate::config::SMALL_FILE_THRESHOLD)
    }

    /// A cheap key for *finding candidate* duplicate downloads.
    ///
    /// Deliberately weak, and never treated as proof: a filename and a size collide by
    /// accident and can be forged trivially. It exists only so the UI can ask "you may already
    /// have this — resume it?", after which the real identity check runs.
    pub fn identity_hint(&self) -> String {
        let mut h = Sha256::new();
        h.update(self.filename.to_lowercase().as_bytes());
        h.update(b":");
        h.update(self.total_size.unwrap_or(0).to_le_bytes());
        format!("{:x}", h.finalize())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error("unsupported url scheme {0:?}: only http and https are downloadable")]
    UnsupportedScheme(String),
    #[error("invalid url: {0}")]
    InvalidUrl(String),
    #[error("redirect refused: {0}")]
    Redirect(RedirectError),
    #[error("server returned {status}")]
    Status { status: u16, class: ErrorClass },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("client setup failed: {0}")]
    Client(String),
}

impl ProbeError {
    pub fn class(&self) -> ErrorClass {
        match self {
            Self::Status { class, .. } => class.clone(),
            Self::Transport(_) => ErrorClass::TransientNetwork,
            _ => ErrorClass::Fatal,
        }
    }
}

/// Validate that a URL is something we are willing to download.
pub fn validate_url(raw: &str) -> Result<url::Url, ProbeError> {
    let url = url::Url::parse(raw).map_err(|e| ProbeError::InvalidUrl(e.to_string()))?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        other => Err(ProbeError::UnsupportedScheme(other.to_string())),
    }
}

/// Probe a URL for everything the planner needs.
///
/// `url_previously_worked` steers 403 classification: on a link that used to work it means
/// "the signature expired, go get a fresh one", and on a first attempt it means "you never
/// had access". Same status, completely different remedy.
pub async fn probe(
    raw_url: &str,
    settings: &Settings,
    spec: &RequestSpec,
    url_previously_worked: bool,
) -> Result<ProbeResult, ProbeError> {
    let mut current = validate_url(raw_url)?;
    let mut chain: Vec<Hop> = Vec::new();
    let origin = current.clone();

    for hop in 0..=settings.max_redirects {
        let http = client::build(settings, spec, &current).map_err(|e| ProbeError::Client(e.to_string()))?;

        // Strip caller-supplied credentials once we have left the original origin.
        let effective_spec = if redirect::is_cross_origin(&origin, &current) {
            RequestSpec { cookies: None, headers: strip_auth(&spec.headers), ..spec.clone() }
        } else {
            spec.clone()
        };

        let req = client::apply_spec(http.get(current.clone()), &effective_spec)
            .header(reqwest::header::RANGE, "bytes=0-0")
            .timeout(settings.connect_timeout + Duration::from_secs(15));

        let resp = req.send().await.map_err(|e| {
            let _ = classify_transport(&e);
            ProbeError::Transport(e.to_string())
        })?;

        let status = resp.status().as_u16();

        if (300..400).contains(&status) {
            if hop == settings.max_redirects {
                return Err(ProbeError::Redirect(RedirectError::TooMany(settings.max_redirects)));
            }
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| ProbeError::Redirect(RedirectError::Malformed("no Location".into())))?
                .to_string();

            chain.push(redirect::record_hop(&current, status));
            current = redirect::next_url(&current, &location, !settings.block_insecure_redirect)
                .map_err(ProbeError::Redirect)?;
            continue;
        }

        if !(200..300).contains(&status) {
            let retry_after = resp
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| headers::parse_retry_after(v, Duration::from_secs(120)));
            return Err(ProbeError::Status {
                status,
                class: classify_status(status, retry_after, url_previously_worked),
            });
        }

        return Ok(interpret(resp, current, chain));
    }

    Err(ProbeError::Redirect(RedirectError::TooMany(settings.max_redirects)))
}

fn strip_auth(headers: &[(String, String)]) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(k, _)| {
            !k.eq_ignore_ascii_case("authorization")
                && !k.eq_ignore_ascii_case("cookie")
                && !k.eq_ignore_ascii_case("proxy-authorization")
        })
        .cloned()
        .collect()
}

fn interpret(resp: reqwest::Response, final_url: url::Url, chain: Vec<Hop>) -> ProbeResult {
    let status = resp.status().as_u16();
    let h = resp.headers();

    let get = |name: reqwest::header::HeaderName| {
        h.get(name).and_then(|v| v.to_str().ok()).map(str::to_string)
    };

    let content_range = get(reqwest::header::CONTENT_RANGE)
        .as_deref()
        .and_then(headers::parse_content_range);
    let content_length = get(reqwest::header::CONTENT_LENGTH).and_then(|v| v.parse::<u64>().ok());
    let accept_ranges = get(reqwest::header::ACCEPT_RANGES);

    // Any non-identity encoding means byte ranges address the *compressed* stream while the
    // client decompresses transparently — every segment offset would be silently wrong.
    let transforming_encoding = get(reqwest::header::CONTENT_ENCODING)
        .as_deref()
        .is_some_and(headers::is_transforming_encoding);

    let (range_support, total_size) = match status {
        // The good case: ranges work and the total is stated authoritatively.
        206 => match content_range {
            Some(cr) => (RangeSupport::Supported, cr.total),
            // 206 without a parseable Content-Range is a broken server; do not trust ranges.
            None => (RangeSupport::Unsupported, content_length),
        },
        // The server ignored our Range and sent the whole body: no range support, and
        // Content-Length is the full size rather than a slice.
        _ => {
            let advertised_none = accept_ranges.as_deref().is_some_and(|v| v.eq_ignore_ascii_case("none"));
            let _ = advertised_none;
            (RangeSupport::Unsupported, content_length)
        }
    };

    let disposition = get(reqwest::header::CONTENT_DISPOSITION);
    let content_type = get(reqwest::header::CONTENT_TYPE);
    let name = filename::resolve(disposition.as_deref(), &final_url, content_type.as_deref());

    let result = ProbeResult {
        filename: name,
        total_size,
        // Never trust ranges when the body is transformed, whatever the status said.
        range_support: if transforming_encoding { RangeSupport::Unsupported } else { range_support },
        etag: get(reqwest::header::ETAG).as_deref().and_then(Validator::parse),
        last_modified: get(reqwest::header::LAST_MODIFIED),
        disposition_filename: disposition
            .as_deref()
            .and_then(filename::parse_content_disposition),
        content_type,
        transforming_encoding,
        http_version: format!("{:?}", resp.version()),
        final_url,
        redirect_chain: chain,
    };

    // Drop the response without reading the body. For a 200 that means we are abandoning a
    // potentially multi-GB stream after one byte, which is exactly the intent.
    drop(resp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_and_https_are_downloadable() {
        assert!(validate_url("https://example.com/f").is_ok());
        assert!(validate_url("http://example.com/f").is_ok());
        for bad in ["file:///etc/passwd", "data:text/plain,x", "javascript:alert(1)", "ftp://h/f"] {
            assert!(
                matches!(validate_url(bad), Err(ProbeError::UnsupportedScheme(_))),
                "{bad} should be refused"
            );
        }
        assert!(matches!(validate_url("not a url"), Err(ProbeError::InvalidUrl(_))));
    }

    #[test]
    fn strips_credentials_when_leaving_the_origin() {
        let h = vec![
            ("Authorization".to_string(), "Bearer secret".to_string()),
            ("Cookie".to_string(), "session=abc".to_string()),
            ("X-Custom".to_string(), "keep".to_string()),
        ];
        let stripped = strip_auth(&h);
        assert_eq!(stripped.len(), 1);
        assert_eq!(stripped[0].0, "X-Custom");
    }

    fn result_with(size: Option<u64>, support: RangeSupport, encoding: bool) -> ProbeResult {
        ProbeResult {
            final_url: url::Url::parse("https://example.com/f.bin").unwrap(),
            filename: "f.bin".into(),
            total_size: size,
            range_support: support,
            etag: None,
            last_modified: None,
            content_type: None,
            disposition_filename: None,
            transforming_encoding: encoding,
            http_version: "HTTP/1.1".into(),
            redirect_chain: vec![],
        }
    }

    #[test]
    fn resumability_requires_ranges_and_an_untransformed_body() {
        assert!(result_with(Some(1 << 30), RangeSupport::Supported, false).is_resumable());
        assert!(!result_with(Some(1 << 30), RangeSupport::Unsupported, false).is_resumable());
        // Even with ranges, a compressed body makes offsets meaningless.
        assert!(!result_with(Some(1 << 30), RangeSupport::Supported, true).is_resumable());
    }

    #[test]
    fn small_files_are_not_worth_segmenting() {
        // Handshakes cost more than the parallelism recovers below the threshold.
        assert!(!result_with(Some(1024), RangeSupport::Supported, false).is_segmentable());
        assert!(!result_with(None, RangeSupport::Supported, false).is_segmentable());
        assert!(result_with(Some(100 << 20), RangeSupport::Supported, false).is_segmentable());
    }

    #[test]
    fn identity_hint_is_stable_and_size_sensitive() {
        let a = result_with(Some(1000), RangeSupport::Supported, false);
        let b = result_with(Some(1000), RangeSupport::Supported, false);
        assert_eq!(a.identity_hint(), b.identity_hint());

        let c = result_with(Some(1001), RangeSupport::Supported, false);
        assert_ne!(a.identity_hint(), c.identity_hint(), "size must affect the hint");

        let mut d = result_with(Some(1000), RangeSupport::Supported, false);
        d.filename = "other.bin".into();
        assert_ne!(a.identity_hint(), d.identity_hint(), "name must affect the hint");
    }

    #[test]
    fn identity_hint_ignores_filename_case() {
        let mut a = result_with(Some(10), RangeSupport::Supported, false);
        a.filename = "Movie.MKV".into();
        let mut b = result_with(Some(10), RangeSupport::Supported, false);
        b.filename = "movie.mkv".into();
        assert_eq!(a.identity_hint(), b.identity_hint());
    }
}
