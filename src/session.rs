use std::io;
use std::sync::Arc;

use tokio::io::copy_bidirectional;
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tracing::{debug, warn};

use crate::backend::open_transport;
use crate::protocol::{
    read_packet, write_fail, write_okay, write_okay_payload, write_packet, write_service,
};
use crate::registry::DeviceRegistry;

pub struct SessionContext {
    pub registry: DeviceRegistry,
    pub adb_version: u32,
    pub kill_notify: Arc<Notify>,
}

/// Handle one adb client connection in host mode until disconnect or transport bind.
pub async fn handle_client(mut client: TcpStream, ctx: SessionContext) -> io::Result<()> {
    loop {
        let payload = match read_packet(&mut client).await {
            Ok(p) => p,
            Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(err) => return Err(err),
        };

        let service = match String::from_utf8(payload) {
            Ok(s) => s,
            Err(_) => {
                write_fail(&mut client, "invalid utf8 service").await?;
                continue;
            }
        };

        debug!(service = %service, "host service");

        if service == "host:version" {
            let ver = format!("{:04x}", ctx.adb_version);
            write_okay_payload(&mut client, ver.as_bytes()).await?;
            continue;
        }

        if service == "host:devices" || service == "host:devices-l" {
            let long = service.ends_with("-l");
            let body = ctx.registry.snapshot().await.format_devices(long);
            write_okay_payload(&mut client, body.as_bytes()).await?;
            continue;
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

        if let Some(serial) = service.strip_prefix("host:transport:") {
            return bind_transport(&mut client, &ctx, serial).await;
        }

        if service == "host:transport-any" {
            let snap = ctx.registry.snapshot().await;
            let online: Vec<_> = snap
                .devices
                .iter()
                .filter(|d| d.state == "device")
                .collect();
            match online.len() {
                0 => {
                    write_fail(&mut client, "no devices/emulators found").await?;
                    continue;
                }
                1 => {
                    return bind_transport(&mut client, &ctx, &online[0].public_serial).await;
                }
                _ => {
                    write_fail(&mut client, "more than one device/emulator").await?;
                    continue;
                }
            }
        }

        // host-serial:<serial>:<request> — rewrite serial if needed and forward.
        if let Some(rest) = service.strip_prefix("host-serial:") {
            return forward_host_serial(&mut client, &ctx, rest).await;
        }

        // host:<request> that is serial-scoped via host: prefix variants we don't implement.
        write_fail(
            &mut client,
            &format!("adb-hub: unsupported service '{service}'"),
        )
        .await?;
    }
}

async fn bind_transport(
    client: &mut TcpStream,
    ctx: &SessionContext,
    public_serial: &str,
) -> io::Result<()> {
    let snap = ctx.registry.snapshot().await;
    let Some(entry) = snap.find(public_serial) else {
        write_fail(client, &format!("device '{public_serial}' not found")).await?;
        return Ok(());
    };

    let upstream = match open_transport(entry.backend_addr, &entry.upstream_serial).await {
        Ok(s) => s,
        Err(err) => {
            write_fail(client, &err.to_string()).await?;
            return Ok(());
        }
    };

    write_okay(client).await?;
    debug!(
        serial = %public_serial,
        backend = %entry.backend_name,
        "transport bound; piping"
    );

    let mut upstream = upstream;
    match copy_bidirectional(client, &mut upstream).await {
        Ok(_) => Ok(()),
        Err(err) if is_benign(&err) => Ok(()),
        Err(err) => {
            warn!(error = %err, "transport pipe error");
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

    // Initial snapshot.
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

async fn forward_host_serial(
    client: &mut TcpStream,
    ctx: &SessionContext,
    rest: &str,
) -> io::Result<()> {
    // rest = "<serial>:<request...>"
    let Some((public_serial, request)) = rest.split_once(':') else {
        write_fail(client, "invalid host-serial service").await?;
        return Ok(());
    };

    let snap = ctx.registry.snapshot().await;
    let Some(entry) = snap.find(public_serial) else {
        write_fail(client, &format!("device '{public_serial}' not found")).await?;
        return Ok(());
    };

    let upstream_service = format!("host-serial:{}:{}", entry.upstream_serial, request);
    let mut upstream = TcpStream::connect(entry.backend_addr).await?;
    write_service(&mut upstream, &upstream_service).await?;

    // Client request already consumed; relay upstream response bytes as-is.
    match tokio::io::copy(&mut upstream, client).await {
        Ok(_) => Ok(()),
        Err(err) if is_benign(&err) => Ok(()),
        Err(err) => Err(err),
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
