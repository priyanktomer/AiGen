//! Standalone runner for the SwiftLoad test server.
//!
//! Used by the CLI walkthrough in `docs/PLAN.md` §N and by the benchmark harness. Tests
//! embed the library directly instead, so they get an ephemeral port per test.

use clap::Parser;
use std::net::SocketAddr;

#[derive(Parser)]
#[command(
    name = "swiftload-testserver",
    about = "Deterministic, adversarial HTTP server"
)]
struct Args {
    /// Port to bind. 0 picks an ephemeral port.
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Bind address.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?),
        )
        .init();

    let args = Args::parse();
    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    let h = swiftload_testserver::spawn(addr).await?;

    println!("swiftload-testserver listening on {}", h.base_url());
    println!();
    println!(
        "  {}/plain/alpha/104857600            honest, range-capable",
        h.base_url()
    );
    println!(
        "  {}/norange/alpha/104857600          Accept-Ranges: none",
        h.base_url()
    );
    println!(
        "  {}/liar/alpha/104857600             advertises ranges, ignores them",
        h.base_url()
    );
    println!(
        "  {}/nolen/alpha/104857600            no Content-Length",
        h.base_url()
    );
    println!(
        "  {}/throttle/alpha/104857600?bps=2000000&per=conn",
        h.base_url()
    );
    println!(
        "  {}/throttle/alpha/104857600?bps=20000000&per=total",
        h.base_url()
    );
    println!(
        "  {}/rotate-etag/alpha/104857600      same bytes, new ETag each time",
        h.base_url()
    );
    println!(
        "  {}/decoy/other/104857600            same size, different bytes",
        h.base_url()
    );
    println!(
        "  {}/mint/alpha?n=104857600           issue a fresh signed URL",
        h.base_url()
    );
    println!(
        "  {}/expire/alpha                     invalidate all signed URLs",
        h.base_url()
    );
    println!(
        "  {}/stats                            connection + byte counters",
        h.base_url()
    );
    println!();

    tokio::signal::ctrl_c().await?;
    println!("shutting down");
    Ok(())
}
