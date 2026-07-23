use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{sleep, timeout};
use tracing::{debug, info, warn};

use crate::auth::authenticate_stream;
use crate::config::{BackendConfig, HubConfig};
use crate::protocol::{read_okay_payload, read_packet, read_status, write_service};
use crate::registry::DeviceRegistry;

const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const RECONNECT_WAIT: Duration = Duration::from_secs(2);

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

/// One-shot poll of every backend (used before hub accepts clients).
pub async fn poll_backends_once(config: &HubConfig, registry: &DeviceRegistry) {
    let mut lists: Vec<(BackendConfig, String)> = Vec::new();
    for backend in &config.backends {
        match fetch_devices_l(backend.addr, backend.pair_code.as_deref()).await {
            Ok(body) => {
                debug!(backend = %backend.name, addr = %backend.addr, "initial device poll ok");
                lists.push((backend.clone(), body));
            }
            Err(err) => {
                warn!(
                    backend = %backend.name,
                    addr = %backend.addr,
                    error = %err,
                    "initial device poll failed"
                );
                lists.push((backend.clone(), String::new()));
            }
        }
    }
    registry.update_from_backend_lists(&lists).await;
}

/// Prefer long-lived `host:track-devices-l` per backend; fall back to periodic
/// `host:devices-l` if track is unsupported. This avoids connect/disconnect spam.
pub async fn run_backend_poller(config: HubConfig, registry: DeviceRegistry) {
    let shared: Arc<Mutex<HashMap<String, (BackendConfig, String)>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // Seed shared from a fresh poll so partial watcher publishes never wipe peers.
    {
        let mut guard = shared.lock().await;
        for backend in &config.backends {
            let body = match fetch_devices_l(backend.addr, backend.pair_code.as_deref()).await {
                Ok(body) => body,
                Err(err) => {
                    warn!(
                        backend = %backend.name,
                        addr = %backend.addr,
                        error = %err,
                        "poller seed poll failed"
                    );
                    String::new()
                }
            };
            guard.insert(backend.name.clone(), (backend.clone(), body));
        }
    }
    publish_shared(&registry, &shared, &config.backends).await;

    let mut tasks = Vec::new();
    for backend in config.backends.clone() {
        let registry = registry.clone();
        let shared = shared.clone();
        let order = config.backends.clone();
        let poll_interval = config.poll_interval;
        tasks.push(tokio::spawn(async move {
            run_backend_watcher(backend, registry, shared, order, poll_interval).await;
        }));
    }

    for t in tasks {
        let _ = t.await;
    }
}

async fn run_backend_watcher(
    backend: BackendConfig,
    registry: DeviceRegistry,
    shared: Arc<Mutex<HashMap<String, (BackendConfig, String)>>>,
    order: Vec<BackendConfig>,
    poll_interval: Duration,
) {
    // Probe whether track-devices-l works; otherwise use interval polling.
    match probe_track_support(&backend).await {
        Ok(true) => {
            info!(backend = %backend.name, "using track-devices-l watcher");
            loop {
                if let Err(err) = watch_track_devices(&backend, &registry, &shared, &order).await {
                    warn!(
                        backend = %backend.name,
                        error = %err,
                        "track-devices watcher ended; reconnecting"
                    );
                    sleep(RECONNECT_WAIT).await;
                }
            }
        }
        Ok(false) | Err(_) => {
            info!(
                backend = %backend.name,
                interval_ms = poll_interval.as_millis(),
                "falling back to devices-l polling"
            );
            loop {
                match fetch_devices_l(backend.addr, backend.pair_code.as_deref()).await {
                    Ok(body) => {
                        {
                            let mut guard = shared.lock().await;
                            guard.insert(backend.name.clone(), (backend.clone(), body));
                        }
                        publish_shared(&registry, &shared, &order).await;
                    }
                    Err(err) => {
                        warn!(
                            backend = %backend.name,
                            addr = %backend.addr,
                            error = %err,
                            "devices-l poll failed"
                        );
                        {
                            let mut guard = shared.lock().await;
                            guard.insert(backend.name.clone(), (backend.clone(), String::new()));
                        }
                        publish_shared(&registry, &shared, &order).await;
                    }
                }
                sleep(poll_interval).await;
            }
        }
    }
}

async fn probe_track_support(backend: &BackendConfig) -> io::Result<bool> {
    let mut stream = connect_backend(backend.addr, backend.pair_code.as_deref()).await?;
    write_service(&mut stream, "host:track-devices-l").await?;
    let status = read_status(&mut stream).await?;
    if &status == b"OKAY" {
        // Drain one snapshot then drop (watcher will reconnect).
        let _ = read_packet(&mut stream).await;
        return Ok(true);
    }
    if &status == b"FAIL" {
        let reason = read_packet(&mut stream).await.unwrap_or_default();
        debug!(
            backend = %backend.name,
            reason = %String::from_utf8_lossy(&reason),
            "track-devices-l not supported"
        );
        return Ok(false);
    }
    Ok(false)
}

async fn watch_track_devices(
    backend: &BackendConfig,
    registry: &DeviceRegistry,
    shared: &Arc<Mutex<HashMap<String, (BackendConfig, String)>>>,
    order: &[BackendConfig],
) -> io::Result<()> {
    let mut stream = connect_backend(backend.addr, backend.pair_code.as_deref()).await?;
    write_service(&mut stream, "host:track-devices-l").await?;
    let status = read_status(&mut stream).await?;
    if &status != b"OKAY" {
        let reason = if &status == b"FAIL" {
            read_packet(&mut stream).await.unwrap_or_default()
        } else {
            status.to_vec()
        };
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!(
                "track-devices-l status {}: {}",
                String::from_utf8_lossy(&status),
                String::from_utf8_lossy(&reason)
            ),
        ));
    }

    loop {
        let body = read_packet(&mut stream).await?;
        let text = String::from_utf8_lossy(&body).into_owned();
        debug!(backend = %backend.name, bytes = text.len(), "track-devices update");
        {
            let mut guard = shared.lock().await;
            guard.insert(backend.name.clone(), (backend.clone(), text));
        }
        publish_shared(registry, shared, order).await;
    }
}

async fn publish_shared(
    registry: &DeviceRegistry,
    shared: &Arc<Mutex<HashMap<String, (BackendConfig, String)>>>,
    order: &[BackendConfig],
) {
    let guard = shared.lock().await;
    // Require a seeded entry for every backend before publishing, so a late
    // watcher cannot briefly replace the registry with an incomplete merge.
    if !order.iter().all(|b| guard.contains_key(&b.name)) {
        return;
    }
    let lists: Vec<(BackendConfig, String)> = order
        .iter()
        .map(|b| {
            let (cfg, body) = guard.get(&b.name).expect("checked above");
            (cfg.clone(), body.clone())
        })
        .collect();
    drop(guard);
    registry.update_from_backend_lists(&lists).await;
}
