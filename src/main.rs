use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process;

use adb_proxy::auth::{generate_pair_code, validate_pair_code};
use adb_proxy::protocol::{read_okay_payload, write_service};
use adb_proxy::proxy_config::{default_proxy_config_path, ProxyFileConfig};
use adb_proxy::registry::parse_device_line;
use adb_proxy::{run_proxy, ProxyConfig};
use clap::{Parser, Subcommand};
use tokio::net::TcpStream;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(name = "adb-proxy")]
#[command(about = "TCP proxy for remote adb server access (pair-code auth required)")]
#[command(version = adb_proxy::VERSION)]
struct Args {
    #[command(subcommand)]
    command: Option<Commands>,

    #[arg(long, default_value = "0.0.0.0:5038", env = "ADB_PROXY_LISTEN", global = true)]
    listen: SocketAddr,

    #[arg(
        long,
        default_value = "127.0.0.1:5037",
        env = "ADB_PROXY_TARGET",
        global = true
    )]
    target: SocketAddr,

    /// 8-character A-Z0-9 pair code (default: random each start)
    #[arg(long, env = "ADB_PROXY_PAIR_CODE")]
    pair_code: Option<String>,

    /// Device enable policy config (default: ~/.config/adb-proxy/config.toml)
    #[arg(long, env = "ADB_PROXY_CONFIG", global = true)]
    config: Option<PathBuf>,

    #[arg(long, default_value = "info", env = "ADB_PROXY_LOG", global = true)]
    log_level: String,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Manage local USB device enable state
    Device {
        #[command(subcommand)]
        action: DeviceAction,
    },
}

#[derive(Debug, Subcommand)]
enum DeviceAction {
    /// List devices from local adb and their enable state
    List,
    /// Enable a device (by serial)
    Enable { serial: String },
    /// Disable a device (by serial)
    Disable { serial: String },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    init_tracing(&args.log_level);

    let policy_path = args
        .config
        .clone()
        .unwrap_or_else(default_proxy_config_path);

    if let Some(Commands::Device { action }) = args.command {
        if let Err(err) = run_device_command(action, &policy_path, args.target).await {
            eprintln!("adb-proxy device error: {err}");
            process::exit(1);
        }
        return Ok(());
    }

    let pair_code = match args.pair_code {
        Some(code) => {
            validate_pair_code(&code).map_err(|e| format!("invalid --pair-code: {e}"))?;
            code
        }
        None => generate_pair_code(),
    };

    run_proxy(ProxyConfig {
        listen: args.listen,
        target: args.target,
        pair_code,
        policy_path,
    })
    .await?;

    Ok(())
}

async fn run_device_command(
    action: DeviceAction,
    policy_path: &Path,
    target: SocketAddr,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut cfg = ProxyFileConfig::load_file(policy_path)?;

    match action {
        DeviceAction::Enable { serial } => {
            cfg.set_device_enabled(&serial, true);
            cfg.save_file(policy_path)?;
            println!("enabled device '{serial}' ({})", policy_path.display());
        }
        DeviceAction::Disable { serial } => {
            cfg.set_device_enabled(&serial, false);
            cfg.save_file(policy_path)?;
            println!("disabled device '{serial}' ({})", policy_path.display());
        }
        DeviceAction::List => {
            let live = fetch_local_devices(target).await.unwrap_or_else(|err| {
                eprintln!("warning: could not query local adb at {target}: {err}");
                String::new()
            });

            println!("{:<24} {:<12} ENABLED", "SERIAL", "STATE");

            let mut seen = std::collections::HashSet::new();
            for line in live.lines() {
                if let Some((serial, state, _)) = parse_device_line(line) {
                    seen.insert(serial.clone());
                    let enabled = if cfg.devices.is_enabled(&serial) {
                        "yes"
                    } else {
                        "no"
                    };
                    println!("{serial:<24} {state:<12} {enabled}");
                }
            }
            // Also show disabled devices that are not currently visible.
            for (serial, enabled) in cfg.devices.explicit_serials() {
                if !enabled && !seen.contains(serial) {
                    println!("{serial:<24} {:<12} no", "-");
                }
            }
        }
    }
    Ok(())
}

async fn fetch_local_devices(target: SocketAddr) -> Result<String, Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect(target).await?;
    write_service(&mut stream, "host:devices-l").await?;
    let body = read_okay_payload(&mut stream).await?;
    Ok(String::from_utf8_lossy(&body).into_owned())
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
