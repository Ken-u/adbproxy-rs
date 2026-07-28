//! Compatibility binary: same as `adb-hub --daemon`.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;
use std::time::Duration;

use adb_proxy::config::{default_config_path, HubConfig};
use adb_proxy::service::{run_service_with_shutdown, ServiceConfig};
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[cfg(unix)]
use adb_proxy::ipc::DEFAULT_CONTROL_ABSTRACT;

#[cfg(not(unix))]
const DEFAULT_CONTROL_ABSTRACT: &str = "adb-hubd";

#[derive(Debug, Parser)]
#[command(name = "adb-hubd")]
#[command(about = "Same as `adb-hub --daemon` (compatibility alias)")]
#[command(version = adb_proxy::VERSION)]
struct Args {
    #[arg(long, default_value = "127.0.0.1:5037", env = "ADB_HUBD_LISTEN")]
    listen: SocketAddr,

    #[arg(long, default_value_t = 5039, env = "ADB_HUBD_LOCAL_PORT")]
    local_port: u16,

    #[arg(long = "no-local", env = "ADB_HUBD_NO_LOCAL")]
    no_local: bool,

    #[arg(long, default_value = DEFAULT_CONTROL_ABSTRACT, env = "ADB_HUBD_CONTROL", hide = true)]
    control: String,

    #[arg(long, env = "ADB_HUBD_CONTROL_PATH", hide = true)]
    control_path: Option<PathBuf>,

    #[arg(long, default_value_t = 1000, env = "ADB_HUBD_POLL_MS")]
    poll_interval_ms: u64,

    #[arg(long)]
    foreground: bool,

    #[arg(long, env = "ADB_HUB_CONFIG")]
    config: Option<PathBuf>,

    #[arg(long, default_value = "info", env = "ADB_HUBD_LOG")]
    log_level: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    init_tracing(&args.log_level);
    let _ = args.foreground;

    let mut hub = if let Some(path) = &args.config {
        HubConfig::load_file(path).unwrap_or_else(|_| HubConfig::local_only())
    } else {
        let path = default_config_path();
        if path.is_file() {
            HubConfig::load_file(&path).unwrap_or_else(|_| HubConfig::local_only())
        } else {
            HubConfig::local_only()
        }
    };
    hub.listen = args.listen;
    hub.local_adb_port = args.local_port;
    hub.include_local = !args.no_local;
    hub.poll_interval = Duration::from_millis(args.poll_interval_ms.max(100));

    let mut service = ServiceConfig::from_hub(hub);
    service.control_abstract = args.control;
    service.control_path = args.control_path;

    if let Err(err) = run_service_with_shutdown(service, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    {
        eprintln!("adb-hubd error: {err}");
        process::exit(1);
    }
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
