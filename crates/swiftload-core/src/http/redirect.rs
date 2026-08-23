//! Manual redirect handling.
//!
//! Done by hand rather than with reqwest's built-in policy so three rules actually apply:
//! no protocol downgrade, no credential leakage across origins, and a recorded chain the
//! user can inspect (redacted) when a signed link misbehaves.

use crate::util::redact::{redact, RedactedUrl};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RedirectError {
    TooMany(usize),
    /// https -> http. Refused by default: a downgrade exposes the request, and for a signed
    /// URL it exposes the credential in the query string.
    InsecureDowngrade,
    /// Anything that is not http(s): file:, data:, javascript: and friends.
    UnsupportedScheme(String),
    Malformed(String),
}

impl std::fmt::Display for RedirectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooMany(n) => write!(f, "redirect chain exceeded {n} hops"),
            Self::InsecureDowngrade => write!(f, "refused https to http redirect"),
            Self::UnsupportedScheme(s) => write!(f, "refused redirect to unsupported scheme {s:?}"),
            Self::Malformed(s) => write!(f, "malformed redirect target: {s}"),
        }
    }
}

/// One hop in a followed chain, recorded for diagnostics. Always redacted.
#[derive(Debug, Clone)]
pub struct Hop {
    pub url: RedactedUrl,
    pub status: u16,
}

/// Resolve the next URL for a redirect response, enforcing the safety rules.
pub fn next_url(
    current: &url::Url,
    location: &str,
    allow_downgrade: bool,
) -> Result<url::Url, RedirectError> {
    // Relative Locations are legal and common.
    let next = current
        .join(location)
        .map_err(|e| RedirectError::Malformed(format!("{location:?}: {e}")))?;

    match next.scheme() {
        "http" | "https" => {}
        other => return Err(RedirectError::UnsupportedScheme(other.to_string())),
    }

    if current.scheme() == "https" && next.scheme() == "http" && !allow_downgrade {
        return Err(RedirectError::InsecureDowngrade);
    }
    Ok(next)
}

/// Whether credentials must be stripped when moving between these URLs.
///
/// Origin is scheme + host + port. Forwarding an `Authorization` header or cookies to a
/// different origin hands the credential to whoever controls that host.
pub fn is_cross_origin(from: &url::Url, to: &url::Url) -> bool {
    from.scheme() != to.scheme()
        || from.host_str() != to.host_str()
        || from.port_or_known_default() != to.port_or_known_default()
}

pub fn record_hop(url: &url::Url, status: u16) -> Hop {
    Hop {
        url: redact(url.as_str()),
        status,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn follows_absolute_and_relative_targets() {
        let cur = u("https://a.example.com/dir/file");
        assert_eq!(
            next_url(&cur, "https://b.example.com/x", false)
                .unwrap()
                .as_str(),
            "https://b.example.com/x"
        );
        assert_eq!(
            next_url(&cur, "/other", false).unwrap().as_str(),
            "https://a.example.com/other"
        );
        assert_eq!(
            next_url(&cur, "sibling", false).unwrap().as_str(),
            "https://a.example.com/dir/sibling"
        );
    }

    #[test]
    fn refuses_protocol_downgrade_by_default() {
        let cur = u("https://secure.example.com/f");
        assert_eq!(
            next_url(&cur, "http://plain.example.com/f", false),
            Err(RedirectError::InsecureDowngrade)
        );
        // Allowed only when the user explicitly opts in.
        assert!(next_url(&cur, "http://plain.example.com/f", true).is_ok());
    }

    #[test]
    fn upgrade_is_always_fine() {
        let cur = u("http://plain.example.com/f");
        assert!(next_url(&cur, "https://secure.example.com/f", false).is_ok());
    }

    #[test]
    fn refuses_non_http_schemes() {
        let cur = u("https://example.com/f");
        for target in [
            "file:///etc/passwd",
            "data:text/plain,hi",
            "javascript:alert(1)",
            "ftp://x/y",
        ] {
            let got = next_url(&cur, target, false);
            assert!(
                matches!(got, Err(RedirectError::UnsupportedScheme(_))),
                "{target} was not refused: {got:?}"
            );
        }
    }

    #[test]
    fn detects_cross_origin_hops() {
        let a = u("https://a.example.com/f");
        assert!(
            is_cross_origin(&a, &u("https://b.example.com/f")),
            "different host"
        );
        assert!(
            is_cross_origin(&a, &u("http://a.example.com/f")),
            "different scheme"
        );
        assert!(
            is_cross_origin(&a, &u("https://a.example.com:8443/f")),
            "different port"
        );

        assert!(
            !is_cross_origin(&a, &u("https://a.example.com/other")),
            "same origin"
        );
        assert!(
            !is_cross_origin(&a, &u("https://a.example.com:443/f")),
            "explicit default port"
        );
    }

    #[test]
    fn recorded_hops_are_redacted() {
        let hop = record_hop(&u("https://cdn.example.com/f?token=SECRET"), 302);
        assert!(!hop.url.as_str().contains("SECRET"), "{}", hop.url);
        assert!(hop.url.as_str().contains("token="));
    }
}
