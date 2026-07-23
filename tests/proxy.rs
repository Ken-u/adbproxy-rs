use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use adb_proxy::auth::{authenticate_stream, auth_service};
use adb_proxy::protocol::{read_packet, read_status, write_service};
use adb_proxy::{run_proxy_with_shutdown, wait_for_port, ProxyConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

#[tokio::test]
async fn forwards_bytes_after_auth() {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    let upstream_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = upstream_listener.accept().await.unwrap();
            let mut buf = [0_u8; 64];
            let n = socket.read(&mut buf).await.unwrap();
            if n == 0 {
                continue;
            }

            // After auth, proxy forwards the raw ADB service packet from hub.
            assert_eq!(&buf[..n], b"000chost:devices");
            socket.write_all(b"OKAY").await.unwrap();
            socket.shutdown().await.unwrap();
            break;
        }
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let config = ProxyConfig {
        listen: proxy_addr,
        target: upstream_addr,
        pair_code: "ABCD1234".into(),
    };

    let proxy_task = tokio::spawn(async move {
        run_proxy_with_shutdown(config, async {
            let _ = shutdown_rx.await;
        })
        .await
    });

    wait_for_port(proxy_addr, Duration::from_secs(2))
        .await
        .unwrap();

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    authenticate_stream(&mut client, "ABCD1234").await.unwrap();
    write_service(&mut client, "host:devices").await.unwrap();
    client.shutdown().await.unwrap();

    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert_eq!(response, b"OKAY");

    let _ = shutdown_tx.send(());
    proxy_task.await.unwrap().unwrap();
    upstream_task.await.unwrap();
}

#[tokio::test]
async fn rejects_missing_auth() {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();
    // Upstream should never be contacted on auth failure.
    let upstream_task = tokio::spawn(async move {
        let _ = upstream_listener.accept().await;
        panic!("upstream should not accept without auth");
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let config = ProxyConfig {
        listen: proxy_addr,
        target: upstream_addr,
        pair_code: "ABCD1234".into(),
    };
    let proxy_task = tokio::spawn(async move {
        run_proxy_with_shutdown(config, async {
            let _ = shutdown_rx.await;
        })
        .await
    });
    wait_for_port(proxy_addr, Duration::from_secs(2))
        .await
        .unwrap();

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    // Send a normal ADB service instead of auth.
    write_service(&mut client, "host:devices").await.unwrap();
    let status = read_status(&mut client).await.unwrap();
    assert_eq!(&status, b"FAIL");
    let reason = read_packet(&mut client).await.unwrap();
    assert!(String::from_utf8_lossy(&reason).contains("auth"));

    let _ = shutdown_tx.send(());
    proxy_task.await.unwrap().unwrap();
    upstream_task.abort();
}

#[tokio::test]
async fn rejects_wrong_pair_code() {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let config = ProxyConfig {
        listen: proxy_addr,
        target: upstream_addr,
        pair_code: "ABCD1234".into(),
    };
    let proxy_task = tokio::spawn(async move {
        run_proxy_with_shutdown(config, async {
            let _ = shutdown_rx.await;
        })
        .await
    });
    wait_for_port(proxy_addr, Duration::from_secs(2))
        .await
        .unwrap();

    let mut client = TcpStream::connect(proxy_addr).await.unwrap();
    write_service(&mut client, &auth_service("ZZZZ9999")).await.unwrap();
    let status = read_status(&mut client).await.unwrap();
    assert_eq!(&status, b"FAIL");
    let reason = read_packet(&mut client).await.unwrap();
    assert_eq!(reason, b"unauthorized");

    let _ = shutdown_tx.send(());
    proxy_task.await.unwrap().unwrap();
    drop(upstream_listener);
}

#[tokio::test]
async fn proxy_config_accepts_socket_addresses() {
    let listen = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5038);
    let target = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 5037);

    let config = ProxyConfig {
        listen,
        target,
        pair_code: "ABCD1234".into(),
    };

    assert_eq!(config.listen, listen);
    assert_eq!(config.target, target);
    assert_eq!(config.pair_code, "ABCD1234");
}
