use std::future::Future;
use std::io;
use std::sync::Arc;

use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tracing::{error, info, warn};

use crate::backend::run_backend_poller;
use crate::config::{BackendConfig, HubConfig};
use crate::local::LocalAdb;
use crate::registry::DeviceRegistry;
use crate::session::{handle_client, SessionContext};

#[derive(Debug, Error)]
pub enum HubError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error(
        "failed to bind {addr}: {source}. If a local adb server is running, stop it first with `adb kill-server`"
    )]
    Bind {
        addr: std::net::SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("local adb server error: {0}")]
    LocalAdb(io::Error),
}

pub type Result<T> = std::result::Result<T, HubError>;

pub async fn run_hub(config: HubConfig) -> Result<()> {
    run_hub_with_shutdown(config, std::future::pending::<()>()).await
}

pub async fn run_hub_with_shutdown(
    mut config: HubConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    // Keep LocalAdb alive for the hub lifetime so Drop can kill the side server.
    let _local_adb = if config.include_local {
        let local = LocalAdb::prepare(config.local_adb_port)
            .await
            .map_err(HubError::LocalAdb)?;
        // Prepend local backend; skip if user already configured the same name/addr.
        let local_backend = BackendConfig {
            name: LocalAdb::backend_name().to_string(),
            addr: local.addr,
        };
        config
            .backends
            .retain(|b| b.name != local_backend.name && b.addr != local_backend.addr);
        config.backends.insert(0, local_backend);
        Some(local)
    } else {
        None
    };

    if config.backends.is_empty() {
        return Err(HubError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no backends configured",
        )));
    }

    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|source| HubError::Bind {
            addr: config.listen,
            source,
        })?;

    info!(
        listen = %config.listen,
        backends = config.backends.len(),
        include_local = config.include_local,
        "adb-hub listening"
    );
    for b in &config.backends {
        info!(name = %b.name, addr = %b.addr, "backend configured");
    }

    let registry = DeviceRegistry::new();
    let kill_notify = Arc::new(Notify::new());

    let poller_config = config.clone();
    let poller_registry = registry.clone();
    let poller = tokio::spawn(async move {
        run_backend_poller(poller_config, poller_registry).await;
    });

    tokio::task::yield_now().await;

    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown signal received");
                poller.abort();
                return Ok(());
            }
            _ = kill_notify.notified() => {
                info!("host:kill received");
                poller.abort();
                return Ok(());
            }
            accepted = listener.accept() => {
                let (client, client_addr) = accepted?;
                info!(client = %client_addr, "client connected");
                let ctx = SessionContext {
                    registry: registry.clone(),
                    adb_version: config.adb_version,
                    kill_notify: kill_notify.clone(),
                };
                tokio::spawn(async move {
                    if let Err(err) = handle_client(client, ctx).await {
                        if is_benign(&err) {
                            warn!(client = %client_addr, error = %err, "client disconnected");
                        } else {
                            error!(client = %client_addr, error = %err, "client session failed");
                        }
                    }
                });
            }
        }
    }
}

fn is_benign(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::UnexpectedEof
    )
}
