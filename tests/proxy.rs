use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

use adb_proxy::auth::{authenticate_stream, auth_service};
use adb_proxy::protocol::{
    read_okay_payload, read_packet, read_status, write_okay_payload, write_service,
};
use adb_proxy::proxy_config::ProxyFileConfig;
use adb_proxy::{run_proxy_with_shutdown, wait_for_port, ProxyConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

fn temp_policy_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("adb-proxy-test-{name}-{}.toml", std::process::id()))
}

#[tokio::test]
async fn forwards_bytes_after_auth() {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    let upstream_task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = upstream_listener.accept().await.unwrap();
            let Ok(req) = read_packet(&mut socket).await else {
                continue;
            };
            assert_eq!(req, b"host:devices");
            write_okay_payload(&mut socket, b"ABC\tdevice\n").await.unwrap();
            break;
        }
    });

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener);

    let policy_path = temp_policy_path("forward");
    let _ = std::fs::remove_file(&policy_path);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let config = ProxyConfig {
        listen: proxy_addr,
        target: upstream_addr,
        pair_code: "ABCD1234".into(),
        policy_path: policy_path.clone(),
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
    let body = read_okay_payload(&mut client).await.unwrap();
    assert_eq!(body, b"ABC\tdevice\n");

    let _ = shutdown_tx.send(());
    proxy_task.await.unwrap().unwrap();
    upstream_task.await.unwrap();
    let _ = std::fs::remove_file(&policy_path);
}

#[tokio::test]
async fn filters_disabled_device_from_list_and_transport() {
    let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream_listener.local_addr().unwrap();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = upstream_listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let Ok(req) = read_packet(&mut socket).await else {
                    return;
                };
                let service = String::from_utf8_lossy(&req).into_owned();
                if service == "host:devices" {
                    let _ = write_okay_payload(
                        &mut socket,
                        b"KEEP\tdevice\nDROP\tdevice\n",
                    )
                    .await;
                    return;
                }
                if service.starts_with("host:transport:") {
                    let _ = write_okay_payload(&mut socket, b"should-not-reach").await;
                }
            });
        }
    });

    let policy_path = temp_policy_path("filter");
    let mut cfg = ProxyFileConfig::default();
    cfg.set_device_enabled("DROP", false);
    cfg.save_file(&policy_path).unwrap();

    let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_addr = proxy_listener.local_addr().unwrap();
    drop(proxy_listener);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let config = ProxyConfig {
        listen: proxy_addr,
        target: upstream_addr,
        pair_code: "ABCD1234".into(),
        policy_path: policy_path.clone(),
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

    {
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        authenticate_stream(&mut client, "ABCD1234").await.unwrap();
        write_service(&mut client, "host:devices").await.unwrap();
        let body = read_okay_payload(&mut client).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("KEEP\tdevice"), "got: {text}");
        assert!(!text.contains("DROP"), "got: {text}");
    }

    {
        let mut client = TcpStream::connect(proxy_addr).await.unwrap();
        authenticate_stream(&mut client, "ABCD1234").await.unwrap();
        write_service(&mut client, "host:transport:DROP").await.unwrap();
        let status = read_status(&mut client).await.unwrap();
        assert_eq!(&status, b"FAIL");
        let reason = read_packet(&mut client).await.unwrap();
        assert!(String::from_utf8_lossy(&reason).contains("disabled"));
    }

    let _ = shutdown_tx.send(());
    let _ = proxy_task.await;
    let _ = std::fs::remove_file(&policy_path);
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

    let policy_path = temp_policy_path("noauth");
    let _ = std::fs::remove_file(&policy_path);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let config = ProxyConfig {
        listen: proxy_addr,
        target: upstream_addr,
        pair_code: "ABCD1234".into(),
        policy_path,
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

    let policy_path = temp_policy_path("wrongcode");
    let _ = std::fs::remove_file(&policy_path);

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let config = ProxyConfig {
        listen: proxy_addr,
        target: upstream_addr,
        pair_code: "ABCD1234".into(),
        policy_path,
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
        policy_path: PathBuf::from("/tmp/adb-proxy-test.toml"),
    };

    assert_eq!(config.listen, listen);
    assert_eq!(config.target, target);
    assert_eq!(config.pair_code, "ABCD1234");
}
