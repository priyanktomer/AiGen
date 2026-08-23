//! Headless driver for the SwiftLoad engine.
//!
//! Session 1 has no UI, so this is how the engine is exercised by hand and in CI. It is not a
//! throwaway: it drives the same `Manager`-level surface the desktop shell will, and the
//! crash-recovery and URL-refresh tests both go through it.

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use swiftload_core::{
    config::{RequestSpec, Settings},
    http::headers::Validator,
    store::{models::*, Store, StoreSink},
    task::{
        download,
        identity::{validate_replacement_url, ResourceSignals, Verdict},
        probe::{probe, RangeSupport},
        DownloadError, DownloadRequest, HostHint,
    },
    util::redact,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "swiftload",
    version,
    about = "Segmented HTTP downloader with adaptive concurrency"
)]
struct Cli {
    /// State database. Downloads recorded here survive process restarts.
    #[arg(long, global = true, default_value = "swiftload.db")]
    db: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download a URL.
    Get {
        url: String,
        #[arg(long, short, default_value = ".")]
        out: PathBuf,
        /// Connection count, or "auto" to let the governor decide.
        #[arg(long, default_value = "auto")]
        conns: String,
        #[arg(long)]
        name: Option<String>,
        /// Verify the finished file against this SHA-256.
        #[arg(long)]
        sha256: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Resume an interrupted download by id.
    Resume {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Point an existing download at a replacement URL, keeping its progress.
    ///
    /// Validates that the new link serves the same file before swapping. If identity is
    /// plausible but not proven, the swap is refused unless --yes is given.
    RefreshUrl {
        id: String,
        url: String,
        /// Accept a replacement whose identity was confirmed by content but not by headers.
        #[arg(long)]
        yes: bool,
        /// Validate and report without changing anything.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// List recorded downloads.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show the redacted URL chain for a download.
    Links { id: String },
    /// Report what a server says about a URL, without downloading it.
    Inspect { url: String },
    /// Print the SHA-256 of a local file.
    Verify { path: PathBuf },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Get {
            url,
            out,
            conns,
            name,
            sha256,
            json,
        } => {
            let store = open_store(&cli.db)?;
            cmd_get(store, url, out, conns, name, sha256, json).await
        }
        Cmd::Resume { id, json } => {
            let store = open_store(&cli.db)?;
            cmd_resume(store, id, json).await
        }
        Cmd::RefreshUrl {
            id,
            url,
            yes,
            dry_run,
            json,
        } => {
            let store = open_store(&cli.db)?;
            cmd_refresh(store, id, url, yes, dry_run, json).await
        }
        Cmd::List { json } => cmd_list(open_store(&cli.db)?, json),
        Cmd::Links { id } => cmd_links(open_store(&cli.db)?, id),
        Cmd::Inspect { url } => cmd_inspect(url).await,
        Cmd::Verify { path } => {
            println!("{}  {}", sha256_of(&path)?, path.display());
            Ok(())
        }
    }
}

fn open_store(path: &Path) -> Result<Arc<Store>> {
    let store = Store::open(path).with_context(|| format!("opening {}", path.display()))?;
    // Any download still flagged clean is from a previous run; clearing them now means a crash
    // during *this* run is still detected as unclean.
    store.clear_clean_flags()?;
    Ok(Arc::new(store))
}

async fn cmd_get(
    store: Arc<Store>,
    url: String,
    out: PathBuf,
    conns: String,
    name: Option<String>,
    sha256: Option<String>,
    json: bool,
) -> Result<()> {
    let settings = Settings::default();
    let max_conns = parse_conns(&conns)?;
    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    // Probe first so the record carries a real filename and size before any bytes move.
    let pr = probe(&url, &settings, &RequestSpec::default(), false).await?;
    let filename = name.clone().unwrap_or_else(|| pr.filename.clone());

    // Offer an existing partial rather than silently starting a second copy.
    let hint = pr.identity_hint();
    for existing in store.find_resumable_by_hint(&hint)? {
        if existing.bytes_done > 0 {
            eprintln!(
                "note: an incomplete download of {} already exists ({} of {}).\n      \
                 resume it with:  swiftload resume {}",
                existing.filename,
                human(existing.bytes_done),
                existing.total_size.map_or("?".into(), human),
                existing.id
            );
        }
    }

    let part = out.join(format!("{filename}.slpart"));
    let mut rec = DownloadRecord::new(
        &url,
        &filename,
        &out.to_string_lossy(),
        &part.to_string_lossy(),
    );
    rec.total_size = pr.total_size;
    rec.identity_hint = hint;
    rec.etag = pr.etag.as_ref().map(|e| e.as_header());
    rec.last_modified = pr.last_modified.clone();
    rec.content_type = pr.content_type.clone();
    rec.http_version = Some(pr.http_version.clone());
    rec.accept_ranges = range_support_code(pr.range_support);
    rec.max_connections = max_conns;
    rec.final_url = pr.final_url.to_string();
    store.insert(&rec)?;

    run(store, rec, settings, Default::default(), sha256, json).await
}

async fn cmd_resume(store: Arc<Store>, id: String, json: bool) -> Result<()> {
    let rec = store
        .get(&id)?
        .with_context(|| format!("no download with id {id}"))?;
    if rec.status == DownloadStatus::Completed {
        println!("already complete: {}", rec.filename);
        return Ok(());
    }
    let settings = Settings::default();

    // An unclean shutdown gives back a margin of every span, in case fsync lied.
    let (ranges, unclean) = store.resume_state(&id, settings.paranoid_recovery)?;
    if unclean && !json {
        eprintln!(
            "note: previous run did not shut down cleanly — re-fetching {} to be safe",
            human(rec.completed_ranges.total() - ranges.total())
        );
    }
    let mut rec = rec;
    rec.completed_ranges = ranges;
    run(store, rec, settings, Default::default(), None, json).await
}

async fn cmd_refresh(
    store: Arc<Store>,
    id: String,
    new_url: String,
    yes: bool,
    dry_run: bool,
    json: bool,
) -> Result<()> {
    let rec = store
        .get(&id)?
        .with_context(|| format!("no download with id {id}"))?;
    let settings = Settings::default();

    let old = ResourceSignals {
        total_size: rec.total_size,
        etag: rec.etag.as_deref().and_then(Validator::parse),
        last_modified: rec.last_modified.clone(),
        content_type: rec.content_type.clone(),
        filename: Some(rec.filename.clone()),
        range_support: code_to_range_support(rec.accept_ranges),
    };

    let (ranges, _) = store.resume_state(&id, settings.paranoid_recovery)?;
    let report = validate_replacement_url(
        &new_url,
        &old,
        std::path::Path::new(&rec.part_path),
        &ranges,
        &id,
        &settings,
        &RequestSpec::default(),
    )
    .await?;

    if json {
        println!(
            "{}",
            serde_json::json!({
                "verdict": format!("{:?}", report.verdict),
                "safe": report.verdict.is_safe_to_resume(),
                "needs_user": report.verdict.needs_user(),
                "windows_checked": report.windows_checked,
                "bytes_verified": report.bytes_verified,
                "preserved_bytes": ranges.total(),
            })
        );
    } else {
        println!(
            "checked {} sample section(s), {} of traffic",
            report.windows_checked,
            human(report.bytes_verified)
        );
        match &report.verdict {
            Verdict::Resume(ev) => println!("✓ {}", ev.user_summary()),
            Verdict::Confirm(ev) => println!("? {}", ev.user_summary()),
            Verdict::Reject(r) => println!("✗ {}", r.user_message()),
            Verdict::RestartOnly => println!(
                "! This link does not support resuming. Using it means downloading all {} again.",
                rec.total_size.map_or("of it".into(), human)
            ),
        }
        println!(
            "  {} already downloaded would be kept",
            human(ranges.total())
        );
    }

    match &report.verdict {
        Verdict::Reject(r) => bail!("{}", r.user_message()),
        Verdict::RestartOnly => {
            bail!("this link cannot resume; the partial file has been left in place")
        }
        Verdict::Confirm(_) if !yes => {
            bail!("identity confirmed by content but not by the server's own tags — pass --yes to resume")
        }
        _ => {}
    }
    if dry_run {
        return Ok(());
    }

    let validation = match &report.verdict {
        Verdict::Resume(ev) if ev.etag_matches => ValidationState::AutoVerified,
        Verdict::Resume(_) => ValidationState::ContentVerified,
        _ => ValidationState::UserConfirmed,
    };
    store.swap_url(
        &id,
        &new_url,
        &report.resolved_url,
        report
            .new_signals
            .etag
            .as_ref()
            .map(|e| e.as_header())
            .as_deref(),
        report.new_signals.last_modified.as_deref(),
        report.new_signals.total_size,
        range_support_code(report.new_signals.range_support),
        validation,
        if yes {
            "accepted_confirmed"
        } else {
            "accepted_auto"
        },
    )?;

    let mut rec = store.get(&id)?.unwrap();
    rec.completed_ranges = ranges;
    run(store, rec, settings, Default::default(), None, json).await
}

/// Shared execution path for get / resume / refresh.
async fn run(
    store: Arc<Store>,
    rec: DownloadRecord,
    settings: Settings,
    spec: RequestSpec,
    expected_sha256: Option<String>,
    json: bool,
) -> Result<()> {
    let id = rec.id.clone();
    store.set_status(&id, DownloadStatus::Active, None)?;

    // What a previous download from this host settled on, so we can skip re-discovering it.
    let host = redact::host_of(&rec.current_url);
    let host_hint = store.host_profile(&host)?.map(|p| HostHint {
        best_conns: p.best_observed_conns,
        saturated: p.saturation_detected,
    });

    let req = DownloadRequest {
        url: rec.current_url.clone(),
        dest_dir: PathBuf::from(&rec.dest_dir),
        filename: Some(rec.filename.clone()),
        max_conns: rec.max_connections,
        spec,
        completed: rec.completed_ranges.clone(),
        expected_sha256,
        host_hint,
    };

    let cancel = CancellationToken::new();
    {
        // Ctrl-C is a clean pause: the writer flushes and checkpoints so the partial stays
        // resumable, rather than being abandoned mid-write.
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("\ninterrupted — flushing so the download can resume");
                cancel.cancel();
            }
        });
    }

    let last_len = Arc::new(AtomicU64::new(0));
    let progress: Option<Box<dyn Fn(swiftload_core::task::Progress) + Send + Sync>> = if json {
        None
    } else {
        let last_len = last_len.clone();
        Some(Box::new(move |p| {
            let line = render(&p);
            let pad = (last_len.load(Ordering::Relaxed) as usize).saturating_sub(line.len());
            eprint!("\r{line}{}", " ".repeat(pad));
            last_len.store(line.len() as u64, Ordering::Relaxed);
        }))
    };

    let sink = Arc::new(StoreSink {
        store: store.clone(),
        id: id.clone(),
    });
    let outcome = download(req, settings, sink, cancel, progress).await;
    if !json {
        eprintln!();
    }

    match outcome {
        Ok(o) => {
            store.set_status(&id, DownloadStatus::Completed, None)?;
            store.mark_clean(&id, true)?;
            // Remember what worked here. Only when the governor was actually in charge — a
            // user-pinned connection count says nothing about what the server would allow.
            if rec.max_connections.is_none() {
                let _ = store.record_host_profile(
                    &host,
                    o.settled_conns,
                    o.avg_bps,
                    o.saturation_detected,
                );
            }
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true, "id": id,
                        "path": o.path.display().to_string(),
                        "bytes": o.bytes, "resumed_from": o.resumed_from,
                        "elapsed_ms": o.elapsed.as_millis(),
                        "avg_bps": o.avg_bps, "peak_bps": o.peak_bps,
                        "peak_conns": o.peak_conns, "retries": o.retries,
                        "requests": o.requests, "sha256": o.sha256,
                    })
                );
            } else {
                println!("{}", o.path.display());
                println!(
                    "  {} in {:.1}s  avg {}/s  peak {}/s  up to {} conn(s)  {} retries",
                    human(o.bytes),
                    o.elapsed.as_secs_f64(),
                    human(o.avg_bps),
                    human(o.peak_bps),
                    o.peak_conns,
                    o.retries
                );
                if o.resumed_from > 0 {
                    println!(
                        "  resumed from {} — only {} transferred",
                        human(o.resumed_from),
                        human(o.bytes - o.resumed_from)
                    );
                }
                if let Some(h) = &o.sha256 {
                    println!("  sha256 {h}");
                }
                println!("  id {id}");
            }
            Ok(())
        }
        Err(e) => {
            let status = match &e {
                DownloadError::Cancelled => DownloadStatus::Paused,
                DownloadError::Failed(swiftload_core::http::errors::ErrorClass::UrlExpired) => {
                    DownloadStatus::NeedsAttention
                }
                _ => DownloadStatus::Failed,
            };
            store.set_status(&id, status, Some(&e.to_string()))?;
            // A clean stop, even on failure: the partial stays exactly as recorded.
            store.mark_clean(&id, true)?;

            if json {
                println!(
                    "{}",
                    serde_json::json!({ "ok": false, "id": id, "error": e.to_string() })
                );
                std::process::exit(1);
            }
            if status == DownloadStatus::NeedsAttention {
                eprintln!(
                    "The download link has expired. {} is already downloaded.\n\
                     Provide a fresh link with:  swiftload refresh-url {id} <new-url>",
                    human(store.get(&id)?.map(|r| r.bytes_done).unwrap_or(0))
                );
            }
            Err(e.into())
        }
    }
}

fn cmd_list(store: Arc<Store>, json: bool) -> Result<()> {
    let all = store.list(None)?;
    if json {
        let rows: Vec<_> = all
            .iter()
            .map(|d| {
                serde_json::json!({
                    "id": d.id, "filename": d.filename, "status": d.status.as_str(),
                    "bytes_done": d.bytes_done, "total_size": d.total_size,
                    "url_refresh_count": d.url_refresh_count,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&rows)?);
        return Ok(());
    }
    if all.is_empty() {
        println!("no downloads recorded");
        return Ok(());
    }
    for d in all {
        let pct = d.percent().map_or("   ?".into(), |p| format!("{p:5.1}%"));
        println!(
            "{}  {pct}  {:>10} / {:<10}  {:<15}  {}",
            &d.id[..8],
            human(d.bytes_done),
            d.total_size.map_or("?".into(), human),
            d.status.as_str(),
            d.filename
        );
    }
    Ok(())
}

fn cmd_links(store: Arc<Store>, id: String) -> Result<()> {
    for e in store.url_history(&id)? {
        println!(
            "{:>2}  {:<18} {:<20} kept {:>10}  {}",
            e.seq,
            e.source,
            e.outcome,
            human(e.bytes_done_at_swap),
            e.url_redacted
        );
    }
    Ok(())
}

async fn cmd_inspect(url: String) -> Result<()> {
    let settings = Settings::default();
    let p = probe(&url, &settings, &RequestSpec::default(), false).await?;

    println!("url          {}", redact::redact(p.final_url.as_str()));
    println!("filename     {}", p.filename);
    println!(
        "size         {}",
        p.total_size
            .map_or("unknown".into(), |s| format!("{} ({s} bytes)", human(s)))
    );
    println!("ranges       {:?}", p.range_support);
    println!("resumable    {}", p.is_resumable());
    println!("segmentable  {}", p.is_segmentable());
    println!("http         {}", p.http_version);
    if let Some(e) = &p.etag {
        println!(
            "etag         {} ({})",
            e.raw(),
            if e.is_strong() { "strong" } else { "weak" }
        );
    }
    if let Some(lm) = &p.last_modified {
        println!("modified     {lm}");
    }
    if let Some(ct) = &p.content_type {
        println!("type         {ct}");
    }
    if p.transforming_encoding {
        println!("encoding     transformed — segmentation disabled to avoid corrupt offsets");
    }
    for (i, hop) in p.redirect_chain.iter().enumerate() {
        println!("redirect {i}   {} -> {}", hop.status, hop.url);
    }
    Ok(())
}

fn parse_conns(s: &str) -> Result<Option<usize>> {
    match s {
        "auto" => Ok(None),
        v => Ok(Some(
            v.parse().context("--conns must be a number or \"auto\"")?,
        )),
    }
}

fn range_support_code(r: RangeSupport) -> i64 {
    match r {
        RangeSupport::Unknown => 0,
        RangeSupport::Supported => 1,
        RangeSupport::Unsupported => 2,
        RangeSupport::Lied => 3,
    }
}

fn code_to_range_support(c: i64) -> RangeSupport {
    match c {
        1 => RangeSupport::Supported,
        2 => RangeSupport::Unsupported,
        3 => RangeSupport::Lied,
        _ => RangeSupport::Unknown,
    }
}

fn render(p: &swiftload_core::task::Progress) -> String {
    let pct = match p.total {
        Some(t) if t > 0 => format!("{:5.1}%", p.bytes_done as f64 / t as f64 * 100.0),
        _ => "     ".into(),
    };
    let of = match p.total {
        Some(t) => format!("{} / {}", human(p.bytes_done), human(t)),
        None => human(p.bytes_done),
    };
    let eta = p.eta.map_or("--:--".into(), |e| {
        let s = e.as_secs();
        format!("{:02}:{:02}", s / 60, s % 60)
    });
    format!(
        "{pct}  {of}  {}/s  {} conn  ETA {eta}",
        human(p.current_bps),
        p.conns
    )
}

fn sha256_of(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        match f.read(&mut buf)? {
            0 => break,
            n => h.update(&buf[..n]),
        }
    }
    Ok(format!("{:x}", h.finalize()))
}

fn human(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0;
    while v >= 1024.0 && i < UNITS.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[i])
    }
}
