//! Unified hub service entry: one command for every user.
//!
//! On Linux (shared `:5037` supported):
//! - If nothing owns the listen port yet, become the shared daemon.
//! - Always run this user's agent (private backends / pair codes).
//! - If another process already owns the daemon, only run the agent.
//!
//! Elsewhere: silently run the classic in-process hub (local + remotes).

use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;
use tracing::info;

use crate::config::HubConfig;
use crate::hub::{run_hub_with_shutdown, HubError};
use crate::peercred::multi_user_supported;

#[cfg(unix)]
use tracing::warn;
#[cfg(unix)]
use crate::agent::{run_agent_with_shutdown, AgentConfig, AgentError};
#[cfg(unix)]
use crate::daemon::{run_daemon_with_shutdown, DaemonConfig, DaemonError};
#[cfg(unix)]
use crate::ipc::DEFAULT_CONTROL_ABSTRACT;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error(transparent)]
    Hub(#[from] HubError),

    #[cfg(unix)]
    #[error(transparent)]
    Daemon(#[from] DaemonError),

    #[cfg(unix)]
    #[error(transparent)]
    Agent(#[from] AgentError),

    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, ServiceError>;

#[derive(Clone, Debug)]
pub struct ServiceConfig {
    pub listen: SocketAddr,
    pub local_adb_port: u16,
    pub include_local: bool,
    pub poll_interval: Duration,
    pub adb_version: u32,
    /// User hub config (remote backends + pair codes).
    pub hub: HubConfig,
    pub control_abstract: String,
    pub control_path: Option<PathBuf>,
}

impl ServiceConfig {
    pub fn from_hub(hub: HubConfig) -> Self {
        Self {
            listen: hub.listen,
            local_adb_port: hub.local_adb_port,
            include_local: hub.include_local,
            poll_interval: hub.poll_interval,
            adb_version: hub.adb_version,
            hub,
            control_abstract: {
                #[cfg(unix)]
                {
                    DEFAULT_CONTROL_ABSTRACT.to_string()
                }
                #[cfg(not(unix))]
                {
                    String::new()
                }
            },
            control_path: None,
        }
    }
}

/// Run the user-facing hub service until `shutdown` completes.
pub async fn run_service_with_shutdown(
    config: ServiceConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    if multi_user_supported() {
        #[cfg(unix)]
        {
            return run_shared_service(config, shutdown).await;
        }
        #[cfg(not(unix))]
        {
            unreachable!("multi_user_supported is false off Linux");
        }
    }

    run_classic_service(config, shutdown).await
}

async fn run_classic_service(
    config: ServiceConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    info!(listen = %config.listen, "adb-hub starting");
    let mut hub = config.hub;
    hub.listen = config.listen;
    hub.local_adb_port = config.local_adb_port;
    hub.include_local = config.include_local;
    hub.poll_interval = config.poll_interval;
    hub.adb_version = config.adb_version;
    run_hub_with_shutdown(hub, shutdown)
        .await
        .map_err(ServiceError::from)
}

#[cfg(unix)]
async fn run_shared_service(
    config: ServiceConfig,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    use tokio::sync::oneshot;

    let agent_cfg = AgentConfig {
        config_path: config
            .hub
            .config_path
            .clone()
            .unwrap_or_else(crate::config::default_config_path),
        control_abstract: config.control_abstract.clone(),
        control_path: config.control_path.clone(),
        poll_interval: config.poll_interval,
        inline_backends: Some(config.hub.clone()),
        connect_retries: 50,
        connect_retry_delay: Duration::from_millis(100),
    };

    let daemon_cfg = DaemonConfig {
        listen: config.listen,
        local_adb_port: config.local_adb_port,
        include_local: config.include_local,
        control_abstract: config.control_abstract.clone(),
        control_path: config.control_path.clone(),
        poll_interval: config.poll_interval,
        adb_version: config.adb_version,
    };

    let we_start_daemon = match tokio::net::TcpListener::bind(config.listen).await {
        Ok(listener) => {
            drop(listener);
            true
        }
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            info!(
                listen = %config.listen,
                "shared hub already running; registering this user's backends"
            );
            false
        }
        Err(err) => return Err(err.into()),
    };

    if !we_start_daemon {
        return run_agent_with_shutdown(agent_cfg, shutdown)
            .await
            .map_err(ServiceError::from);
    }

    info!(
        listen = %config.listen,
        "adb-hub starting (shared listener + this user's backends)"
    );

    let (daemon_stop_tx, daemon_stop_rx) = oneshot::channel::<()>();
    let (agent_stop_tx, agent_stop_rx) = oneshot::channel::<()>();

    let mut daemon_task = tokio::spawn(async move {
        run_daemon_with_shutdown(daemon_cfg, async move {
            let _ = daemon_stop_rx.await;
        })
        .await
    });
    let mut agent_task = tokio::spawn(async move {
        run_agent_with_shutdown(agent_cfg, async move {
            let _ = agent_stop_rx.await;
        })
        .await
    });

    tokio::pin!(shutdown);
    tokio::select! {
        _ = &mut shutdown => {
            let _ = agent_stop_tx.send(());
            let _ = daemon_stop_tx.send(());
            let _ = (&mut agent_task).await;
            let _ = (&mut daemon_task).await;
            Ok(())
        }
        res = &mut agent_task => {
            let _ = daemon_stop_tx.send(());
            let _ = (&mut daemon_task).await;
            match res {
                Ok(Ok(())) => Ok(()),
                Ok(Err(err)) => Err(err.into()),
                Err(err) => Err(io::Error::new(io::ErrorKind::Other, err).into()),
            }
        }
        res = &mut daemon_task => {
            match res {
                Ok(Ok(())) => {
                    let _ = agent_stop_tx.send(());
                    let _ = (&mut agent_task).await;
                    Ok(())
                }
                Ok(Err(DaemonError::Bind { source, .. }))
                    if source.kind() == io::ErrorKind::AddrInUse =>
                {
                    warn!("another hub took the listen port; continuing with this user's agent");
                    tokio::select! {
                        _ = &mut shutdown => {
                            let _ = agent_stop_tx.send(());
                            let _ = (&mut agent_task).await;
                            Ok(())
                        }
                        ares = &mut agent_task => {
                            match ares {
                                Ok(Ok(())) => Ok(()),
                                Ok(Err(err)) => Err(err.into()),
                                Err(err) => Err(io::Error::new(io::ErrorKind::Other, err).into()),
                            }
                        }
                    }
                }
                Ok(Err(err)) => {
                    let _ = agent_stop_tx.send(());
                    let _ = (&mut agent_task).await;
                    Err(err.into())
                }
                Err(err) => {
                    let _ = agent_stop_tx.send(());
                    let _ = (&mut agent_task).await;
                    Err(io::Error::new(io::ErrorKind::Other, err).into())
                }
            }
        }
    }
}
