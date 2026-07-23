use std::net::SocketAddr;
use std::path::PathBuf;
use std::process;

use adb_proxy::config::{
    default_config_path, legacy_config_path, parse_backend_arg, BackendConfig, HubConfig,
};
use adb_proxy::hub::run_hub_with_shutdown;
use clap::Parser;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "adb-hub")]
#[command(about = "Local adb server that aggregates remote adb-proxy backends")]
struct Args {
    /// Listen address (default 127.0.0.1:5037)
    #[arg(long, env = "ADB_HUB_LISTEN")]
    listen: Option<SocketAddr>,

    /// Path to TOML config (default ~/.config/adb-hub/config.toml)
    #[arg(long, env = "ADB_HUB_CONFIG")]
    config: Option<PathBuf>,

    /// Backend as name=host:port or host:port (repeatable; overrides config backends)
    #[arg(long = "backend", value_name = "SPEC")]
    backends: Vec<String>,

    /// Device list poll interval in milliseconds
    #[arg(long, env = "ADB_HUB_POLL_MS")]
    poll_interval_ms: Option<u64>,

    #[arg(long, default_value = "info", env = "ADB_HUB_LOG")]
    log_level: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    init_tracing(&args.log_level);

    let config = match build_config(&args) {
        Ok(c) => c,
        Err(err) => {
            eprintln!("adb-hub config error: {err}");
            process::exit(2);
        }
    };

    if let Err(err) = run_hub_with_shutdown(config, async {
        let _ = tokio::signal::ctrl_c().await;
    })
    .await
    {
        eprintln!("adb-hub error: {err}");
        process::exit(1);
    }
}

fn build_config(args: &Args) -> Result<HubConfig, Box<dyn std::error::Error>> {
    let mut config = if !args.backends.is_empty() {
        let mut backends = Vec::new();
        for spec in &args.backends {
            backends.push(parse_backend_arg(spec)?);
        }
        HubConfig {
            listen: HubConfig::default_listen(),
            poll_interval: std::time::Duration::from_millis(1000),
            backends,
            adb_version: 40,
        }
    } else if let Some(path) = args.config.as_ref() {
        HubConfig::load_file(path)?
    } else {
        load_default_config()?
    };

    if let Some(listen) = args.listen {
        config.listen = listen;
    }
    if let Some(ms) = args.poll_interval_ms {
        config.poll_interval = std::time::Duration::from_millis(ms.max(100));
    }

    // If --backend was not used but --listen was with a loaded file, keep file backends.
    // If --backend was used, already set. Allow combining: when both config file and
    // --backend are given, --backend wins (handled above).
    if args.backends.is_empty() && args.config.is_none() {
        // already loaded default
    }

    if config.backends.is_empty() {
        return Err("no backends configured; use --backend or a config file".into());
    }

    // Deduplicate by keeping last definition of a name.
    let mut seen = std::collections::HashSet::new();
    let mut unique: Vec<BackendConfig> = Vec::new();
    for b in config.backends.into_iter().rev() {
        if seen.insert(b.name.clone()) {
            unique.push(b);
        }
    }
    unique.reverse();
    config.backends = unique;

    Ok(config)
}

fn load_default_config() -> Result<HubConfig, Box<dyn std::error::Error>> {
    let path = default_config_path();
    if path.is_file() {
        return Ok(HubConfig::load_file(&path)?);
    }
    let legacy = legacy_config_path();
    if legacy.is_file() {
        eprintln!(
            "adb-hub: loading legacy config from {} (consider migrating to {})",
            legacy.display(),
            path.display()
        );
        return Ok(HubConfig::load_legacy_file(&legacy)?);
    }
    Err(format!(
        "no config found at {} or {}; pass --backend name=host:port",
        path.display(),
        legacy.display()
    )
    .into())
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
