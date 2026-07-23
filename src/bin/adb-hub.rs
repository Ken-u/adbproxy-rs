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
#[command(about = "Local adb server that aggregates local USB + remote adb-proxy backends")]
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

    /// Do not start/aggregate the local USB adb server (default: aggregate local)
    #[arg(long = "no-local", env = "ADB_HUB_NO_LOCAL")]
    no_local: bool,

    /// Side port for the real local adb server (default 5039)
    #[arg(long, env = "ADB_HUB_LOCAL_PORT")]
    local_port: Option<u16>,

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
            include_local: !args.no_local,
            local_adb_port: args.local_port.unwrap_or(5039),
        }
    } else if let Some(path) = args.config.as_ref() {
        let mut c = HubConfig::load_file(path)?;
        if args.no_local {
            c.include_local = false;
        }
        if let Some(p) = args.local_port {
            c.local_adb_port = p;
        }
        c
    } else {
        match load_default_config() {
            Ok(mut c) => {
                if args.no_local {
                    c.include_local = false;
                }
                if let Some(p) = args.local_port {
                    c.local_adb_port = p;
                }
                c
            }
            Err(_) if !args.no_local => {
                let mut c = HubConfig::local_only();
                if let Some(p) = args.local_port {
                    c.local_adb_port = p;
                }
                c
            }
            Err(err) => return Err(err),
        }
    };

    if let Some(listen) = args.listen {
        config.listen = listen;
    }
    if let Some(ms) = args.poll_interval_ms {
        config.poll_interval = std::time::Duration::from_millis(ms.max(100));
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

    if config.backends.is_empty() && !config.include_local {
        return Err("no backends configured; use --backend, a config file, or enable --local".into());
    }

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
        "no config found at {} or {}; pass --backend name=host:port or rely on --local",
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
