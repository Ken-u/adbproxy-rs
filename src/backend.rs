use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::auth::authenticate_stream;
use crate::config::{BackendConfig, HubConfig};
use crate::protocol::{read_okay_payload, write_service};
use crate::registry::DeviceRegistry;

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Connect to a backend and optionally authenticate with its pair code.
pub async fn connect_backend(addr: SocketAddr, pair_code: Option<&str>) -> io::Result<TcpStream> {
    let mut stream = timeout(QUERY_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))??;
    if let Some(code) = pair_code {
        timeout(QUERY_TIMEOUT, authenticate_stream(&mut stream, code))
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "auth timeout"))??;
    }
    Ok(stream)
}

/// Query `host:version` from an adb server; returns decimal ADB_SERVER_VERSION.
pub async fn fetch_server_version(
    addr: SocketAddr,
    pair_code: Option<&str>,
) -> io::Result<u32> {
    let mut stream = connect_backend(addr, pair_code).await?;
    write_service(&mut stream, "host:version").await?;
    let body = timeout(QUERY_TIMEOUT, read_okay_payload(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "read timeout"))??;
    let text = std::str::from_utf8(&body)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    u32::from_str_radix(text.trim(), 16)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Fetch `host:devices-l` body from one backend (without OKAY framing).
pub async fn fetch_devices_l(addr: SocketAddr, pair_code: Option<&str>) -> io::Result<String> {
    let mut stream = connect_backend(addr, pair_code).await?;

    write_service(&mut stream, "host:devices-l").await?;
    let body = timeout(QUERY_TIMEOUT, read_okay_payload(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "read timeout"))??;

    Ok(String::from_utf8_lossy(&body).into_owned())
}

pub async fn run_backend_poller(config: HubConfig, registry: DeviceRegistry) {
    loop {
        let mut lists: Vec<(BackendConfig, String)> = Vec::new();
        for backend in &config.backends {
            match fetch_devices_l(backend.addr, backend.pair_code.as_deref()).await {
                Ok(body) => {
                    debug!(backend = %backend.name, addr = %backend.addr, "polled devices");
                    lists.push((backend.clone(), body));
                }
                Err(err) => {
                    warn!(
                        backend = %backend.name,
                        addr = %backend.addr,
                        error = %err,
                        "backend poll failed"
                    );
                    // Treat unreachable backend as empty list so devices disappear.
                    lists.push((backend.clone(), String::new()));
                }
            }
        }
        registry.update_from_backend_lists(&lists).await;
        tokio::time::sleep(config.poll_interval).await;
    }
}
