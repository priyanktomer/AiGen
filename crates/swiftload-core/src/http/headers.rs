//! Response-header parsing.
//!
//! Everything here handles input from an untrusted server, so every parser returns an Option
//! rather than panicking, and nothing is assumed to be well-formed.

use std::time::Duration;

/// A parsed `Content-Range: bytes START-END/TOTAL` response header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentRange {
    pub start: u64,
    pub end: u64,
    /// `None` when the server sent `*`, meaning it will not commit to a total size.
    pub total: Option<u64>,
}

impl ContentRange {
    pub fn len(&self) -> u64 {
        self.end + 1 - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub fn parse_content_range(v: &str) -> Option<ContentRange> {
    let spec = v.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = spec.split_once('/')?;
    let (a, b) = range.trim().split_once('-')?;
    let start = a.trim().parse().ok()?;
    let end = b.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    // A server claiming a range that extends past its own stated total is inconsistent;
    // treating that as unparseable is safer than trusting half of it.
    if let Some(t) = total {
        if end >= t {
            return None;
        }
    }
    Some(ContentRange { start, end, total })
}

/// `Retry-After`, in either of its two legal forms: delta-seconds or an HTTP-date.
///
/// Capped, because a server asking us to wait a week is not something to obey literally.
pub fn parse_retry_after(v: &str, cap: Duration) -> Option<Duration> {
    let v = v.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(cap));
    }
    let when = httpdate::parse_http_date(v).ok()?;
    let now = std::time::SystemTime::now();
    Some(when.duration_since(now).unwrap_or(Duration::ZERO).min(cap))
}

/// An entity validator, used for `If-Range` and for identity checks after a URL swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Validator {
    /// `"abc"` — asserts byte-for-byte identity.
    Strong(String),
    /// `W/"abc"` — asserts only semantic equivalence, so it is never sufficient on its own.
    Weak(String),
}

impl Validator {
    pub fn parse(v: &str) -> Option<Self> {
        let v = v.trim();
        if v.is_empty() {
            return None;
        }
        match v.strip_prefix("W/").or_else(|| v.strip_prefix("w/")) {
            Some(rest) => Some(Self::Weak(rest.trim().to_string())),
            None => Some(Self::Strong(v.to_string())),
        }
    }

    pub fn is_strong(&self) -> bool {
        matches!(self, Self::Strong(_))
    }

    pub fn raw(&self) -> &str {
        match self {
            Self::Strong(s) | Self::Weak(s) => s,
        }
    }

    /// The value to send back in `If-Range`.
    pub fn as_header(&self) -> String {
        match self {
            Self::Strong(s) => s.clone(),
            Self::Weak(s) => format!("W/{s}"),
        }
    }
}

/// Whether a `Content-Encoding` value means the body is transformed.
///
/// This gates a genuine silent-corruption bug: if the server compresses the body, byte ranges
/// address the *compressed* stream while the client decompresses transparently, so every
/// segment offset is wrong. Detecting it forces a single-stream download.
pub fn is_transforming_encoding(v: &str) -> bool {
    let v = v.trim().to_ascii_lowercase();
    !(v.is_empty() || v == "identity")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_content_range() {
        assert_eq!(
            parse_content_range("bytes 0-0/12345"),
            Some(ContentRange { start: 0, end: 0, total: Some(12345) })
        );
        assert_eq!(
            parse_content_range("bytes 1000-1999/5000"),
            Some(ContentRange { start: 1000, end: 1999, total: Some(5000) })
        );
        assert_eq!(parse_content_range("bytes 0-0/12345").unwrap().len(), 1);
    }

    #[test]
    fn parses_unknown_total() {
        let cr = parse_content_range("bytes 5-9/*").unwrap();
        assert_eq!(cr.total, None);
        assert_eq!(cr.len(), 5);
    }

    #[test]
    fn rejects_inconsistent_or_malformed_ranges() {
        assert_eq!(parse_content_range("bytes 100-50/1000"), None, "end before start");
        assert_eq!(parse_content_range("bytes 0-999/500"), None, "range exceeds stated total");
        assert_eq!(parse_content_range("items 0-1/2"), None, "wrong unit");
        assert_eq!(parse_content_range("garbage"), None);
        assert_eq!(parse_content_range(""), None);
        assert_eq!(parse_content_range("bytes 0-1"), None, "missing total");
    }

    #[test]
    fn retry_after_accepts_seconds_and_dates() {
        let cap = Duration::from_secs(120);
        assert_eq!(parse_retry_after("7", cap), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("  30 ", cap), Some(Duration::from_secs(30)));

        // A date in the past means "now".
        assert_eq!(
            parse_retry_after("Wed, 01 Jan 2020 00:00:00 GMT", cap),
            Some(Duration::ZERO)
        );
        assert!(parse_retry_after("not a date", cap).is_none());
    }

    #[test]
    fn retry_after_is_capped() {
        // Obeying a week-long Retry-After literally would hang the download forever.
        let cap = Duration::from_secs(120);
        assert_eq!(parse_retry_after("604800", cap), Some(cap));
    }

    #[test]
    fn distinguishes_strong_from_weak_validators() {
        let s = Validator::parse("\"abc123\"").unwrap();
        assert!(s.is_strong());
        assert_eq!(s.as_header(), "\"abc123\"");

        let w = Validator::parse("W/\"abc123\"").unwrap();
        assert!(!w.is_strong(), "weak validators assert equivalence, not byte identity");
        assert_eq!(w.raw(), "\"abc123\"");
        assert_eq!(w.as_header(), "W/\"abc123\"");

        assert!(Validator::parse("  ").is_none());
    }

    #[test]
    fn detects_transforming_content_encoding() {
        // The silent-corruption gate: any non-identity encoding invalidates byte offsets.
        assert!(is_transforming_encoding("gzip"));
        assert!(is_transforming_encoding("GZIP"));
        assert!(is_transforming_encoding("br"));
        assert!(is_transforming_encoding("deflate"));
        assert!(!is_transforming_encoding("identity"));
        assert!(!is_transforming_encoding(""));
        assert!(!is_transforming_encoding("  "));
    }
}
