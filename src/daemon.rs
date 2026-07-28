//! Shared multi-user daemon (`adb-hubd`).
//!
//! Owns `127.0.0.1:5037`, the shared local ADB backend, UID routing, and the
//! agent control socket. Pair codes never enter this process.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{mpsc, Mutex, Notify, RwLock};
use tracing::{debug, error, info, warn};

use crate::backend::{fetch_devices_l, fetch_server_version};
use crate::config::BackendConfig;
use crate::ipc::{
    read_message, sanitized_to_snapshot, write_message, IpcMessage, OpenPrivateStream,
    PROTOCOL_VERSION, StreamClose, DEFAULT_CONTROL_ABSTRACT,
};
use crate::local::LocalAdb;
use crate::peercred::{is_loopback, multi_user_supported, multi_user_unsupported_reason, peer_cred_unix, tcp_peer_uid};
use crate::protocol::{
    read_packet, write_fail, write_okay, write_okay_payload, write_packet, write_service,
};
use crate::registry::DeviceSnapshot;
use crate::session::{pick_preferred_device, rewrite_upstream_service};
use crate::tenant::{TenantRegistry, Uid};

#[derive(Debug, Error)]
pub enum DaemonError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error(
        "failed to bind {addr}: {source}. If a local adb server is running, stop it first with `adb kill-server`"
    )]
    Bind {
        addr: SocketAddr,
        #[source]
        source: io::Error,
    },

    #[error("local adb server error: {0}")]
    LocalAdb(io::Error),

    #[error("{}", multi_user_unsupported_reason())]
    UnsupportedPlatform,
}

pub type Result<T> = std::result::Result<T, DaemonError>;

#[derive(Clone, Debug)]
pub struct DaemonConfig {
    pub listen: SocketAddr,
    pub local_adb_port: u16,
    pub include_local: bool,
    pub control_abstract: String,
    pub control_path: Option<PathBuf>,
    pub poll_interval: Duration,
    pub adb_version: u32,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:5037".parse().expect("valid"),
            local_adb_port: 5039,
            include_local: true,
            control_abstract: DEFAULT_CONTROL_ABSTRACT.into(),
            control_path: None,
            poll_interval: Duration::from_millis(1000),
            adb_version: 41,
        }
    }
}

struct AgentConn {
    uid: Uid,
    instance_token: String,
    writer: Arc<Mutex<WriteHalf<UnixStream>>>,
    /// stream_id → channel of bytes from agent toward the ADB client
    stream_tx: Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>,
    /// Pending OpenResult waiters
    open_waiters: Mutex<HashMap<u32, tokio::sync::oneshot::Sender<OpenOutcome>>>,
}

#[derive(Debug)]
struct OpenOutcome {
    ok: bool,
    fail_reason: Option<String>,
}

struct DaemonState {
    tenants: TenantRegistry,
    agents: RwLock<HashMap<Uid, Arc<AgentConn>>>,
    next_stream_id: AtomicU32,
    local_backend: Option<BackendConfig>,
    adb_version: std::sync::atomic::AtomicU32,
}

pub async fn run_daemon(config: DaemonConfig) -> Result<()> {
    run_daemon_with_shutdown(config, std::future::pending::<()>()).await
}

pub async fn run_daemon_with_shutdown(
    mut config: DaemonConfig,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    if !multi_user_supported() {
        return Err(DaemonError::UnsupportedPlatform);
    }

    if !is_loopback(config.listen) {
        return Err(DaemonError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "multi-user daemon must listen on loopback only",
        )));
    }

    let _local_adb = if config.include_local {
        let local = LocalAdb::prepare(config.local_adb_port)
            .await
            .map_err(DaemonError::LocalAdb)?;
        match fetch_server_version(local.addr, None).await {
            Ok(v) => {
                info!(adb_version = v, "synced hub version from local adb server");
                config.adb_version = v;
            }
            Err(err) => {
                warn!(error = %err, "could not read local adb version; using configured value");
            }
        }
        Some(local)
    } else {
        None
    };

    let local_backend = _local_adb.as_ref().map(|local| BackendConfig {
        name: LocalAdb::backend_name().to_string(),
        addr: local.addr,
        pair_code: None,
        enabled: true,
    });

    let state = Arc::new(DaemonState {
        tenants: TenantRegistry::new(),
        agents: RwLock::new(HashMap::new()),
        next_stream_id: AtomicU32::new(1),
        local_backend: local_backend.clone(),
        adb_version: std::sync::atomic::AtomicU32::new(config.adb_version),
    });

    // Seed shared devices before accepting ADB clients.
    if let Some(ref backend) = local_backend {
        poll_shared_once(&state, backend).await;
    }

    let control = bind_control(&config).await?;
    info!(
        abstract = %config.control_abstract,
        path = ?config.control_path,
        "adb-hubd control socket ready"
    );

    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|source| DaemonError::Bind {
            addr: config.listen,
            source,
        })?;
    info!(listen = %config.listen, "adb-hubd listening (multi-user)");

    let poll_state = state.clone();
    let poll_backend = local_backend.clone();
    let poll_interval = config.poll_interval;
    let poller = tokio::spawn(async move {
        let Some(backend) = poll_backend else {
            return;
        };
        loop {
            tokio::time::sleep(poll_interval).await;
            poll_shared_once(&poll_state, &backend).await;
        }
    });

    let kill_notify = Arc::new(Notify::new());
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("shutdown signal received");
                poller.abort();
                return Ok(());
            }
            // host:kill must NOT stop the daemon (acceptance criteria).
            _ = kill_notify.notified() => {
                info!("host:kill ignored (multi-user daemon stays up)");
            }
            accepted = listener.accept() => {
                let (client, peer) = accepted?;
                let local = match client.local_addr() {
                    Ok(a) => a,
                    Err(err) => {
                        warn!(error = %err, "missing local addr");
                        continue;
                    }
                };
                if !is_loopback(peer) {
                    warn!(%peer, "rejecting non-loopback ADB client");
                    continue;
                }
                let state = state.clone();
                let kill_notify = kill_notify.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_adb_client(client, local, peer, state, kill_notify).await {
                        if !is_benign(&err) {
                            error!(%peer, error = %err, "ADB client session failed");
                        }
                    }
                });
            }
            accepted = control.accept() => {
                let (stream, _addr) = match accepted {
                    Ok(v) => v,
                    Err(err) => {
                        error!(error = %err, "control accept failed");
                        continue;
                    }
                };
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(err) = handle_agent(stream, state).await {
                        if !is_benign(&err) {
                            error!(error = %err, "agent session failed");
                        }
                    }
                });
            }
        }
    }
}

async fn poll_shared_once(state: &DaemonState, backend: &BackendConfig) {
    match fetch_devices_l(backend.addr, None).await {
        Ok(body) => {
            state
                .tenants
                .update_shared_from_lists(&[(backend.clone(), body)])
                .await;
        }
        Err(err) => {
            warn!(error = %err, "shared local device poll failed");
            state
                .tenants
                .update_shared_from_lists(&[(backend.clone(), String::new())])
                .await;
        }
    }
}

async fn bind_control(config: &DaemonConfig) -> io::Result<UnixListener> {
    if let Some(path) = &config.control_path {
        if path.exists() {
            let _ = std::fs::remove_file(path);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        return UnixListener::bind(path);
    }
    match abstract_bind(&config.control_abstract) {
        Ok(l) => Ok(l),
        Err(err) => {
            // Fallback filesystem socket for tests / restricted environments.
            let path = PathBuf::from("/tmp/adb-hubd.sock");
            if path.exists() {
                let _ = std::fs::remove_file(&path);
            }
            warn!(
                error = %err,
                path = %path.display(),
                "abstract control bind failed; using filesystem socket"
            );
            UnixListener::bind(&path)
        }
    }
}

fn abstract_bind(name: &str) -> io::Result<UnixListener> {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::net::UnixListener as StdUnixListener;

    let fd = unsafe {
        #[cfg(target_os = "linux")]
        {
            libc::socket(
                libc::AF_UNIX,
                libc::SOCK_STREAM | libc::SOCK_CLOEXEC,
                0,
            )
        }
        #[cfg(not(target_os = "linux"))]
        {
            libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0)
        }
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    let bytes = name.as_bytes();
    if bytes.len() + 1 >= addr.sun_path.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "abstract socket name too long",
        ));
    }
    addr.sun_path[0] = 0;
    for (i, b) in bytes.iter().enumerate() {
        addr.sun_path[i + 1] = *b as _;
    }
    let len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 1 + bytes.len()) as libc::socklen_t;

    use std::os::fd::AsRawFd;
    let rc = unsafe {
        libc::bind(
            owned.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::listen(owned.as_raw_fd(), 128) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let std = unsafe { StdUnixListener::from_raw_fd(owned.into_raw_fd()) };
    std.set_nonblocking(true)?;
    UnixListener::from_std(std)
}

async fn handle_agent(stream: UnixStream, state: Arc<DaemonState>) -> io::Result<()> {
    let cred = peer_cred_unix(&stream)?;
    let uid = cred.uid;
    info!(uid, pid = cred.pid, "agent connected");

    let (mut reader, writer) = tokio::io::split(stream);
    let writer = Arc::new(Mutex::new(writer));

    // First message must be RegisterAgent.
    let first = read_message(&mut reader).await?;
    let IpcMessage::RegisterAgent(reg) = first else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected RegisterAgent",
        ));
    };
    if reg.version != PROTOCOL_VERSION {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "unsupported agent protocol version {} (want {PROTOCOL_VERSION})",
                reg.version
            ),
        ));
    }

    let conn = Arc::new(AgentConn {
        uid,
        instance_token: reg.instance_token.clone(),
        writer: writer.clone(),
        stream_tx: Mutex::new(HashMap::new()),
        open_waiters: Mutex::new(HashMap::new()),
    });

    {
        let mut agents = state.agents.write().await;
        if let Some(old) = agents.insert(uid, conn.clone()) {
            info!(
                uid,
                old_token = %old.instance_token,
                new_token = %conn.instance_token,
                "replaced stale agent for uid"
            );
        }
    }

    let result = agent_read_loop(reader, conn.clone(), state.clone()).await;

    // Only remove if we are still the active agent for this uid.
    {
        let mut agents = state.agents.write().await;
        if agents
            .get(&uid)
            .map(|c| c.instance_token == conn.instance_token)
            .unwrap_or(false)
        {
            agents.remove(&uid);
            state.tenants.remove_agent(uid).await;
            info!(uid, "agent disconnected; private devices removed");
        }
    }

    result
}

async fn agent_read_loop(
    mut reader: tokio::io::ReadHalf<UnixStream>,
    conn: Arc<AgentConn>,
    state: Arc<DaemonState>,
) -> io::Result<()> {
    loop {
        let msg = read_message(&mut reader).await?;
        match msg {
            IpcMessage::DeviceSnapshot(m) | IpcMessage::DeviceSnapshotChanged(m) => {
                let snap = sanitized_to_snapshot(&m.devices);
                state.tenants.update_agent_devices(conn.uid, snap).await;
                debug!(
                    uid = conn.uid,
                    generation = m.generation,
                    devices = m.devices.len(),
                    "agent device snapshot"
                );
            }
            IpcMessage::OpenResult(r) => {
                if let Some(tx) = conn.open_waiters.lock().await.remove(&r.stream_id) {
                    let _ = tx.send(OpenOutcome {
                        ok: r.ok,
                        fail_reason: r.fail_reason,
                    });
                }
            }
            IpcMessage::StreamData { stream_id, data } => {
                if let Some(tx) = conn.stream_tx.lock().await.get(&stream_id) {
                    let _ = tx.send(data).await;
                }
            }
            IpcMessage::StreamClose(c) => {
                conn.stream_tx.lock().await.remove(&c.stream_id);
                if let Some(tx) = conn.open_waiters.lock().await.remove(&c.stream_id) {
                    let _ = tx.send(OpenOutcome {
                        ok: false,
                        fail_reason: Some(c.reason),
                    });
                }
            }
            IpcMessage::Ping => {
                write_message(&mut *conn.writer.lock().await, &IpcMessage::Pong).await?;
            }
            IpcMessage::Pong => {}
            other => {
                debug!(?other, uid = conn.uid, "daemon ignoring unexpected agent msg");
            }
        }
    }
}

async fn handle_adb_client(
    mut client: TcpStream,
    local: SocketAddr,
    peer: SocketAddr,
    state: Arc<DaemonState>,
    kill_notify: Arc<Notify>,
) -> io::Result<()> {
    let uid = match tcp_peer_uid(local, peer) {
        Ok(uid) => uid,
        Err(err) => {
            // Fail closed: never fall back to another tenant.
            warn!(%peer, error = %err, "UID resolution failed; rejecting client");
            write_fail(&mut client, "unable to identify client user").await?;
            return Ok(());
        }
    };
    debug!(uid, %peer, "ADB client identified");

    let payload = match read_packet(&mut client).await {
        Ok(p) => p,
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
        Err(err) => return Err(err),
    };
    let service = match String::from_utf8(payload) {
        Ok(s) => s,
        Err(_) => {
            write_fail(&mut client, "invalid utf8 service").await?;
            return Ok(());
        }
    };
    debug!(uid, service = %service, "host service");

    let snap = state.tenants.snapshot_for(uid).await;
    let backend_order = backend_order(&snap, state.local_backend.as_ref());

    if service == "host:devices" || service == "host:devices-l" {
        let long = service.ends_with("-l");
        let body = snap.format_devices(long);
        write_okay_payload(&mut client, body.as_bytes()).await?;
        return Ok(());
    }

    if service == "host:track-devices" || service == "host:track-devices-l" {
        let long = service.ends_with("-l");
        return track_devices_multi(&mut client, &state, uid, long).await;
    }

    if service == "host:kill" {
        // Do not terminate the common daemon or shared local ADB.
        write_okay(&mut client).await?;
        kill_notify.notify_waiters();
        return Ok(());
    }

    if service == "host:version" {
        let ver = state.adb_version.load(Ordering::Relaxed);
        let body = format!("{ver:04x}");
        write_okay_payload(&mut client, body.as_bytes()).await?;
        return Ok(());
    }

    if service == "host:get-state"
        || service == "host:get-serialno"
        || service == "host:get-connection-state"
    {
        match pick_preferred_device(&snap, &backend_order) {
            Ok(entry) => {
                let body = if service == "host:get-serialno" {
                    entry.public_serial.as_str()
                } else {
                    entry.state.as_str()
                };
                write_okay_payload(&mut client, body.as_bytes()).await?;
            }
            Err(reason) => write_fail(&mut client, &reason).await?,
        }
        return Ok(());
    }

    // Opaque host-global requests: only forward known-safe ones to shared local.
    if is_unsafe_global_host(&service) {
        write_fail(
            &mut client,
            "host command not permitted on multi-user daemon",
        )
        .await?;
        return Ok(());
    }

    match route_multi(&service, &snap, &backend_order, state.local_backend.as_ref()) {
        Ok(Route::Shared { addr, upstream }) => {
            forward_shared(&mut client, addr, &upstream).await
        }
        Ok(Route::Private {
            route_id,
            upstream,
        }) => forward_private(&mut client, &state, uid, route_id, upstream).await,
        Err(reason) => {
            write_fail(&mut client, &reason).await?;
            Ok(())
        }
    }
}

enum Route {
    Shared { addr: SocketAddr, upstream: String },
    Private { route_id: String, upstream: String },
}

fn route_multi(
    service: &str,
    snap: &DeviceSnapshot,
    backend_order: &[String],
    local: Option<&BackendConfig>,
) -> std::result::Result<Route, String> {
    // Device-scoped routing with public→upstream rewrite.
    if let Some((entry, upstream)) = rewrite_upstream_service(service, snap, backend_order)? {
        if let Some(route_id) = &entry.route_id {
            return Ok(Route::Private {
                route_id: route_id.clone(),
                upstream,
            });
        }
        return Ok(Route::Shared {
            addr: entry.backend_addr,
            upstream,
        });
    }

    // Default / features → shared local when available.
    if let Some(local) = local {
        return Ok(Route::Shared {
            addr: local.addr,
            upstream: service.to_string(),
        });
    }
    Err("no shared local backend".into())
}

fn backend_order(snap: &DeviceSnapshot, local: Option<&BackendConfig>) -> Vec<String> {
    let mut order = Vec::new();
    if local.is_some() {
        order.push(LocalAdb::backend_name().to_string());
    }
    for d in &snap.devices {
        if !order.contains(&d.backend_name) {
            order.push(d.backend_name.clone());
        }
    }
    order
}

fn is_unsafe_global_host(service: &str) -> bool {
    // Refuse opaque global mutations / server control beyond kill (handled above).
    matches!(
        service,
        "host:reconnect-offline" | "host:resetowninghost"
    ) || service.starts_with("host:kill:")
}

async fn forward_shared(client: &mut TcpStream, addr: SocketAddr, service: &str) -> io::Result<()> {
    let mut upstream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(err) => {
            write_fail(client, &format!("backend {addr}: {err}")).await?;
            return Ok(());
        }
    };
    write_service(&mut upstream, service).await?;
    match tokio::io::copy_bidirectional(client, &mut upstream).await {
        Ok(_) => Ok(()),
        Err(err) if is_benign(&err) => Ok(()),
        Err(err) => Err(err),
    }
}

async fn forward_private(
    client: &mut TcpStream,
    state: &DaemonState,
    uid: Uid,
    route_id: String,
    service: String,
) -> io::Result<()> {
    let agent = {
        let agents = state.agents.read().await;
        agents.get(&uid).cloned()
    };
    let Some(agent) = agent else {
        write_fail(client, "device not found").await?;
        return Ok(());
    };

    let stream_id = state.next_stream_id.fetch_add(1, Ordering::Relaxed);
    let (otx, orx) = tokio::sync::oneshot::channel();
    agent.open_waiters.lock().await.insert(stream_id, otx);

    write_message(
        &mut *agent.writer.lock().await,
        &IpcMessage::OpenPrivateStream(OpenPrivateStream {
            stream_id,
            route_id,
            service,
        }),
    )
    .await?;

    let outcome = tokio::time::timeout(Duration::from_secs(10), orx)
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "open private stream timeout"))?
        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "open waiter dropped"))?;

    if !outcome.ok {
        let reason = outcome
            .fail_reason
            .unwrap_or_else(|| "open failed".to_string());
        write_fail(client, &reason).await?;
        return Ok(());
    }

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
    agent.stream_tx.lock().await.insert(stream_id, tx);

    // Bidirectional: client ↔ agent StreamData
    let writer = agent.writer.clone();
    let mut client_buf = vec![0u8; 64 * 1024];
    loop {
        tokio::select! {
            n = client.read(&mut client_buf) => {
                match n {
                    Ok(0) => break,
                    Ok(n) => {
                        write_message(
                            &mut *writer.lock().await,
                            &IpcMessage::StreamData {
                                stream_id,
                                data: client_buf[..n].to_vec(),
                            },
                        ).await?;
                    }
                    Err(err) if is_benign(&err) => break,
                    Err(err) => return Err(err),
                }
            }
            data = rx.recv() => {
                match data {
                    Some(data) => {
                        if let Err(err) = client.write_all(&data).await {
                            if is_benign(&err) { break; }
                            return Err(err);
                        }
                    }
                    None => break,
                }
            }
        }
    }

    agent.stream_tx.lock().await.remove(&stream_id);
    let _ = write_message(
        &mut *writer.lock().await,
        &IpcMessage::StreamClose(StreamClose {
            stream_id,
            reason: "client closed".into(),
        }),
    )
    .await;
    Ok(())
}

async fn track_devices_multi(
    client: &mut TcpStream,
    state: &DaemonState,
    uid: Uid,
    long: bool,
) -> io::Result<()> {
    write_okay(client).await?;
    let mut rx = state.tenants.subscribe();
    let body = state.tenants.snapshot_for(uid).await.format_devices(long);
    write_packet(client, body.as_bytes()).await?;

    loop {
        match rx.recv().await {
            Ok(()) => {
                let body = state.tenants.snapshot_for(uid).await.format_devices(long);
                if let Err(err) = write_packet(client, body.as_bytes()).await {
                    if is_benign(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let body = state.tenants.snapshot_for(uid).await.format_devices(long);
                write_packet(client, body.as_bytes()).await?;
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
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
