use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::protocol::{
    read_packet, write_fail, write_okay, write_okay_payload, write_packet, write_service,
};
use crate::registry::{DeviceEntry, DeviceRegistry, DeviceSnapshot};

pub struct SessionContext {
    pub registry: DeviceRegistry,
    /// Backend used for host services without a device serial (features, version, …).
    pub default_backend: SocketAddr,
    pub kill_notify: Arc<Notify>,
}

/// Handle one adb client connection.
///
/// Policy:
/// - `host:devices` / `track-devices` → answer from aggregated registry
/// - `host:kill` → stop the hub
/// - services that name a serial → rewrite serial if needed, forward whole session
/// - `tport:any` / `transport-any` → prefer local backend, else first backend with a device
/// - everything else → forward whole session to the default backend
pub async fn handle_client(mut client: TcpStream, ctx: SessionContext) -> io::Result<()> {
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

    debug!(service = %service, "host service");

    if service == "host:devices" || service == "host:devices-l" {
        let long = service.ends_with("-l");
        let body = ctx.registry.snapshot().await.format_devices(long);
        write_okay_payload(&mut client, body.as_bytes()).await?;
        return Ok(());
    }

    if service == "host:track-devices" || service == "host:track-devices-l" {
        let long = service.ends_with("-l");
        return track_devices(&mut client, &ctx.registry, long).await;
    }

    if service == "host:kill" {
        write_okay(&mut client).await?;
        ctx.kill_notify.notify_waiters();
        return Ok(());
    }

    let snap = ctx.registry.snapshot().await;
    match route_service(&service, &snap, ctx.default_backend) {
        Ok((addr, upstream_service)) => {
            debug!(%addr, service = %upstream_service, "forwarding to backend");
            forward_session(&mut client, addr, &upstream_service).await
        }
        Err(reason) => {
            write_fail(&mut client, &reason).await?;
            Ok(())
        }
    }
}

/// Decide which backend gets this service and what service string to send upstream.
fn route_service(
    service: &str,
    snap: &DeviceSnapshot,
    default_backend: SocketAddr,
) -> Result<(SocketAddr, String), String> {
    // host:transport:SERIAL
    if let Some(serial) = service.strip_prefix("host:transport:") {
        let entry = lookup_online(snap, serial)?;
        return Ok((
            entry.backend_addr,
            format!("host:transport:{}", entry.upstream_serial),
        ));
    }

    // host:tport:serial:SERIAL
    if let Some(serial) = service.strip_prefix("host:tport:serial:") {
        let entry = lookup_online(snap, serial)?;
        return Ok((
            entry.backend_addr,
            format!("host:tport:serial:{}", entry.upstream_serial),
        ));
    }

    // host:tport:any / host:transport-any
    if service == "host:tport:any" || service == "host:transport-any" {
        let entry = pick_preferred(snap)?;
        let upstream = if service == "host:tport:any" {
            format!("host:tport:serial:{}", entry.upstream_serial)
        } else {
            format!("host:transport:{}", entry.upstream_serial)
        };
        return Ok((entry.backend_addr, upstream));
    }

    // host-serial:SERIAL:request…
    if let Some(rest) = service.strip_prefix("host-serial:") {
        let Some((serial, request)) = rest.split_once(':') else {
            return Err("invalid host-serial service".into());
        };
        let entry = lookup_device(snap, serial)?;
        return Ok((
            entry.backend_addr,
            format!("host-serial:{}:{}", entry.upstream_serial, request),
        ));
    }

    // host:transport-usb / host:transport-local → same preference as "any"
    if service == "host:transport-usb" || service == "host:transport-local" {
        let entry = pick_preferred(snap)?;
        return Ok((
            entry.backend_addr,
            format!("host:transport:{}", entry.upstream_serial),
        ));
    }

    // Default: opaque forward to the default backend (local adb or first remote).
    Ok((default_backend, service.to_string()))
}

fn lookup_device<'a>(snap: &'a DeviceSnapshot, public_serial: &str) -> Result<&'a DeviceEntry, String> {
    snap.find(public_serial)
        .ok_or_else(|| format!("device '{public_serial}' not found"))
}

fn lookup_online<'a>(snap: &'a DeviceSnapshot, public_serial: &str) -> Result<&'a DeviceEntry, String> {
    let entry = lookup_device(snap, public_serial)?;
    if entry.state != "device" {
        return Err(format!("device '{public_serial}' is not online ({})", entry.state));
    }
    Ok(entry)
}

/// Pick a device when the client did not pass `-s`.
///
/// 1. Prefer online devices on the `local` backend.
/// 2. Otherwise use the first backend (registry order) that has online devices.
/// 3. Within the chosen backend, exactly one online device is required.
fn pick_preferred(snap: &DeviceSnapshot) -> Result<&DeviceEntry, String> {
    const LOCAL: &str = "local";

    match pick_one_on_backend(snap, LOCAL) {
        Ok(entry) => return Ok(entry),
        Err(PickBackendErr::None) => {}
        Err(PickBackendErr::Many) => {
            return Err("more than one device/emulator".into());
        }
    }

    // Preserve first-seen backend order from the merged device list.
    let mut backend_order: Vec<&str> = Vec::new();
    for d in &snap.devices {
        if d.backend_name == LOCAL {
            continue;
        }
        if !backend_order.contains(&d.backend_name.as_str()) {
            backend_order.push(d.backend_name.as_str());
        }
    }

    for name in backend_order {
        match pick_one_on_backend(snap, name) {
            Ok(entry) => return Ok(entry),
            Err(PickBackendErr::None) => continue,
            Err(PickBackendErr::Many) => {
                return Err("more than one device/emulator".into());
            }
        }
    }

    Err("no devices/emulators found".into())
}

enum PickBackendErr {
    None,
    Many,
}

fn pick_one_on_backend<'a>(
    snap: &'a DeviceSnapshot,
    backend_name: &str,
) -> Result<&'a DeviceEntry, PickBackendErr> {
    let online: Vec<_> = snap
        .devices
        .iter()
        .filter(|d| d.backend_name == backend_name && d.state == "device")
        .collect();
    match online.len() {
        0 => Err(PickBackendErr::None),
        1 => Ok(online[0]),
        _ => Err(PickBackendErr::Many),
    }
}

async fn forward_session(
    client: &mut TcpStream,
    addr: SocketAddr,
    service: &str,
) -> io::Result<()> {
    let mut upstream = match TcpStream::connect(addr).await {
        Ok(s) => s,
        Err(err) => {
            write_fail(client, &format!("backend {addr}: {err}")).await?;
            return Ok(());
        }
    };
    write_service(&mut upstream, service).await?;
    match copy_bidirectional(client, &mut upstream).await {
        Ok(_) => Ok(()),
        Err(err) if is_benign(&err) => Ok(()),
        Err(err) => {
            warn!(%addr, service, error = %err, "forward pipe error");
            Err(err)
        }
    }
}

async fn track_devices(
    client: &mut TcpStream,
    registry: &DeviceRegistry,
    long: bool,
) -> io::Result<()> {
    write_okay(client).await?;
    let mut rx = registry.subscribe();

    let body = registry.snapshot().await.format_devices(long);
    write_packet(client, body.as_bytes()).await?;

    loop {
        match rx.recv().await {
            Ok(()) => {
                let body = registry.snapshot().await.format_devices(long);
                if let Err(err) = write_packet(client, body.as_bytes()).await {
                    if is_benign(&err) {
                        return Ok(());
                    }
                    return Err(err);
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                let body = registry.snapshot().await.format_devices(long);
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

#[cfg(test)]
mod route_tests {
    use super::*;
    use crate::registry::DeviceEntry;

    fn snap_one() -> DeviceSnapshot {
        DeviceSnapshot {
            devices: vec![DeviceEntry {
                public_serial: "office:ABC".into(),
                upstream_serial: "ABC".into(),
                state: "device".into(),
                extras: String::new(),
                backend_name: "office".into(),
                backend_addr: "10.0.0.1:5038".parse().unwrap(),
            }],
        }
    }

    #[test]
    fn rewrites_transport_serial() {
        let default: SocketAddr = "127.0.0.1:5039".parse().unwrap();
        let (addr, svc) =
            route_service("host:transport:office:ABC", &snap_one(), default).unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:5038");
        assert_eq!(svc, "host:transport:ABC");
    }

    #[test]
    fn rewrites_tport_serial() {
        let default: SocketAddr = "127.0.0.1:5039".parse().unwrap();
        let (addr, svc) =
            route_service("host:tport:serial:office:ABC", &snap_one(), default).unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:5038");
        assert_eq!(svc, "host:tport:serial:ABC");
    }

    #[test]
    fn tport_any_becomes_serial() {
        let default: SocketAddr = "127.0.0.1:5039".parse().unwrap();
        let (addr, svc) = route_service("host:tport:any", &snap_one(), default).unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:5038");
        assert_eq!(svc, "host:tport:serial:ABC");
    }

    #[test]
    fn tport_any_prefers_local_even_if_remotes_exist() {
        let snap = DeviceSnapshot {
            devices: vec![
                DeviceEntry {
                    public_serial: "LOCAL1".into(),
                    upstream_serial: "LOCAL1".into(),
                    state: "device".into(),
                    extras: String::new(),
                    backend_name: "local".into(),
                    backend_addr: "127.0.0.1:5039".parse().unwrap(),
                },
                DeviceEntry {
                    public_serial: "REMOTE1".into(),
                    upstream_serial: "REMOTE1".into(),
                    state: "device".into(),
                    extras: String::new(),
                    backend_name: "office".into(),
                    backend_addr: "10.0.0.1:5038".parse().unwrap(),
                },
            ],
        };
        let default: SocketAddr = "127.0.0.1:5039".parse().unwrap();
        let (addr, svc) = route_service("host:tport:any", &snap, default).unwrap();
        assert_eq!(addr.to_string(), "127.0.0.1:5039");
        assert_eq!(svc, "host:tport:serial:LOCAL1");
    }

    #[test]
    fn tport_any_falls_back_to_first_remote_with_device() {
        let snap = DeviceSnapshot {
            devices: vec![
                DeviceEntry {
                    public_serial: "REMOTE1".into(),
                    upstream_serial: "REMOTE1".into(),
                    state: "device".into(),
                    extras: String::new(),
                    backend_name: "office".into(),
                    backend_addr: "10.0.0.1:5038".parse().unwrap(),
                },
                DeviceEntry {
                    public_serial: "REMOTE2".into(),
                    upstream_serial: "REMOTE2".into(),
                    state: "device".into(),
                    extras: String::new(),
                    backend_name: "lab".into(),
                    backend_addr: "10.0.0.2:5038".parse().unwrap(),
                },
            ],
        };
        let default: SocketAddr = "127.0.0.1:5039".parse().unwrap();
        let (addr, svc) = route_service("host:transport-any", &snap, default).unwrap();
        assert_eq!(addr.to_string(), "10.0.0.1:5038");
        assert_eq!(svc, "host:transport:REMOTE1");
    }

    #[test]
    fn tport_any_errors_when_local_has_many() {
        let snap = DeviceSnapshot {
            devices: vec![
                DeviceEntry {
                    public_serial: "L1".into(),
                    upstream_serial: "L1".into(),
                    state: "device".into(),
                    extras: String::new(),
                    backend_name: "local".into(),
                    backend_addr: "127.0.0.1:5039".parse().unwrap(),
                },
                DeviceEntry {
                    public_serial: "L2".into(),
                    upstream_serial: "L2".into(),
                    state: "device".into(),
                    extras: String::new(),
                    backend_name: "local".into(),
                    backend_addr: "127.0.0.1:5039".parse().unwrap(),
                },
            ],
        };
        let default: SocketAddr = "127.0.0.1:5039".parse().unwrap();
        let err = route_service("host:tport:any", &snap, default).unwrap_err();
        assert!(err.contains("more than one"));
    }

    #[test]
    fn features_goes_to_default() {
        let default: SocketAddr = "127.0.0.1:5039".parse().unwrap();
        let (addr, svc) = route_service("host:features", &snap_one(), default).unwrap();
        assert_eq!(addr, default);
        assert_eq!(svc, "host:features");
    }
}
