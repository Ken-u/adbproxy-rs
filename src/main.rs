use std::net::SocketAddr;

use adb_proxy::{run_proxy, ProxyConfig};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "adb-proxy")]
#[command(about = "Transparent TCP proxy for remote adb server access")]
struct Args {
    #[arg(long, default_value = "0.0.0.0:5038", env = "ADB_PROXY_LISTEN")]
    listen: SocketAddr,

    #[arg(long, default_value = "127.0.0.1:5037", env = "ADB_PROXY_TARGET")]
    target: SocketAddr,

    #[arg(long, default_value = "info", env = "ADB_PROXY_LOG")]
    log_level: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    init_tracing(&args.log_level);

    run_proxy(ProxyConfig {
        listen: args.listen,
        target: args.target,
    })
    .await?;

    Ok(())
}

fn init_tracing(log_level: &str) {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(log_level))
        .unwrap_or_else(|_| EnvFilter::new("info"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}
