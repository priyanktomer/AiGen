//! Benchmark harness.
//!
//! The question this exists to answer is narrow and falsifiable:
//!
//! > Is segmented downloading actually faster than a single stream, and under what conditions?
//!
//! Run against an arbitrary public URL the answer is unreproducible — you are mostly measuring
//! the tester's link and the time of day. So the default environment is the local test server,
//! where the shaping is **ground truth**: `per=conn` caps each connection (parallelism should
//! win near-linearly) and `per=total` caps them collectively (parallelism should win nothing).
//! Those two scenarios are the whole thesis, and they are the ones a reader can re-run.
//!
//! Two rules make the numbers mean something:
//!
//! * **Treatments are interleaved, not blocked.** Running all the 1-connection reps and then
//!   all the 8-connection reps conflates the treatment with whatever else the machine was
//!   doing. Each repetition runs every treatment, in order.
//! * **Medians, with p10/p90.** A mean over five runs is dominated by whichever run hit a
//!   scheduler hiccup.

mod sysmetrics;

use anyhow::Result;
use clap::Parser;
use serde::Serialize;
use std::{sync::Arc, time::Instant};
use swiftload_core::{
    config::Settings,
    task::{download, writer::NullSink, DownloadRequest},
};
use swiftload_testserver as ts;
use tokio_util::sync::CancellationToken;

#[derive(Parser)]
#[command(
    name = "swiftload-bench",
    about = "Measure whether segmentation actually helps"
)]
struct Args {
    /// Repetitions per treatment. Five is the minimum for a meaningful median.
    #[arg(long, default_value_t = 5)]
    reps: usize,
    /// File size per run.
    #[arg(long, default_value_t = 32 * 1024 * 1024)]
    size: u64,
    /// Per-connection cap for the throttled scenarios, in bytes per second.
    #[arg(long, default_value_t = 4_000_000)]
    bps: u64,
    /// Connection counts to test. "auto" means the adaptive governor.
    #[arg(long, value_delimiter = ',', default_values_t = ["1".to_string(), "2".to_string(), "4".to_string(), "8".to_string(), "auto".to_string()])]
    conns: Vec<String>,
    /// Write JSON results here.
    #[arg(long)]
    json: Option<std::path::PathBuf>,
    /// Write a Markdown report here.
    #[arg(long)]
    report: Option<std::path::PathBuf>,
}

#[derive(Debug, Clone, Serialize)]
struct Run {
    scenario: String,
    conns: String,
    rep: usize,
    bytes: u64,
    wall_ms: u128,
    throughput_bps: u64,
    peak_conns: usize,
    retries: u64,
    requests: u64,
    server_connections: u64,
    cpu_ms: u64,
    peak_rss_bytes: u64,
}

struct Scenario {
    name: &'static str,
    path: fn(u64, u64) -> String,
    /// What the shaping means, printed in the report so a reader knows what was measured.
    expectation: &'static str,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        name: "per-connection cap",
        path: |size, bps| format!("/throttle/bench/{size}?bps={bps}&per=conn"),
        expectation: "each connection is capped, so parallelism should scale nearly linearly",
    },
    Scenario {
        name: "shared total cap",
        path: |size, bps| format!("/throttle/bench/{size}?bps={}&per=total", bps * 4),
        expectation: "one budget is shared, so extra connections should buy nothing",
    },
    Scenario {
        name: "unshaped loopback",
        path: |size, _| format!("/plain/bench/{size}"),
        expectation: "no network bottleneck at all, so this measures overhead, not speed",
    },
];

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let server = ts::spawn("127.0.0.1:0".parse().unwrap()).await?;
    let mut runs: Vec<Run> = Vec::new();

    eprintln!(
        "swiftload-bench: {} scenarios x {} treatments x {} reps, {} per run\n",
        SCENARIOS.len(),
        args.conns.len(),
        args.reps,
        human(args.size)
    );

    for rep in 0..args.reps {
        for scenario in SCENARIOS {
            for conns in &args.conns {
                // Interleaved: every treatment runs inside every repetition, so a slow patch
                // of machine time hits all of them rather than penalising one.
                let url = server.url(&(scenario.path)(args.size, args.bps));
                let run = measure(&server, &url, conns, scenario.name, rep, args.size).await?;
                eprintln!(
                    "  rep {rep}  {:<20} {:>5} conns  {:>9}/s  {:>2} peak  {} conns opened",
                    scenario.name,
                    conns,
                    human(run.throughput_bps),
                    run.peak_conns,
                    run.server_connections
                );
                runs.push(run);
            }
        }
    }

    let report = build_report(&args, &runs);
    println!("\n{report}");

    if let Some(p) = &args.json {
        std::fs::write(p, serde_json::to_string_pretty(&runs)?)?;
        eprintln!("wrote {}", p.display());
    }
    if let Some(p) = &args.report {
        std::fs::write(p, &report)?;
        eprintln!("wrote {}", p.display());
    }
    Ok(())
}

async fn measure(
    server: &ts::Handle,
    url: &str,
    conns: &str,
    scenario: &str,
    rep: usize,
    size: u64,
) -> Result<Run> {
    let dir = tempfile::tempdir()?;
    let settings = Settings {
        download_dir: dir.path().to_path_buf(),
        // Hashing is real work but not what is being measured here.
        hash_on_complete: false,
        max_conns_per_download: 16,
        ..Default::default()
    };

    let mut req = DownloadRequest::new(url.to_string(), dir.path());
    req.filename = Some("bench.bin".into());
    req.max_conns = match conns {
        "auto" => None,
        v => Some(v.parse()?),
    };

    let conns_before = server.stats().accepts;
    let cpu_before = sysmetrics::cpu_millis();
    let started = Instant::now();

    let outcome = download(
        req,
        settings,
        Arc::new(NullSink),
        CancellationToken::new(),
        None,
    )
    .await?;
    let wall = started.elapsed();

    anyhow::ensure!(
        outcome.bytes == size,
        "short download: {} of {size}",
        outcome.bytes
    );

    Ok(Run {
        scenario: scenario.to_string(),
        conns: conns.to_string(),
        rep,
        bytes: outcome.bytes,
        wall_ms: wall.as_millis(),
        throughput_bps: (outcome.bytes as f64 / wall.as_secs_f64()) as u64,
        peak_conns: outcome.peak_conns,
        retries: outcome.retries,
        requests: outcome.requests,
        server_connections: server.stats().accepts - conns_before,
        cpu_ms: sysmetrics::cpu_millis().saturating_sub(cpu_before),
        peak_rss_bytes: sysmetrics::peak_rss_bytes(),
    })
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
    sorted[idx]
}

fn build_report(args: &Args, runs: &[Run]) -> String {
    let mut out = String::new();
    out.push_str("# SwiftLoad benchmark\n\n");
    out.push_str(&format!(
        "{} per run, {} repetitions per treatment, interleaved. Throughput is the median of \
         per-run rates, with p10/p90 to show spread.\n\n",
        human(args.size),
        args.reps
    ));

    for scenario in SCENARIOS {
        out.push_str(&format!(
            "## {}\n\n_{}_\n\n",
            scenario.name, scenario.expectation
        ));
        out.push_str("| conns | median | p10 | p90 | vs. 1 conn | peak conns | TCP conns | CPU ms | peak RSS |\n");
        out.push_str("|---|---|---|---|---|---|---|---|---|\n");

        let baseline = median_for(runs, scenario.name, "1");
        for conns in &args.conns {
            let mut rates: Vec<u64> = runs
                .iter()
                .filter(|r| r.scenario == scenario.name && &r.conns == conns)
                .map(|r| r.throughput_bps)
                .collect();
            if rates.is_empty() {
                continue;
            }
            rates.sort_unstable();
            let med = percentile(&rates, 0.5);
            let sample: Vec<&Run> = runs
                .iter()
                .filter(|r| r.scenario == scenario.name && &r.conns == conns)
                .collect();
            let speedup = if baseline > 0 {
                med as f64 / baseline as f64
            } else {
                0.0
            };
            let peak_conns = sample.iter().map(|r| r.peak_conns).max().unwrap_or(0);
            let tcp = sample
                .iter()
                .map(|r| r.server_connections)
                .max()
                .unwrap_or(0);
            let cpu = sample.iter().map(|r| r.cpu_ms).sum::<u64>() / sample.len() as u64;
            let rss = sample.iter().map(|r| r.peak_rss_bytes).max().unwrap_or(0);

            out.push_str(&format!(
                "| {conns} | {}/s | {}/s | {}/s | {speedup:.2}x | {peak_conns} | {tcp} | {cpu} | {} |\n",
                human(med),
                human(percentile(&rates, 0.1)),
                human(percentile(&rates, 0.9)),
                human(rss)
            ));
        }
        out.push('\n');
    }

    out.push_str(&interpretation(args, runs));
    out
}

fn median_for(runs: &[Run], scenario: &str, conns: &str) -> u64 {
    let mut v: Vec<u64> = runs
        .iter()
        .filter(|r| r.scenario == scenario && r.conns == conns)
        .map(|r| r.throughput_bps)
        .collect();
    v.sort_unstable();
    percentile(&v, 0.5)
}

/// Say plainly what the numbers show, including where parallelism does not help.
///
/// A benchmark report that only shows wins is marketing, and the first reader who reproduces a
/// neutral result stops trusting all of it.
fn interpretation(args: &Args, runs: &[Run]) -> String {
    let mut s = String::from("## What this shows\n\n");

    let per_conn_1 = median_for(runs, "per-connection cap", "1");
    let per_conn_auto = median_for(runs, "per-connection cap", "auto");
    let total_1 = median_for(runs, "shared total cap", "1");
    let total_8 = median_for(runs, "shared total cap", "8");

    // The best *fixed* level actually tested, not a hardcoded guess at which one won.
    // Hardcoding one level here silently flatters adaptive whenever a higher level was tested
    // and beat it, which is the one direction a benchmark must never err in.
    let (best_fixed_conns, best_fixed) = args
        .conns
        .iter()
        .filter(|c| c.as_str() != "auto")
        .map(|c| (c.clone(), median_for(runs, "per-connection cap", c)))
        .max_by_key(|(_, bps)| *bps)
        .unwrap_or_else(|| ("1".to_string(), 0));

    if per_conn_1 > 0 && best_fixed > 0 {
        s.push_str(&format!(
            "- **Where segmentation wins.** Against a per-connection cap, {best_fixed_conns} \
             connections reached {:.2}x the throughput of 1. This is the case that makes a \
             download manager worth having: the server, not the link, is the limit.\n",
            best_fixed as f64 / per_conn_1 as f64
        ));
    }
    if total_1 > 0 {
        s.push_str(&format!(
            "- **Where it does not.** Against a shared cap, 8 connections reached {:.2}x the \
             throughput of 1 — that is, essentially nothing. Extra connections here cost CPU, \
             memory and server load for no gain, which is why the default connection count is \
             conservative and the governor stops when it detects this.\n",
            total_8 as f64 / total_1 as f64
        ));
    }
    if per_conn_auto > 0 && best_fixed > 0 {
        let ratio = per_conn_auto as f64 / best_fixed as f64;
        let verdict = if ratio >= 0.9 {
            "which clears the 10% bar the project sets for itself"
        } else {
            "**which does not clear the 10% bar the project sets for itself** — the governor is              leaving throughput on the table here and that is a known gap, not a rounding error"
        };
        s.push_str(&format!(
            "- **Adaptive vs. the best fixed choice.** The governor reached {ratio:.2}x the \
             throughput of the best fixed level tested ({best_fixed_conns} connections), without \
             being told the connection count, {verdict}. The bar for adaptive concurrency is not \
             that it wins, but that it never loses badly to a sensible fixed guess.\n"
        ));
    }

    s.push_str(&format!(
        "\n### Caveats\n\n\
         - These runs are against a **local** server over loopback, so there is no real network. \
           That is deliberate: the shaping is ground truth, so the per-connection and shared-cap \
           results mean exactly what they say. It also means the unshaped row measures syscall \
           and copy overhead, not download speed.\n\
         - Real-world results depend on the server, the path, and the time of day. Nothing here \
           predicts what any particular download will do.\n\
         - {} repetitions per treatment, interleaved rather than blocked. Blocked runs conflate \
           the treatment with whatever else the machine was doing.\n\
         - No claim is made that SwiftLoad is faster than any other downloader on any particular \
           file. What is claimed is that it detects which of these regimes it is in, and adjusts.\n",
        args.reps
    ));
    s
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
