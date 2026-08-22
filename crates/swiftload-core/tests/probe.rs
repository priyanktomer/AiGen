//! Probe behaviour against the adversarial server.
//!
//! These are the cases that decide whether the rest of the engine is even allowed to
//! segment a download, so each one is checked against a server that misbehaves on purpose.

use swiftload_core::{
    config::{RequestSpec, Settings},
    task::probe::{probe, RangeSupport},
};
use swiftload_testserver as ts;

fn settings() -> Settings {
    Settings::default()
}

#[tokio::test]
async fn detects_range_support_and_total_size_in_one_request() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = probe(&h.url("/plain/alpha/104857600"), &settings(), &RequestSpec::default(), false)
        .await
        .unwrap();

    assert_eq!(r.range_support, RangeSupport::Supported);
    assert_eq!(r.total_size, Some(104_857_600));
    assert!(r.is_resumable());
    assert!(r.is_segmentable());
    assert!(r.etag.is_some());
    assert!(r.last_modified.is_some());

    // One byte of body, not 100 MB: the probe must not drag the whole file down.
    assert!(h.stats().bytes_served <= 1024, "probe pulled {} bytes", h.stats().bytes_served);
}

#[tokio::test]
async fn a_server_without_range_support_is_downloadable_but_not_resumable() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = probe(&h.url("/norange/alpha/4096"), &settings(), &RequestSpec::default(), false)
        .await
        .unwrap();

    assert_eq!(r.range_support, RangeSupport::Unsupported);
    assert!(!r.is_resumable());
    // Content-Length is still the real size, because the server sent the whole body.
    assert_eq!(r.total_size, Some(4096));
}

#[tokio::test]
async fn missing_content_length_yields_unknown_size() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = probe(&h.url("/nolen/alpha/4096"), &settings(), &RequestSpec::default(), false)
        .await
        .unwrap();

    assert_eq!(r.total_size, None, "no Content-Length means no size, not a guess");
    assert!(!r.is_segmentable(), "cannot plan segments without a size");
}

#[tokio::test]
async fn a_transforming_content_encoding_disables_segmentation() {
    // The silent-corruption case: byte ranges would address the compressed stream while the
    // client decompresses transparently, so every segment offset would be wrong.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = probe(&h.url("/gzip/alpha/104857600"), &settings(), &RequestSpec::default(), false)
        .await
        .unwrap();

    assert!(r.transforming_encoding, "server applied gzip despite our identity request");
    assert_eq!(r.range_support, RangeSupport::Unsupported, "must refuse to segment");
    assert!(!r.is_resumable());
    assert!(!r.is_segmentable());
}

#[tokio::test]
async fn follows_redirect_chains_and_records_them() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = probe(&h.url("/redirect/3/alpha/8192"), &settings(), &RequestSpec::default(), false)
        .await
        .unwrap();

    assert_eq!(r.total_size, Some(8192));
    assert_eq!(r.redirect_chain.len(), 3);
    assert!(r.final_url.path().contains("/plain/"), "final url {}", r.final_url);
}

#[tokio::test]
async fn refuses_an_overlong_redirect_chain() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let mut s = settings();
    s.max_redirects = 2;
    let err = probe(&h.url("/redirect/9/alpha/8192"), &s, &RequestSpec::default(), false)
        .await
        .unwrap_err();
    assert!(matches!(err, swiftload_core::task::probe::ProbeError::Redirect(_)), "{err}");
}

#[tokio::test]
async fn resolves_the_filename_from_the_url() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    // The last path segment is the size, so this exercises the fallback chain honestly.
    let r = probe(&h.url("/plain/alpha/4096"), &settings(), &RequestSpec::default(), false)
        .await
        .unwrap();
    assert_eq!(r.filename, "4096");
}

#[tokio::test]
async fn honours_content_disposition_for_the_filename() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = probe(
        &h.url("/plain/alpha/4096?name=Report%20Final.pdf"),
        &settings(),
        &RequestSpec::default(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(r.filename, "Report Final.pdf");
}

#[tokio::test]
async fn classifies_terminal_and_transient_statuses() {
    use swiftload_core::http::errors::ErrorClass;
    use swiftload_core::task::probe::ProbeError;

    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();

    let err = probe(&h.url("/status/404"), &settings(), &RequestSpec::default(), false).await.unwrap_err();
    assert!(matches!(&err, ProbeError::Status { class: ErrorClass::Fatal, status: 404 }), "{err}");

    let err = probe(&h.url("/status/429?n=3"), &settings(), &RequestSpec::default(), false).await.unwrap_err();
    match err {
        ProbeError::Status { class: ErrorClass::RateLimited { retry_after }, .. } => {
            assert_eq!(retry_after, Some(std::time::Duration::from_secs(3)), "Retry-After must be honoured");
        }
        other => panic!("expected rate limiting, got {other}"),
    }

    let err = probe(&h.url("/status/503"), &settings(), &RequestSpec::default(), false).await.unwrap_err();
    assert!(err.class().is_retryable());
}

#[tokio::test]
async fn forbidden_is_expiry_only_when_the_link_previously_worked() {
    use swiftload_core::http::errors::ErrorClass;
    use swiftload_core::task::probe::ProbeError;

    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();

    // First attempt: we simply never had access.
    let err = probe(&h.url("/status/403"), &settings(), &RequestSpec::default(), false).await.unwrap_err();
    assert!(matches!(&err, ProbeError::Status { class: ErrorClass::Fatal, .. }), "{err}");

    // Same status on a link that used to work: the signature expired, so a fresh link fixes it.
    let err = probe(&h.url("/status/403"), &settings(), &RequestSpec::default(), true).await.unwrap_err();
    assert!(matches!(&err, ProbeError::Status { class: ErrorClass::UrlExpired, .. }), "{err}");
}

#[tokio::test]
async fn signed_urls_probe_before_and_fail_after_expiry() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = reqwest::Client::builder().no_proxy().build().unwrap();

    let url = c.get(h.url("/mint/alpha?n=1048576")).send().await.unwrap().text().await.unwrap();
    let r = probe(&url, &settings(), &RequestSpec::default(), false).await.unwrap();
    assert_eq!(r.total_size, Some(1_048_576));

    c.get(h.url("/expire/alpha")).send().await.unwrap();
    let err = probe(&url, &settings(), &RequestSpec::default(), true).await.unwrap_err();
    assert!(
        matches!(err.class(), swiftload_core::http::errors::ErrorClass::UrlExpired),
        "expired link should classify as expiry, got {err}"
    );
}
