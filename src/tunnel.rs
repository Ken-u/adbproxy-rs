//! Local listeners for scrcpy connections to remote ADB backends.
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{Ipv4Addr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{oneshot, Mutex};
use tokio::task::JoinHandle;
use tracing::debug;

use crate::protocol::{
    read_packet, read_status, write_fail, write_okay, write_packet, write_service,
};

pub type OpenStream =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send>> + Send + Sync>;

struct Listener {
    owner: Option<u32>,
    serial: String,
    stop: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}
impl Drop for Listener {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}
impl Listener {
    async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

#[derive(Default)]
pub struct TunnelManager {
    listeners: Mutex<HashMap<u16, Listener>>,
}

impl TunnelManager {
    pub async fn remove_owner(&self, owner: u32) {
        self.listeners
            .lock()
            .await
            .retain(|_, listener| listener.owner != Some(owner));
    }

    async fn add(
        &self,
        port: u16,
        serial: &str,
        socket: String,
        owner: Option<u32>,
        open: OpenStream,
    ) -> io::Result<u16> {
        let mut listeners = self.listeners.lock().await;
        if let Some(previous) = listeners.get(&port) {
            if previous.owner != owner || previous.serial != serial {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    "port already used by another device",
                ));
            }
            if let Some(previous) = listeners.remove(&port) {
                previous.stop().await;
            }
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await?;
        let actual_port = listener.local_addr()?.port();
        let (stop, mut stopped) = oneshot::channel();
        let task = tokio::spawn(async move {
            let mut draining = false;
            let mut previous = None;
            loop {
                let accepted = if draining {
                    std::future::poll_fn(|cx| std::task::Poll::Ready(listener.poll_accept(cx)))
                        .await
                } else {
                    tokio::select! {
                        biased;
                        _ = &mut stopped => { draining = true; continue; }
                        result = listener.accept() => std::task::Poll::Ready(result),
                    }
                };
                let (mut client, peer) = match accepted {
                    std::task::Poll::Ready(Ok(connection)) => connection,
                    _ => break,
                };
                #[cfg(target_os = "linux")]
                if let Some(uid) = owner {
                    let allowed = listener
                        .local_addr()
                        .ok()
                        .and_then(|local| crate::peercred::tcp_peer_uid(local, peer).ok())
                        == Some(uid);
                    if !allowed {
                        continue;
                    }
                }
                #[cfg(not(target_os = "linux"))]
                let _ = (owner, peer);
                // Preserve scrcpy's socket order while accepting all queued
                // connections before it removes the forward listener.
                let wait_for_previous: Option<oneshot::Receiver<()>> = previous.take();
                let (done, finished) = oneshot::channel();
                previous = Some(finished);
                let open = open.clone();
                let socket = socket.clone();
                tokio::spawn(async move {
                    if let Some(previous) = wait_for_previous {
                        let _ = previous.await;
                    }
                    let connected =
                        tokio::time::timeout(std::time::Duration::from_secs(10), async {
                            let mut upstream = open().await?;
                            if &read_status(&mut upstream).await? != b"OKAY" {
                                return Err(io::Error::other("device transport failed"));
                            }
                            write_service(&mut upstream, &socket).await?;
                            if &read_status(&mut upstream).await? != b"OKAY" {
                                return Err(io::Error::other("device socket failed"));
                            }
                            Ok::<_, io::Error>(upstream)
                        })
                        .await;
                    let _ = done.send(());
                    let mut upstream = match connected {
                        Ok(Ok(stream)) => stream,
                        Ok(Err(err)) => {
                            debug!(error = %err, "scrcpy tunnel connection failed");
                            return;
                        }
                        Err(_) => {
                            debug!("scrcpy tunnel connection timed out");
                            return;
                        }
                    };
                    if let Err(err) =
                        tokio::io::copy_bidirectional(&mut client, &mut upstream).await
                    {
                        debug!(error = %err, "scrcpy tunnel closed");
                    }
                });
            }
        });
        listeners.insert(
            actual_port,
            Listener {
                owner,
                serial: serial.to_string(),
                stop: Some(stop),
                task: Some(task),
            },
        );
        Ok(actual_port)
    }

    async fn remove(&self, port: u16, serial: &str, owner: Option<u32>) -> bool {
        let mut listeners = self.listeners.lock().await;
        if listeners
            .get(&port)
            .is_some_and(|v| v.owner == owner && v.serial == serial)
        {
            if let Some(listener) = listeners.remove(&port) {
                listener.stop().await;
            }
            true
        } else {
            false
        }
    }
}

/// Forward a device selection, intercepting scrcpy tunnel setup for remote devices.
pub async fn transport(
    client: &mut TcpStream,
    mut upstream: TcpStream,
    service: &str,
    serial: &str,
    owner: Option<u32>,
    manager: Arc<TunnelManager>,
    open: OpenStream,
) -> io::Result<()> {
    let status = read_status(&mut upstream).await?;
    client.write_all(&status).await?;
    if &status != b"OKAY" {
        if &status == b"FAIL" {
            let reason = read_packet(&mut upstream).await?;
            write_packet(client, &reason).await?;
        }
        return Ok(());
    }
    if service.starts_with("host:tport:") {
        let mut id = [0; 8];
        upstream.read_exact(&mut id).await?;
        client.write_all(&id).await?;
    }
    let request = read_packet(client).await?;
    let request_str = std::str::from_utf8(&request).unwrap_or("");
    if request_str.starts_with("reverse:forward:localabstract:scrcpy_") {
        return write_fail(
            client,
            "remote scrcpy reverse requires a tunnel; use adb forward",
        )
        .await;
    }
    if let Some(rest) = request_str.strip_prefix("host:forward:tcp:") {
        if let Some((port, socket)) = rest.split_once(';') {
            if socket.starts_with("localabstract:scrcpy_") {
                if let Ok(port) = port.parse::<u16>() {
                    match manager
                        .add(port, serial, socket.to_string(), owner, open)
                        .await
                    {
                        Ok(actual) => {
                            write_okay(client).await?;
                            write_okay(client).await?;
                            if port == 0 {
                                write_packet(client, actual.to_string().as_bytes()).await?;
                            }
                        }
                        Err(err) => {
                            write_fail(client, &err.to_string()).await?;
                        }
                    }
                    return Ok(());
                }
            }
        }
    }
    if let Some(port) = request_str
        .strip_prefix("host:killforward:tcp:")
        .and_then(|s| s.parse::<u16>().ok())
    {
        if manager.remove(port, serial, owner).await {
            write_okay(client).await?;
            return write_okay(client).await;
        }
    }
    write_packet(&mut upstream, &request).await?;
    tokio::io::copy_bidirectional(client, &mut upstream).await?;
    Ok(())
}

pub fn is_transport(service: &str) -> bool {
    service.starts_with("host:tport:") || service.starts_with("host:transport:")
}

pub async fn connect_backend(
    addr: SocketAddr,
    code: Option<String>,
    service: String,
) -> io::Result<TcpStream> {
    let mut stream = TcpStream::connect(addr).await?;
    if let Some(code) = code {
        crate::auth::authenticate_stream(&mut stream, &code).await?;
    }
    write_service(&mut stream, &service).await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test]
    async fn preserves_scrcpy_connection_order_when_first_handshake_is_slow() {
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = backend.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let connected = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = backend.accept().await.unwrap();
                let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                let connected = connected.clone();
                tokio::spawn(async move {
                    if n == 1 {
                        tokio::time::sleep(Duration::from_millis(150)).await;
                    }
                    write_okay(&mut stream).await.unwrap();
                    let _ = read_packet(&mut stream).await.unwrap();
                    let device_order = connected.fetch_add(1, Ordering::SeqCst) + 1;
                    write_okay(&mut stream).await.unwrap();
                    stream.write_all(&[device_order as u8]).await.unwrap();
                });
            }
        });
        let manager = TunnelManager::default();
        let open: OpenStream = Arc::new(move || Box::pin(TcpStream::connect(addr)));
        let port = manager
            .add(0, "serial", "localabstract:scrcpy_test".into(), None, open)
            .await
            .unwrap();
        let mut first = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let mut second = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        // scrcpy removes the forward immediately after opening its sockets.
        // Both connections must remain usable even if still in the backlog.
        assert!(manager.remove(port, "serial", None).await);
        let mut a = [0; 1];
        let mut b = [0; 1];
        tokio::time::timeout(Duration::from_secs(3), async {
            first.read_exact(&mut a).await.unwrap();
            second.read_exact(&mut b).await.unwrap();
        })
        .await
        .unwrap();
        assert_eq!((a, b), ([1], [2]));
        server.abort();
    }
}
