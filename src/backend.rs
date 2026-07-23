use std::io;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::config::{BackendConfig, HubConfig};
use crate::protocol::{read_okay_payload, write_service};
use crate::registry::DeviceRegistry;

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);

/// Fetch `host:devices-l` body from one backend (without OKAY framing).
pub async fn fetch_devices_l(addr: std::net::SocketAddr) -> io::Result<String> {
    let mut stream = timeout(QUERY_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "connect timeout"))??;

    write_service(&mut stream, "host:devices-l").await?;
    let body = timeout(QUERY_TIMEOUT, read_okay_payload(&mut stream))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "read timeout"))??;

    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// Open a transport to `upstream_serial` on the given backend.
/// On success the stream is bound and ready for device services.
pub async fn open_transport(
    addr: std::net::SocketAddr,
    upstream_serial: &str,
) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(addr).await?;
    let service = format!("host:transport:{upstream_serial}");
    write_service(&mut stream, &service).await?;
    let status = crate::protocol::read_status(&mut stream).await?;
    match &status {
        b"OKAY" => Ok(stream),
        b"FAIL" => {
            let reason = crate::protocol::read_packet(&mut stream).await?;
            Err(io::Error::new(
                io::ErrorKind::Other,
                String::from_utf8_lossy(&reason).into_owned(),
            ))
        }
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unexpected transport status: {:?}",
                std::str::from_utf8(other).unwrap_or("?")
            ),
        )),
    }
}

pub async fn run_backend_poller(config: HubConfig, registry: DeviceRegistry) {
    loop {
        let mut lists: Vec<(BackendConfig, String)> = Vec::new();
        for backend in &config.backends {
            match fetch_devices_l(backend.addr).await {
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
