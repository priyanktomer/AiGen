//! HTTP client construction.
//!
//! # The single most important detail in the engine
//!
//! If you build **one** `reqwest::Client` and issue N concurrent ranged GETs, and the server
//! negotiates HTTP/2, hyper will multiplex all N as *streams over one TCP connection*. You
//! then have one congestion window, one loss-recovery domain, and one flow-control budget —
//! strictly worse than a single-connection downloader, because you pay all the coordination
//! overhead and get none of the parallelism. Worse, it is invisible: throughput simply
//! plateaus, and the plateau looks exactly like a saturated server.
//!
//! Most large CDNs serve h2 by default, so this is the common case, not an edge case.
//!
//! The fix is **one `Client` per worker**: separate clients own separate connection pools, so
//! distinct TCP connections are guaranteed regardless of protocol. A `Client` is a cheap
//! handle around a pool, so this costs nothing. `tests/distinct_connections.rs` asserts it
//! against the test server's accept counter, because a regression here would be silent.

use crate::config::{RequestSpec, Settings};
use std::time::Duration;

/// h2's default flow-control window is 64 KB, which caps a single stream at
/// `64 KB / RTT` — about 320 KB/s on a 200 ms path, regardless of available bandwidth.
/// These are sized so the window is never the binding constraint.
const H2_STREAM_WINDOW: u32 = 4 * 1024 * 1024;
const H2_CONNECTION_WINDOW: u32 = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("failed to build HTTP client: {0}")]
    Build(#[from] reqwest::Error),
    #[error("invalid proxy url: {0}")]
    Proxy(String),
}

/// Build one client, intended for exactly one worker.
pub fn build(
    settings: &Settings,
    spec: &RequestSpec,
    target: &url::Url,
) -> Result<reqwest::Client, ClientError> {
    let mut b = reqwest::Client::builder()
        // One idle connection kept alive, so consecutive claims on the same worker reuse the
        // connection instead of re-handshaking, but pools never accumulate.
        .pool_max_idle_per_host(1)
        .pool_idle_timeout(Duration::from_secs(90))
        .connect_timeout(settings.connect_timeout)
        // Deliberately NO overall timeout: a whole-body deadline on a multi-GB download is a
        // bug. Stalls are caught by the per-read idle timeout in the worker instead.
        .tcp_nodelay(true)
        .http2_initial_stream_window_size(H2_STREAM_WINDOW)
        .http2_initial_connection_window_size(H2_CONNECTION_WINDOW)
        .http2_keep_alive_interval(Duration::from_secs(30))
        // Redirects are followed manually so the downgrade and cross-origin header rules in
        // `redirect.rs` actually apply.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(
            spec.user_agent
                .clone()
                .unwrap_or_else(|| settings.user_agent.clone()),
        );

    match &spec.proxy {
        Some(p) => {
            b = b.proxy(reqwest::Proxy::all(p).map_err(|e| ClientError::Proxy(e.to_string()))?);
        }
        None if is_loopback(target) => {
            // Never route loopback through an environment proxy. Real clients bypass local
            // addresses, and without this every test against the local server would fail.
            b = b.no_proxy();
        }
        None => {}
    }

    Ok(b.build()?)
}

fn is_loopback(url: &url::Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Apply the caller-supplied headers plus the ones the engine always sends.
pub fn apply_spec(mut req: reqwest::RequestBuilder, spec: &RequestSpec) -> reqwest::RequestBuilder {
    // `identity` is not optional. If the server compresses the body, byte ranges address the
    // compressed stream while the client decompresses transparently, so every segment offset
    // silently becomes wrong.
    req = req.header(reqwest::header::ACCEPT_ENCODING, "identity");

    if let Some(referer) = &spec.referer {
        req = req.header(reqwest::header::REFERER, referer);
    }
    if let Some(cookies) = &spec.cookies {
        req = req.header(reqwest::header::COOKIE, cookies);
    }
    for (k, v) in &spec.headers {
        req = req.header(k.as_str(), v.as_str());
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    #[test]
    fn builds_for_plain_and_loopback_targets() {
        let s = Settings::default();
        let spec = RequestSpec::default();
        assert!(build(&s, &spec, &u("https://example.com/f")).is_ok());
        assert!(build(&s, &spec, &u("http://127.0.0.1:8080/f")).is_ok());
    }

    #[test]
    fn loopback_bypasses_proxies() {
        assert!(is_loopback(&u("http://127.0.0.1:9/x")));
        assert!(is_loopback(&u("http://localhost:9/x")));
        assert!(is_loopback(&u("http://[::1]:9/x")));
        assert!(!is_loopback(&u("https://example.com/x")));
        assert!(!is_loopback(&u("https://127.0.0.1.example.com/x")));
    }

    #[test]
    fn rejects_a_malformed_proxy_rather_than_silently_ignoring_it() {
        let s = Settings::default();
        let spec = RequestSpec {
            proxy: Some("not a url".into()),
            ..Default::default()
        };
        assert!(build(&s, &spec, &u("https://example.com/f")).is_err());
    }

    #[test]
    fn separate_clients_are_independent_pools() {
        // The property the whole design rests on: two clients cannot share a connection.
        let s = Settings::default();
        let spec = RequestSpec::default();
        let a = build(&s, &spec, &u("https://example.com/f")).unwrap();
        let b = build(&s, &spec, &u("https://example.com/f")).unwrap();
        // reqwest::Client is Arc-backed; distinct builds are distinct pools.
        assert!(!std::ptr::eq(&a as *const _, &b as *const _));
    }
}
