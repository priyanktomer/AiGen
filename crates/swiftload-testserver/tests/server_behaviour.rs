//! The test server is test infrastructure, so it needs tests of its own: if `per=conn` did
//! not actually throttle per connection, every concurrency conclusion drawn from it later
//! would be junk.

use std::time::Instant;
use swiftload_testserver as ts;

fn client() -> reqwest::Client {
    // no_proxy matters: this environment has an HTTPS proxy configured, and loopback traffic
    // must not be routed through it.
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
async fn serves_deterministic_content_and_honours_ranges() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();

    let whole = c.get(h.url("/plain/alpha/4096")).send().await.unwrap();
    assert_eq!(whole.status(), 200);
    assert_eq!(whole.headers()["accept-ranges"], "bytes");
    let body = whole.bytes().await.unwrap();
    assert_eq!(body.len(), 4096);
    assert_eq!(&body[..], &ts::content::chunk(ts::content::seed_of("alpha"), 0, 4096)[..]);

    let part = c
        .get(h.url("/plain/alpha/4096"))
        .header("Range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status(), 206);
    assert_eq!(part.headers()["content-range"], "bytes 1000-1999/4096");
    let pb = part.bytes().await.unwrap();
    assert_eq!(pb.len(), 1000);
    assert_eq!(&pb[..], &ts::content::chunk(ts::content::seed_of("alpha"), 1000, 1000)[..]);
}

#[tokio::test]
async fn probe_range_returns_total_size() {
    // The 1-byte ranged GET the engine uses in place of HEAD.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = client()
        .get(h.url("/plain/alpha/1048576"))
        .header("Range", "bytes=0-0")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(r.headers()["content-range"], "bytes 0-0/1048576");
}

#[tokio::test]
async fn liar_advertises_ranges_then_ignores_them() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = client()
        .get(h.url("/liar/alpha/4096"))
        .header("Range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    assert_eq!(r.headers()["accept-ranges"], "bytes", "advertises support");
    // ...but serves the whole file with 200. A client that trusts the advertisement and
    // writes this body at offset 1000 silently corrupts the file.
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().len(), 4096);
}

#[tokio::test]
async fn norange_and_nolen_modes() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();

    let r = c.get(h.url("/norange/alpha/4096")).header("Range", "bytes=0-99").send().await.unwrap();
    assert_eq!(r.headers()["accept-ranges"], "none");
    assert_eq!(r.status(), 200);

    let r = c.get(h.url("/nolen/alpha/4096")).send().await.unwrap();
    assert!(r.headers().get("content-length").is_none(), "must not advertise a length");
    assert_eq!(r.bytes().await.unwrap().len(), 4096);
}

#[tokio::test]
async fn rotate_etag_changes_while_content_stays_identical() {
    // The case that must NOT be read as proof of a different file.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();

    let a = c.get(h.url("/rotate-etag/alpha/2048")).send().await.unwrap();
    let etag_a = a.headers()["etag"].to_str().unwrap().to_string();
    let body_a = a.bytes().await.unwrap();

    let b = c.get(h.url("/rotate-etag/alpha/2048")).send().await.unwrap();
    let etag_b = b.headers()["etag"].to_str().unwrap().to_string();
    let body_b = b.bytes().await.unwrap();

    assert_ne!(etag_a, etag_b, "ETag should rotate");
    assert_eq!(body_a, body_b, "content must be identical despite the rotation");
}

#[tokio::test]
async fn decoy_matches_size_but_not_content() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();
    let real = c.get(h.url("/plain/real/4096")).send().await.unwrap().bytes().await.unwrap();
    let decoy = c.get(h.url("/decoy/fake/4096")).send().await.unwrap().bytes().await.unwrap();
    assert_eq!(real.len(), decoy.len(), "same size — headers alone cannot separate them");
    assert_ne!(real, decoy, "different bytes — only content comparison catches this");
}

#[tokio::test]
async fn signed_urls_expire_and_can_be_reminted() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();

    let url = c.get(h.url("/mint/alpha?n=4096")).send().await.unwrap().text().await.unwrap();
    assert_eq!(c.get(&url).send().await.unwrap().status(), 200, "freshly minted link works");

    c.get(h.url("/expire/alpha")).send().await.unwrap();
    assert_eq!(c.get(&url).send().await.unwrap().status(), 403, "expired link is refused");

    let fresh = c.get(h.url("/mint/alpha?n=4096")).send().await.unwrap().text().await.unwrap();
    assert_ne!(fresh, url, "a new link must carry a different token");
    assert_eq!(c.get(&fresh).send().await.unwrap().status(), 200);
}

#[tokio::test]
async fn per_connection_throttling_scales_with_concurrency() {
    // The load-bearing property for the whole benchmark story: under a per-connection cap,
    // N connections really do deliver about N times the throughput.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();
    let size = 2_000_000u64;
    let bps = 2_000_000u64;

    let t = Instant::now();
    c.get(h.url(&format!("/throttle/alpha/{size}?bps={bps}&per=conn")))
        .send().await.unwrap().bytes().await.unwrap();
    let single = t.elapsed();

    let t = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for i in 0..4u64 {
        let c = c.clone();
        let u = h.url(&format!("/throttle/alpha/{size}?bps={bps}&per=conn"));
        let (s, e) = (i * size / 4, (i + 1) * size / 4 - 1);
        set.spawn(async move {
            c.get(u).header("Range", format!("bytes={s}-{e}")).send().await.unwrap()
                .bytes().await.unwrap().len()
        });
    }
    let mut got = 0;
    while let Some(r) = set.join_next().await {
        got += r.unwrap();
    }
    let parallel = t.elapsed();

    assert_eq!(got as u64, size);
    assert!(parallel < single / 2, "per=conn should scale: 1 conn {single:?} vs 4 conns {parallel:?}");
}

#[tokio::test]
async fn total_throttling_does_not_scale_with_concurrency() {
    // The mirror image: a shared cap means extra connections buy nothing. A downloader that
    // keeps adding connections here is burning resources and server capacity for zero gain.
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();
    let size = 2_000_000u64;
    let bps = 2_000_000u64;

    let t = Instant::now();
    c.get(h.url(&format!("/throttle/alpha/{size}?bps={bps}&per=total")))
        .send().await.unwrap().bytes().await.unwrap();
    let single = t.elapsed();

    let t = Instant::now();
    let mut set = tokio::task::JoinSet::new();
    for i in 0..4u64 {
        let c = c.clone();
        let u = h.url(&format!("/throttle/alpha/{size}?bps={bps}&per=total"));
        let (s, e) = (i * size / 4, (i + 1) * size / 4 - 1);
        set.spawn(async move {
            c.get(u).header("Range", format!("bytes={s}-{e}")).send().await.unwrap()
                .bytes().await.unwrap().len()
        });
    }
    while set.join_next().await.is_some() {}
    let parallel = t.elapsed();

    assert!(parallel > single / 2, "per=total must not scale: 1 conn {single:?} vs 4 conns {parallel:?}");
}

#[tokio::test]
async fn injected_reset_breaks_the_stream_mid_body() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();

    // A truncated body surfaces as hyper's IncompleteMessage, but *where* it surfaces depends
    // on timing: if the abort beats the client's first body poll it fails at send(), otherwise
    // partway through the stream. A correct client has to handle both, so the assertion is on
    // the operation as a whole rather than on one stage of it.
    let outcome: Result<usize, String> =
        match client().get(h.url("/plain/alpha/10000000?reset_at=131072")).send().await {
            Err(e) => Err(format!("send: {e}")),
            Ok(r) => {
                assert_eq!(r.status(), 200);
                r.bytes().await.map(|b| b.len()).map_err(|e| format!("body: {e}"))
            }
        };

    assert!(outcome.is_err(), "a reset stream must not read as a complete body: {outcome:?}");
    assert_eq!(h.stats().resets_injected, 1);
    assert!(h.stats().bytes_served < 10_000_000, "server should have stopped early");
}

#[tokio::test]
async fn redirect_chains_terminate_at_content() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let r = client().get(h.url("/redirect/3/alpha/2048")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.bytes().await.unwrap().len(), 2048);
}

#[tokio::test]
async fn status_route_sets_retry_after() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();
    let r = c.get(h.url("/status/429?n=7")).send().await.unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "7");
    assert_eq!(c.get(h.url("/status/404")).send().await.unwrap().status(), 404);
}

#[tokio::test]
async fn maxconn_refuses_excess_concurrency() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let c = c.clone();
        let u = h.url("/maxconn/alpha/2000000?n=3&bps=500000&per=conn");
        set.spawn(async move { c.get(u).send().await.unwrap().status().as_u16() });
    }
    let mut too_many = 0;
    while let Some(r) = set.join_next().await {
        if r.unwrap() == 429 {
            too_many += 1;
        }
    }
    assert!(too_many > 0, "server should have refused some of 12 concurrent streams");
}

#[tokio::test]
async fn stats_counts_accepts_and_bytes() {
    let h = ts::spawn("127.0.0.1:0".parse().unwrap()).await.unwrap();
    let c = client();
    for _ in 0..3 {
        c.get(h.url("/plain/alpha/1024")).send().await.unwrap().bytes().await.unwrap();
    }
    let s = h.stats();
    assert!(s.accepts >= 1, "accepts not counted");
    assert_eq!(s.bytes_served, 3072);
    assert_eq!(s.requests, 3);
}
