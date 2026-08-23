//! URL redaction.
//!
//! A signed download URL *is* a bearer credential — anyone holding it can fetch the object.
//! So the full URL lives only in the three `downloads` columns that must actually fetch with
//! it; everything else (URL history, the events ring, logs, diagnostics, benchmark output,
//! the UI's server tab) sees the redacted form.
//!
//! The policy is **default-deny**: every query parameter *value* is replaced, and only the
//! parameter *names* survive. Denylisting known token names (`X-Amz-Signature`, `sig`, …)
//! would work until the next provider invents a new one; names alone are plenty for
//! diagnostics.

use std::fmt;

const PLACEHOLDER: &str = "<redacted>";

/// A URL safe to persist in history, log, or display.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RedactedUrl(String);

impl RedactedUrl {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Strip every query-parameter value, the fragment, and any userinfo from a URL.
pub fn redact(raw: &str) -> RedactedUrl {
    let Ok(mut url) = url::Url::parse(raw) else {
        // Unparseable input is not a reason to leak it. Keep nothing but the shape.
        return RedactedUrl(format!("<unparseable url, {} chars>", raw.len()));
    };

    // Credentials in the authority (https://user:pass@host/...).
    let _ = url.set_username("");
    let _ = url.set_password(None);

    // Fragments are not sent to servers but can still carry tokens in some flows.
    url.set_fragment(None);

    let names: Vec<String> = url.query_pairs().map(|(k, _)| k.into_owned()).collect();
    if names.is_empty() {
        // Preserve "?" only if the original had a query string at all.
        if url.query().is_some() {
            url.set_query(Some(""));
        }
    } else {
        let mut q = url.query_pairs_mut();
        q.clear();
        for name in &names {
            q.append_pair(name, PLACEHOLDER);
        }
        drop(q);
    }

    RedactedUrl(url.to_string())
}

/// Host of a URL, for grouping and per-host budgets. Never carries secrets.
pub fn host_of(raw: &str) -> String {
    url::Url::parse(raw)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_else(|| "<unknown>".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(s: &str) -> String {
        redact(s).0
    }

    #[test]
    fn strips_all_query_values_but_keeps_names() {
        let got = r("https://ex.com/f.zip?token=SECRET&expires=12345");
        assert!(!got.contains("SECRET"), "{got}");
        assert!(!got.contains("12345"), "{got}");
        assert!(
            got.contains("token="),
            "parameter name should survive: {got}"
        );
        assert!(got.contains("expires="), "{got}");
        assert_eq!(
            got,
            "https://ex.com/f.zip?token=%3Credacted%3E&expires=%3Credacted%3E"
        );
    }

    #[test]
    fn redacts_s3_presigned_urls() {
        let s3 = "https://b.s3.amazonaws.com/k.bin?X-Amz-Algorithm=AWS4-HMAC-SHA256\
                  &X-Amz-Credential=AKIAIOSFODNN7EXAMPLE%2F20240101%2Fus-east-1%2Fs3%2Faws4_request\
                  &X-Amz-Signature=deadbeefcafe&X-Amz-SecurityToken=FQoGZ";
        let got = r(s3);
        for secret in [
            "AKIAIOSFODNN7EXAMPLE",
            "deadbeefcafe",
            "FQoGZ",
            "aws4_request",
        ] {
            assert!(!got.contains(secret), "leaked {secret} in {got}");
        }
        assert!(got.contains("X-Amz-Signature="));
    }

    #[test]
    fn redacts_azure_sas_and_gcs() {
        let azure =
            "https://a.blob.core.windows.net/c/b.zip?sv=2021-06-08&sr=b&sig=abc%2Bdef&se=2024";
        let got = r(azure);
        assert!(!got.contains("abc"), "{got}");
        assert!(!got.contains("2021-06-08"), "{got}");

        let gcs =
            "https://storage.googleapis.com/b/o?GoogleAccessId=x@y.iam&Expires=1&Signature=ZZZ";
        let got = r(gcs);
        assert!(!got.contains("ZZZ") && !got.contains("x@y.iam"), "{got}");
    }

    #[test]
    fn strips_userinfo_and_fragment() {
        let got = r("https://user:hunter2@ex.com/f.zip#frag-token");
        assert!(!got.contains("hunter2"), "{got}");
        assert!(!got.contains("user"), "{got}");
        assert!(!got.contains("frag-token"), "{got}");
    }

    #[test]
    fn urls_without_a_query_are_unchanged_in_substance() {
        assert_eq!(
            r("https://ex.com/path/file.zip"),
            "https://ex.com/path/file.zip"
        );
    }

    #[test]
    fn unparseable_input_is_not_echoed() {
        let got = r("not a url at all ?token=SECRET");
        assert!(!got.contains("SECRET"), "{got}");
    }

    #[test]
    fn host_extraction() {
        assert_eq!(host_of("https://cdn.example.com/a?b=c"), "cdn.example.com");
        assert_eq!(host_of("garbage"), "<unknown>");
    }

    proptest::proptest! {
        /// The load-bearing invariant: no query-parameter *value* may survive redaction.
        #[test]
        fn prop_no_query_value_ever_survives(
            host in "[a-z]{1,10}\\.(com|net|org)",
            path in "[a-zA-Z0-9/._-]{0,40}",
            keys in proptest::collection::vec("[a-zA-Z][a-zA-Z0-9_-]{0,12}", 0..6),
            vals in proptest::collection::vec("[a-zA-Z0-9]{8,24}", 0..6),
        ) {
            let n = keys.len().min(vals.len());
            let mut u = format!("https://{host}/{path}");
            if n > 0 {
                u.push('?');
                for i in 0..n {
                    if i > 0 { u.push('&'); }
                    u.push_str(&format!("{}={}", keys[i], vals[i]));
                }
            }
            let got = r(&u);
            for v in vals.iter().take(n) {
                // Values are generated long enough that incidental collision with the
                // host/path alphabet is not a realistic false positive.
                proptest::prop_assert!(!got.contains(v.as_str()), "leaked {} in {}", v, got);
            }
        }
    }
}
