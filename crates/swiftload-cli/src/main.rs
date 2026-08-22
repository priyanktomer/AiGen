//! Headless driver for the SwiftLoad engine.
//!
//! Session 1 has no UI, so this is how the engine is exercised by hand and in CI. It is not a
//! throwaway: the benchmark harness and the crash-recovery tests both drive the engine through
//! exactly this surface.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use swiftload_core::{
    config::Settings,
    task::{download, writer::NullSink, DownloadRequest},
    util::intervals::RangeSet,
};
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(name = "swiftload", version, about = "Segmented HTTP downloader with adaptive concurrency")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Download a URL.
    Get {
        url: String,
        /// Destination directory.
        #[arg(long, short, default_value = ".")]
        out: PathBuf,
        /// Connection count, or "auto" to let the governor decide.
        #[arg(long, default_value = "auto")]
        conns: String,
        /// Override the filename.
        #[arg(long)]
        name: Option<String>,
        /// Verify the finished file against this SHA-256.
        #[arg(long)]
        sha256: Option<String>,
        /// Emit one JSON line with the result instead of a progress display.
        #[arg(long)]
        json: bool,
    },
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

    match Cli::parse().cmd {
        Cmd::Get { url, out, conns, name, sha256, json } => {
            get(url, out, conns, name, sha256, json).await
        }
        Cmd::Inspect { url } => inspect(url).await,
        Cmd::Verify { path } => {
            println!("{}  {}", sha256_of(&path)?, path.display());
            Ok(())
        }
    }
}

async fn get(
    url: String,
    out: PathBuf,
    conns: String,
    name: Option<String>,
    sha256: Option<String>,
    json: bool,
) -> Result<()> {
    let settings = Settings::default();
    let max_conns = match conns.as_str() {
        "auto" => None,
        v => Some(v.parse().context("--conns must be a number or \"auto\"")?),
    };

    std::fs::create_dir_all(&out).with_context(|| format!("creating {}", out.display()))?;

    let req = DownloadRequest {
        url: url.clone(),
        dest_dir: out,
        filename: name,
        max_conns,
        spec: Default::default(),
        // Resume state lives in the store; the CLI starts from what is on disk.
        completed: RangeSet::new(),
        expected_sha256: sha256,
    };

    let cancel = CancellationToken::new();
    {
        // Ctrl-C pauses cleanly: the writer flushes and checkpoints, so the partial file stays
        // resumable rather than being abandoned mid-write.
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

    let started = std::time::Instant::now();
    let outcome = download(req, settings, Arc::new(NullSink), cancel, progress).await;
    if !json {
        eprintln!();
    }

    match outcome {
        Ok(o) => {
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "ok": true,
                        "path": o.path.display().to_string(),
                        "bytes": o.bytes,
                        "resumed_from": o.resumed_from,
                        "elapsed_ms": o.elapsed.as_millis(),
                        "avg_bps": o.avg_bps,
                        "peak_bps": o.peak_bps,
                        "peak_conns": o.peak_conns,
                        "retries": o.retries,
                        "requests": o.requests,
                        "sha256": o.sha256,
                    })
                );
            } else {
                println!("{}", o.path.display());
                println!(
                    "  {} in {:.1}s  avg {}/s  peak {}/s  peak {} conn(s)  {} retries",
                    human(o.bytes),
                    o.elapsed.as_secs_f64(),
                    human(o.avg_bps),
                    human(o.peak_bps),
                    o.peak_conns,
                    o.retries
                );
                if o.resumed_from > 0 {
                    println!("  resumed from {} — {} transferred", human(o.resumed_from), human(o.bytes - o.resumed_from));
                }
                if let Some(h) = &o.sha256 {
                    println!("  sha256 {h}");
                }
            }
            Ok(())
        }
        Err(e) => {
            if json {
                println!("{}", serde_json::json!({ "ok": false, "error": e.to_string() }));
                std::process::exit(1);
            }
            let _ = started;
            Err(e.into())
        }
    }
}

async fn inspect(url: String) -> Result<()> {
    let settings = Settings::default();
    let p = swiftload_core::task::probe::probe(&url, &settings, &Default::default(), false).await?;

    println!("url          {}", swiftload_core::util::redact::redact(p.final_url.as_str()));
    println!("filename     {}", p.filename);
    println!(
        "size         {}",
        p.total_size.map_or("unknown".into(), |s| format!("{} ({s} bytes)", human(s)))
    );
    println!("ranges       {:?}", p.range_support);
    println!("resumable    {}", p.is_resumable());
    println!("segmentable  {}", p.is_segmentable());
    println!("http         {}", p.http_version);
    if let Some(e) = &p.etag {
        println!("etag         {} ({})", e.raw(), if e.is_strong() { "strong" } else { "weak" });
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

fn sha256_of(path: &PathBuf) -> Result<String> {
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
    if i == 0 { format!("{bytes} B") } else { format!("{v:.1} {}", UNITS[i]) }
}

#[allow(dead_code)]
fn unused(_: Duration) {}
