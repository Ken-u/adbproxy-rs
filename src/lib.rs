pub mod backend;
pub mod config;
pub mod hub;
pub mod local;
pub mod protocol;
pub mod registry;
pub mod session;

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use thiserror::Error;
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::{sleep, timeout};
use tracing::{debug, error, info, warn};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProxyConfig {
    pub listen: SocketAddr,
    pub target: SocketAddr,
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
    info!(listen = %config.listen, target = %config.target, "adb-proxy listening");

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
                info!(client = %client_addr, target = %target, "client connected");

                tokio::spawn(async move {
                    match proxy_connection(client, client_addr, target).await {
                        Ok(stats) => {
                            info!(
                                client = %stats.client_addr,
                                target = %stats.target_addr,
                                bytes_client_to_server = stats.bytes_client_to_server,
                                bytes_server_to_client = stats.bytes_server_to_client,
                                duration_ms = stats.duration.as_millis(),
                                "client disconnected"
                            );
                        }
                        Err(err) if is_expected_disconnect(&err) => {
                            warn!(client = %client_addr, target = %target, error = %err, "client disconnected with socket error");
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
) -> Result<ProxyStats> {
    let started = Instant::now();
    let mut upstream = TcpStream::connect(target_addr).await?;
    debug!(client = %client_addr, target = %target_addr, "upstream connected");

    let (bytes_client_to_server, bytes_server_to_client) =
        copy_bidirectional(&mut client, &mut upstream).await?;

    Ok(ProxyStats {
        client_addr,
        target_addr,
        bytes_client_to_server,
        bytes_server_to_client,
        duration: started.elapsed(),
    })
}

fn is_expected_disconnect(err: &ProxyError) -> bool {
    match err {
        ProxyError::Io(err) => matches!(
            err.kind(),
            io::ErrorKind::BrokenPipe
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::ConnectionAborted
                | io::ErrorKind::UnexpectedEof
        ),
        ProxyError::PortNotReady { .. } => false,
    }
}
