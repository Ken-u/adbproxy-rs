use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;

use adb_proxy::auth::{authenticate_stream, validate_pair_code};
use adb_proxy::backend::fetch_devices_l;
use adb_proxy::config::{
    default_backend_name, default_config_path, legacy_config_path, old_config_path,
    parse_backend_arg, BackendConfig, HubConfig,
};
use adb_proxy::hub::run_hub_with_shutdown;
use adb_proxy::policy::DevicePolicyTable;
use adb_proxy::registry::merge_device_lists;
use clap::{Parser, Subcommand};
use tokio::net::TcpStream;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "adb-hub")]
#[command(about = "Local adb server that aggregates local USB + remote adb-proxy backends")]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Listen address (default 127.0.0.1:5037)
    #[arg(long, env = "ADB_HUB_LISTEN", global = true)]
    listen: Option<SocketAddr>,

    /// Path to TOML config (default: %APPDATA%\adb-hub\config.toml on Windows,
    /// ~/.config/adb-hub\config.toml on Linux/macOS)
    #[arg(long, env = "ADB_HUB_CONFIG", global = true)]
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

    #[arg(long, default_value = "info", env = "ADB_HUB_LOG", global = true)]
    log_level: String,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Pair with a remote adb-proxy and save it to the hub config
    Pair {
        /// adb-proxy address (host:port)
        addr: SocketAddr,
        /// 8-character A-Z0-9 pair code shown by adb-proxy
        code: String,
        /// Backend name stored in config (default derived from addr)
        #[arg(long)]
        name: Option<String>,
    },
    /// Manage device enable state (public serial)
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
    /// Manage paired node (backend) enable state
    Node {
        #[command(subcommand)]
        action: NodeAction,
    },
}

#[derive(Debug, Subcommand)]
enum DeviceAction {
    List,
    Enable { serial: String },
    Disable { serial: String },
}

#[derive(Debug, Subcommand)]
enum NodeAction {
    List,
    Enable { name: String },
    Disable { name: String },
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    init_tracing(&args.log_level);

    match args.command {
        Some(Commands::Pair { addr, code, name }) => {
            if let Err(err) = run_pair(addr, &code, name.as_deref(), args.config.as_ref()).await {
                eprintln!("adb-hub pair error: {err}");
                process::exit(1);
            }
        }
        Some(Commands::Device { action }) => {
            if let Err(err) = run_device(action, args.config.as_ref()).await {
                eprintln!("adb-hub device error: {err}");
                process::exit(1);
            }
        }
        Some(Commands::Node { action }) => {
            if let Err(err) = run_node(action, args.config.as_ref()) {
                eprintln!("adb-hub node error: {err}");
                process::exit(1);
            }
        }
        None => {
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
    }
}

async fn run_pair(
    addr: SocketAddr,
    code: &str,
    name: Option<&str>,
    config_path: Option<&PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    validate_pair_code(code)?;

    let mut stream = TcpStream::connect(addr).await?;
    authenticate_stream(&mut stream, code).await?;
    drop(stream);

    let path = config_path.cloned().unwrap_or_else(default_config_path);
    let mut config = if path.is_file() {
        HubConfig::load_file(&path)?
    } else {
        HubConfig::local_only()
    };

    let backend_name = name
        .map(str::to_string)
        .unwrap_or_else(|| default_backend_name(addr));
    config.upsert_backend(BackendConfig {
        name: backend_name.clone(),
        addr,
        pair_code: Some(code.to_string()),
        enabled: true,
    });
    config.save_file(&path)?;

    println!(
        "paired backend '{backend_name}' at {addr} (pair_code saved to {})",
        path.display()
    );
    Ok(())
}

fn resolve_config_path(config_path: Option<&PathBuf>) -> PathBuf {
    config_path.cloned().unwrap_or_else(default_config_path)
}

fn load_or_empty(path: &Path) -> Result<HubConfig, Box<dyn std::error::Error>> {
    if path.is_file() {
        Ok(HubConfig::load_file(path)?)
    } else {
        Ok(HubConfig::local_only())
    }
}

async fn run_device(
    action: DeviceAction,
    config_path: Option<&PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = resolve_config_path(config_path);
    let mut config = load_or_empty(&path)?;

    match action {
        DeviceAction::Enable { serial } => {
            config.set_device_enabled(&serial, true);
            config.save_file(&path)?;
            println!("enabled device '{serial}' ({})", path.display());
        }
        DeviceAction::Disable { serial } => {
            config.set_device_enabled(&serial, false);
            config.save_file(&path)?;
            println!("disabled device '{serial}' ({})", path.display());
        }
        DeviceAction::List => {
            let mut lists = Vec::new();
            for backend in config.enabled_backends() {
                match fetch_devices_l(backend.addr, backend.pair_code.as_deref()).await {
                    Ok(body) => lists.push((backend, body)),
                    Err(err) => {
                        eprintln!(
                            "warning: poll {} ({}) failed: {err}",
                            backend.name, backend.addr
                        );
                        lists.push((backend, String::new()));
                    }
                }
            }
            let snap = merge_device_lists(&lists);

            println!(
                "{:<28} {:<12} {:<10} NODE",
                "SERIAL", "STATE", "ENABLED"
            );
            let mut seen = std::collections::HashSet::new();
            for d in &snap.devices {
                seen.insert(d.public_serial.clone());
                let enabled = if config.devices.is_enabled(&d.public_serial) {
                    "yes"
                } else {
                    "no"
                };
                println!(
                    "{:<28} {:<12} {:<10} {}",
                    d.public_serial, d.state, enabled, d.backend_name
                );
            }
            for (serial, enabled) in config.devices.explicit_serials() {
                if !enabled && !seen.contains(serial) {
                    println!("{serial:<28} {:<12} {:<10} -", "-", "no");
                }
            }
        }
    }
    Ok(())
}

fn run_node(
    action: NodeAction,
    config_path: Option<&PathBuf>,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = resolve_config_path(config_path);
    let mut config = load_or_empty(&path)?;

    match action {
        NodeAction::Enable { name } => {
            config.set_backend_enabled(&name, true)?;
            config.save_file(&path)?;
            println!("enabled node '{name}' ({})", path.display());
        }
        NodeAction::Disable { name } => {
            config.set_backend_enabled(&name, false)?;
            config.save_file(&path)?;
            println!("disabled node '{name}' ({})", path.display());
        }
        NodeAction::List => {
            println!("{:<20} {:<24} ENABLED", "NAME", "ADDR");
            for b in &config.backends {
                let enabled = if b.enabled { "yes" } else { "no" };
                println!("{:<20} {:<24} {enabled}", b.name, b.addr);
            }
            if config.backends.is_empty() {
                println!("(no paired nodes)");
            }
        }
    }
    Ok(())
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
            adb_version: 41,
            include_local: !args.no_local,
            local_adb_port: args.local_port.unwrap_or(5039),
            devices: DevicePolicyTable::default(),
            config_path: None,
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

    if config.backends.iter().all(|b| !b.enabled) && !config.include_local {
        return Err("no enabled backends configured; enable a node or use --local".into());
    }

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
    // Fall back to the previous Windows location (~/.config/adb-hub/config.toml)
    // so existing installs keep working after the APPDATA migration.
    let old = old_config_path();
    if old.is_file() && old != path {
        eprintln!(
            "adb-hub: loading config from legacy location {} (consider moving it to {})",
            old.display(),
            path.display()
        );
        return Ok(HubConfig::load_file(&old)?);
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
        "no config found at {}, {}, or {}; pass --backend name=host:port or rely on --local",
        path.display(),
        old.display(),
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
