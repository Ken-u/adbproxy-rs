//! Per-user agent: reads the caller's hub config, polls remote backends, and
//! proxies private streams for `adb-hubd` without ever sending pair codes.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use rand::Rng;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::auth::authenticate_stream;
use crate::backend::{fetch_devices_l, filter_compatible_backends};
use crate::config::{default_config_path, HubConfig, ReloadableHubPolicy};
use crate::ipc::{
    read_message, write_message, DeviceSnapshotMsg, IpcMessage, OpenResult, PROTOCOL_VERSION,
    RegisterAgent, SanitizedDevice, StreamClose, DEFAULT_CONTROL_ABSTRACT,
};
use crate::protocol::write_service;
use crate::registry::merge_device_lists;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("config error: {0}")]
    Config(String),

    #[error("{}", crate::peercred::multi_user_unsupported_reason())]
    UnsupportedPlatform,
}

pub type Result<T> = std::result::Result<T, AgentError>;

#[derive(Clone, Debug)]
pub struct AgentConfig {
    pub config_path: PathBuf,
    /// Abstract socket name (without leading NUL).
    pub control_abstract: String,
    /// Optional filesystem socket (tests / non-abstract).
    pub control_path: Option<PathBuf>,
    pub poll_interval: Duration,
    /// If set, use these backends instead of (or until) reloading from disk.
    /// Pair codes stay inside this process.
    pub inline_backends: Option<HubConfig>,
    /// How many times to retry control-socket connect while the daemon starts.
    pub connect_retries: u32,
    pub connect_retry_delay: Duration,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            config_path: default_config_path(),
            control_abstract: DEFAULT_CONTROL_ABSTRACT.into(),
            control_path: None,
            poll_interval: Duration::from_millis(1000),
            inline_backends: None,
            connect_retries: 50,
            connect_retry_delay: Duration::from_millis(100),
        }
    }
}

#[derive(Clone)]
struct RouteEntry {
    addr: std::net::SocketAddr,
    pair_code: Option<String>,
    #[allow(dead_code)]
    upstream_serial: String,
}

type SharedWriter = Arc<Mutex<WriteHalf<UnixStream>>>;

/// Cached proxy-version gate: fingerprint of backends → compatible names.
type CompatCache = Arc<Mutex<(String, std::collections::HashSet<String>)>>;

fn backends_fingerprint(backends: &[crate::config::BackendConfig]) -> String {
    backends
        .iter()
        .map(|b| {
            format!(
                "{}@{}#{}",
                b.name,
                b.addr,
                b.pair_code.as_deref().unwrap_or("")
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

pub async fn run_agent(config: AgentConfig) -> Result<()> {
    run_agent_with_shutdown(config, std::future::pending::<()>()).await
}

pub async fn run_agent_with_shutdown(
    config: AgentConfig,
    shutdown: impl std::future::Future<Output = ()>,
) -> Result<()> {
    if !crate::peercred::multi_user_supported() {
        return Err(AgentError::UnsupportedPlatform);
    }

    ensure_owner_only_config(&config.config_path);

    let stream = connect_control_with_retry(&config).await?;
    info!("connected to shared hub control socket");

    let (mut reader, writer) = tokio::io::split(stream);
    let writer: SharedWriter = Arc::new(Mutex::new(writer));

    write_message(
        &mut *writer.lock().await,
        &IpcMessage::RegisterAgent(RegisterAgent {
            version: PROTOCOL_VERSION,
            capabilities: vec!["devices".into(), "streams".into()],
            instance_token: random_token(),
        }),
    )
    .await?;

    let routes: Arc<Mutex<HashMap<String, RouteEntry>>> = Arc::new(Mutex::new(HashMap::new()));
    let streams: Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let compat_cache: CompatCache = Arc::new(Mutex::new((String::new(), Default::default())));

    let mut generation = 0u64;
    if let Err(err) = refresh_and_publish(
        &config,
        &routes,
        &writer,
        &compat_cache,
        &mut generation,
        false,
    )
    .await
    {
        warn!(error = %err, "initial agent snapshot failed");
    }

    let poll_config = config.clone();
    let poll_routes = routes.clone();
    let poll_writer = writer.clone();
    let poll_compat = compat_cache.clone();
    let poll_interval = config.poll_interval;
    let poller = tokio::spawn(async move {
        let mut generation = generation;
        loop {
            tokio::time::sleep(poll_interval).await;
            if let Err(err) = refresh_and_publish(
                &poll_config,
                &poll_routes,
                &poll_writer,
                &poll_compat,
                &mut generation,
                true,
            )
            .await
            {
                warn!(error = %err, "agent snapshot publish failed");
                break;
            }
        }
    });

    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                info!("agent shutdown");
                poller.abort();
                return Ok(());
            }
            msg = read_message(&mut reader) => {
                let msg = match msg {
                    Ok(m) => m,
                    Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
                        info!("daemon closed control connection");
                        poller.abort();
                        return Ok(());
                    }
                    Err(err) => {
                        poller.abort();
                        return Err(err.into());
                    }
                };
                if let Err(err) = handle_daemon_message(msg, &routes, &streams, &writer).await {
                    error!(error = %err, "agent failed handling daemon message");
                }
            }
        }
    }
}

async fn refresh_and_publish(
    config: &AgentConfig,
    routes: &Arc<Mutex<HashMap<String, RouteEntry>>>,
    writer: &SharedWriter,
    compat_cache: &CompatCache,
    generation: &mut u64,
    changed: bool,
) -> io::Result<()> {
    let mut hub_cfg = if let Some(inline) = &config.inline_backends {
        // Prefer live file when present so pair/unpair takes effect; else inline.
        if config.config_path.is_file() {
            HubConfig::load_file(&config.config_path)
                .unwrap_or_else(|_| inline.clone())
        } else {
            inline.clone()
        }
    } else if config.config_path.is_file() {
        HubConfig::load_file(&config.config_path)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?
    } else {
        HubConfig::local_only()
    };
    // Agents never own shared local ADB.
    hub_cfg.include_local = false;
    hub_cfg.backends.retain(|b| b.enabled);
    let policy = ReloadableHubPolicy::from_config(&hub_cfg);

    let enabled = hub_cfg.enabled_backends();
    let fp = backends_fingerprint(&enabled);
    let compatible_names = {
        let mut cache = compat_cache.lock().await;
        if cache.0 != fp {
            let compatible = filter_compatible_backends(&enabled).await;
            cache.0 = fp;
            cache.1 = compatible.into_iter().map(|b| b.name).collect();
        }
        cache.1.clone()
    };

    let mut lists = Vec::new();
    for backend in enabled {
        if !compatible_names.contains(&backend.name) {
            lists.push((backend, String::new()));
            continue;
        }
        match fetch_devices_l(backend.addr, backend.pair_code.as_deref()).await {
            Ok(body) => lists.push((backend, body)),
            Err(err) => {
                debug!(backend = %backend.name, error = %err, "agent backend poll failed");
                lists.push((backend, String::new()));
            }
        }
    }

    let snap = policy.refresh().filter_snapshot(merge_device_lists(&lists));
    let mut new_routes = HashMap::new();
    let mut sanitized = Vec::new();

    for d in &snap.devices {
        let route_id = format!("{}:{}", d.backend_name, d.upstream_serial);
        new_routes.insert(
            route_id.clone(),
            RouteEntry {
                addr: d.backend_addr,
                pair_code: d.pair_code.clone(),
                upstream_serial: d.upstream_serial.clone(),
            },
        );
        sanitized.push(SanitizedDevice {
            public_serial: d.public_serial.clone(),
            upstream_serial: d.upstream_serial.clone(),
            state: d.state.clone(),
            extras: d.extras.clone(),
            backend_name: d.backend_name.clone(),
            route_id,
        });
    }

    *routes.lock().await = new_routes;
    *generation += 1;
    let msg = DeviceSnapshotMsg {
        generation: *generation,
        devices: sanitized,
    };
    let ipc = if changed {
        IpcMessage::DeviceSnapshotChanged(msg)
    } else {
        IpcMessage::DeviceSnapshot(msg)
    };
    write_message(&mut *writer.lock().await, &ipc).await
}

async fn handle_daemon_message(
    msg: IpcMessage,
    routes: &Arc<Mutex<HashMap<String, RouteEntry>>>,
    streams: &Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    writer: &SharedWriter,
) -> io::Result<()> {
    match msg {
        IpcMessage::Ping => write_message(&mut *writer.lock().await, &IpcMessage::Pong).await,
        IpcMessage::Pong => Ok(()),
        IpcMessage::OpenPrivateStream(req) => {
            open_private_stream(req, routes, streams, writer).await
        }
        IpcMessage::StreamData { stream_id, data } => {
            if let Some(tx) = streams.lock().await.get(&stream_id) {
                let _ = tx.send(data).await;
            }
            Ok(())
        }
        IpcMessage::StreamClose(c) => {
            streams.lock().await.remove(&c.stream_id);
            Ok(())
        }
        other => {
            debug!(?other, "agent ignoring unexpected message");
            Ok(())
        }
    }
}

async fn open_private_stream(
    req: crate::ipc::OpenPrivateStream,
    routes: &Arc<Mutex<HashMap<String, RouteEntry>>>,
    streams: &Arc<Mutex<HashMap<u32, mpsc::Sender<Vec<u8>>>>>,
    writer: &SharedWriter,
) -> io::Result<()> {
    let route = routes.lock().await.get(&req.route_id).cloned();
    let Some(route) = route else {
        return write_message(
            &mut *writer.lock().await,
            &IpcMessage::OpenResult(OpenResult {
                stream_id: req.stream_id,
                ok: false,
                fail_reason: Some("unknown route_id".into()),
            }),
        )
        .await;
    };

    // Daemon already rewrote public→upstream serial into `req.service`.
    let mut upstream = match tokio::net::TcpStream::connect(route.addr).await {
        Ok(s) => s,
        Err(err) => {
            return write_message(
                &mut *writer.lock().await,
                &IpcMessage::OpenResult(OpenResult {
                    stream_id: req.stream_id,
                    ok: false,
                    fail_reason: Some(format!("connect: {err}")),
                }),
            )
            .await;
        }
    };

    if let Some(code) = &route.pair_code {
        if let Err(err) = authenticate_stream(&mut upstream, code).await {
            // Never echo the pair code.
            return write_message(
                &mut *writer.lock().await,
                &IpcMessage::OpenResult(OpenResult {
                    stream_id: req.stream_id,
                    ok: false,
                    fail_reason: Some(format!("auth failed: {err}")),
                }),
            )
            .await;
        }
    }

    write_service(&mut upstream, &req.service).await?;
    write_message(
        &mut *writer.lock().await,
        &IpcMessage::OpenResult(OpenResult {
            stream_id: req.stream_id,
            ok: true,
            fail_reason: None,
        }),
    )
    .await?;

    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
    streams.lock().await.insert(req.stream_id, tx);

    let stream_id = req.stream_id;
    let writer_up = writer.clone();
    let streams_up = streams.clone();
    tokio::spawn(async move {
        let (mut r, mut w) = upstream.into_split();
        let to_up = async {
            while let Some(data) = rx.recv().await {
                if w.write_all(&data).await.is_err() {
                    break;
                }
            }
        };
        let from_up = async {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match r.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let msg = IpcMessage::StreamData {
                            stream_id,
                            data: buf[..n].to_vec(),
                        };
                        if write_message(&mut *writer_up.lock().await, &msg)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        };
        tokio::select! {
            _ = to_up => {}
            _ = from_up => {}
        }
        streams_up.lock().await.remove(&stream_id);
        let _ = write_message(
            &mut *writer_up.lock().await,
            &IpcMessage::StreamClose(StreamClose {
                stream_id,
                reason: "closed".into(),
            }),
        )
        .await;
    });
    Ok(())
}

async fn connect_control_with_retry(config: &AgentConfig) -> io::Result<UnixStream> {
    let mut last_err = None;
    for attempt in 0..=config.connect_retries {
        match connect_control(config).await {
            Ok(s) => return Ok(s),
            Err(err) => {
                last_err = Some(err);
                if attempt < config.connect_retries {
                    tokio::time::sleep(config.connect_retry_delay).await;
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::ConnectionRefused, "control socket unavailable")
    }))
}

async fn connect_control(config: &AgentConfig) -> io::Result<UnixStream> {
    if let Some(path) = &config.control_path {
        return UnixStream::connect(path).await;
    }
    match abstract_connect(&config.control_abstract).await {
        Ok(s) => Ok(s),
        Err(err) => {
            // Fallback for environments where abstract sockets are awkward.
            match UnixStream::connect("/tmp/adb-hubd.sock").await {
                Ok(s) => Ok(s),
                Err(_) => Err(err),
            }
        }
    }
}

async fn abstract_connect(name: &str) -> io::Result<UnixStream> {
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::net::UnixStream as StdUnixStream;

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
        libc::connect(
            owned.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let std = unsafe { StdUnixStream::from_raw_fd(owned.into_raw_fd()) };
    std.set_nonblocking(true)?;
    UnixStream::from_std(std)
}

/// Best-effort: ensure config containing pair codes is owner-only (0600).
fn ensure_owner_only_config(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                let mut perms = meta.permissions();
                perms.set_mode(0o600);
                if std::fs::set_permissions(path, perms).is_ok() {
                    info!(
                        path = %path.display(),
                        "restricted config permissions to owner-only (0600)"
                    );
                }
            }
        }
    }
}

fn random_token() -> String {
    let mut rng = rand::thread_rng();
    (0..16)
        .map(|_| format!("{:02x}", rng.gen::<u8>()))
        .collect()
}
