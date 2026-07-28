pub mod auth;
pub mod backend;
pub mod config;
pub mod hub;
pub mod local;
pub mod policy;
pub mod protocol;
pub mod proxy_config;
pub mod registry;
pub mod session;

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info};

use crate::auth::accept_auth;
use crate::policy::{filter_devices_body, transport_serial};
use crate::protocol::{
    read_okay_payload, read_packet, read_status, write_fail, write_okay, write_okay_payload,
    write_packet, write_service,
};
use crate::proxy_config::ReloadableProxyPolicy;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub target: SocketAddr,
    /// 8-character A-Z0-9 pair code required on every client connection.
    pub pair_code: String,
    /// Path to device enable policy (`config.toml`). Reloaded by mtime.
    pub policy_path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyStats {
    pub client_addr: SocketAddr,
    pub target_addr: SocketAddr,
    pub bytes_client_to_server: u64,
    pub bytes_server_to_client: u64,
    pub duration: Duration,
}

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("port {addr} did not become ready within {timeout:?}")]
    PortNotReady { addr: SocketAddr, timeout: Duration },
}

pub type Result<T> = std::result::Result<T, ProxyError>;

pub async fn run_proxy(config: ProxyConfig) -> Result<()> {
    run_proxy_with_shutdown(config, std::future::pending::<()>()).await
}

pub async fn run_proxy_with_shutdown(
    config: ProxyConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let listener = TcpListener::bind(config.listen).await?;
    let policy = Arc::new(ReloadableProxyPolicy::new(config.policy_path.clone()));
    info!(
        listen = %config.listen,
        target = %config.target,
        pair_code = %config.pair_code,
        policy = %config.policy_path.display(),
        "adb-proxy listening (pair with: adb-hub pair <host:port> {})",
        config.pair_code
    );

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown signal received");
                return Ok(());
            }
            accepted = listener.accept() => {
                let (client, client_addr) = accepted?;
                let target = config.target;
                let pair_code = config.pair_code.clone();
                let policy = policy.clone();
                // Normal adb clients open many short-lived TCP sessions; keep at debug.
                debug!(client = %client_addr, target = %target, "client connected");

                tokio::spawn(async move {
                    match proxy_connection(client, client_addr, target, &pair_code, &policy).await {
                        Ok(None) => {
                            info!(client = %client_addr, "client rejected (auth)");
                        }
                        Ok(Some(stats)) => {
                            debug!(
                                client = %stats.client_addr,
                                target = %stats.target_addr,
                                bytes_client_to_server = stats.bytes_client_to_server,
                                bytes_server_to_client = stats.bytes_server_to_client,
                                duration_ms = stats.duration.as_millis(),
                                "client disconnected"
                            );
                        }
                        Err(err) if is_expected_disconnect(&err) => {
                            debug!(client = %client_addr, target = %target, error = %err, "client disconnected with socket error");
                        }
                        Err(err) => {
                            error!(client = %client_addr, target = %target, error = %err, "connection failed");
                        }
                    }
                });
            }
        }
    }
}

pub async fn wait_for_port(addr: SocketAddr, max_wait: Duration) -> Result<()> {
    let start = Instant::now();

    loop {
        match timeout(Duration::from_millis(100), TcpStream::connect(addr)).await {
            Ok(Ok(_)) => return Ok(()),
            Ok(Err(err)) if start.elapsed() >= max_wait => return Err(err.into()),
            Err(_) if start.elapsed() >= max_wait => {
                return Err(ProxyError::PortNotReady {
                    addr,
                    timeout: max_wait,
                });
            }
            Ok(Err(_)) | Err(_) => sleep(Duration::from_millis(10)).await,
        }
    }
}

async fn proxy_connection(
    mut client: TcpStream,
    client_addr: SocketAddr,
    target_addr: SocketAddr,
    pair_code: &str,
    policy: &ReloadableProxyPolicy,
) -> Result<Option<ProxyStats>> {
    let started = Instant::now();
    if !accept_auth(&mut client, pair_code).await? {
        return Ok(None);
    }

    // Peek the first ADB service to decide whether to filter.
    let service_payload = match read_packet(&mut client).await {
        Ok(p) => p,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            return Ok(Some(ProxyStats {
                client_addr,
                target_addr,
                bytes_client_to_server: 0,
                bytes_server_to_client: 0,
                duration: started.elapsed(),
            }));
        }
        Err(err) => return Err(err.into()),
    };
    let service = String::from_utf8_lossy(&service_payload).into_owned();
    debug!(client = %client_addr, service = %service, "client service");

    if let Some(serial) = transport_serial(&service) {
        if !policy.is_enabled(serial) {
            write_fail(
                &mut client,
                &format!("device '{serial}' is disabled"),
            )
            .await?;
            return Ok(Some(ProxyStats {
                client_addr,
                target_addr,
                bytes_client_to_server: 0,
                bytes_server_to_client: 0,
                duration: started.elapsed(),
            }));
        }
    }

    let mut upstream = TcpStream::connect(target_addr).await?;
    debug!(client = %client_addr, target = %target_addr, "upstream connected");

    if service == "host:devices" || service == "host:devices-l" {
        write_service(&mut upstream, &service).await?;
        let body = read_okay_payload(&mut upstream).await?;
        let text = String::from_utf8_lossy(&body);
        let table = policy.refresh();
        let filtered = filter_devices_body(&text, |s| table.is_enabled(s));
        write_okay_payload(&mut client, filtered.as_bytes()).await?;
        return Ok(Some(ProxyStats {
            client_addr,
            target_addr,
            bytes_client_to_server: 0,
            bytes_server_to_client: 0,
            duration: started.elapsed(),
        }));
    }

    if service == "host:track-devices" || service == "host:track-devices-l" {
        write_service(&mut upstream, &service).await?;
        let status = read_status(&mut upstream).await?;
        if &status != b"OKAY" {
            use tokio::io::AsyncWriteExt;
            client.write_all(&status).await?;
            if &status == b"FAIL" {
                let reason = read_packet(&mut upstream).await.unwrap_or_default();
                write_packet(&mut client, &reason).await?;
            }
            return Ok(Some(ProxyStats {
                client_addr,
                target_addr,
                bytes_client_to_server: 0,
                bytes_server_to_client: 0,
                duration: started.elapsed(),
            }));
        }
        write_okay(&mut client).await?;
        loop {
            let body = match read_packet(&mut upstream).await {
                Ok(b) => b,
                Err(err) if is_benign_io(&err) => break,
                Err(err) => return Err(err.into()),
            };
            let text = String::from_utf8_lossy(&body);
            let table = policy.refresh();
            let filtered = filter_devices_body(&text, |s| table.is_enabled(s));
            if let Err(err) = write_packet(&mut client, filtered.as_bytes()).await {
                if is_benign_io(&err) {
                    break;
                }
                return Err(err.into());
            }
        }
        return Ok(Some(ProxyStats {
            client_addr,
            target_addr,
            bytes_client_to_server: 0,
            bytes_server_to_client: 0,
            duration: started.elapsed(),
        }));
    }

    // Opaque forward: replay the service packet, then pipe.
    write_service(&mut upstream, &service).await?;
    let (bytes_client_to_server, bytes_server_to_client) =
        copy_bidirectional(&mut client, &mut upstream).await?;

    Ok(Some(ProxyStats {
        client_addr,
        target_addr,
        bytes_client_to_server,
        bytes_server_to_client,
        duration: started.elapsed(),
    }))
}

fn is_benign_io(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}

fn is_expected_disconnect(err: &ProxyError) -> bool {
    match err {
        ProxyError::Io(err) => is_benign_io(err),
        ProxyError::PortNotReady { .. } => false,
    }
}
