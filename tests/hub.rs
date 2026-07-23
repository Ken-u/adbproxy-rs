use std::net::SocketAddr;
use std::time::Duration;

use adb_proxy::config::{BackendConfig, HubConfig};
use adb_proxy::hub::run_hub_with_shutdown;
use adb_proxy::protocol::{
    read_okay_payload, read_packet, read_status, write_fail, write_okay, write_okay_payload,
    write_packet, write_service,
};
use adb_proxy::wait_for_port;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// Mock upstream adb server: answers devices-l and transport:SERIAL then echoes.
async fn mock_backend(listener: TcpListener, serial: &'static str, extras: &'static str) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(async move {
            let Ok(req) = read_packet(&mut socket).await else {
                return;
            };
            let service = String::from_utf8_lossy(&req).into_owned();
            if service == "host:devices-l" {
                let body = if extras.is_empty() {
                    format!("{serial}\tdevice\n")
                } else {
                    format!("{serial}\tdevice {extras}\n")
                };
                let _ = write_okay_payload(&mut socket, body.as_bytes()).await;
                return;
            }
            if let Some(s) = service.strip_prefix("host:transport:") {
                if s == serial {
                    let _ = write_okay(&mut socket).await;
                    if let Ok(payload) = read_packet(&mut socket).await {
                        let _ = write_packet(&mut socket, &payload).await;
                    }
                } else {
                    let _ = write_fail(&mut socket, "unknown device").await;
                }
                return;
            }
            let _ = write_fail(&mut socket, "unsupported").await;
        });
    }
}

#[tokio::test]
async fn hub_lists_and_transports() {
    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    tokio::spawn(mock_backend(backend_listener, "SERIAL1", "product:test"));

    let hub_addr: SocketAddr = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };

    let config = HubConfig {
        listen: hub_addr,
        poll_interval: Duration::from_millis(100),
        backends: vec![BackendConfig {
            name: "mock".into(),
            addr: backend_addr,
        }],
        adb_version: 40,
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let hub = tokio::spawn(async move {
        run_hub_with_shutdown(config, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    wait_for_port(hub_addr, Duration::from_secs(2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;

    {
        let mut c = TcpStream::connect(hub_addr).await.unwrap();
        write_service(&mut c, "host:version").await.unwrap();
        let body = read_okay_payload(&mut c).await.unwrap();
        assert_eq!(body, b"0028");
    }

    {
        let mut c = TcpStream::connect(hub_addr).await.unwrap();
        write_service(&mut c, "host:devices").await.unwrap();
        let body = read_okay_payload(&mut c).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("SERIAL1\tdevice"), "got: {text}");
    }

    {
        let mut c = TcpStream::connect(hub_addr).await.unwrap();
        write_service(&mut c, "host:transport:SERIAL1").await.unwrap();
        let status = read_status(&mut c).await.unwrap();
        assert_eq!(&status, b"OKAY");
        write_packet(&mut c, b"shell:echo").await.unwrap();
        let echoed = read_packet(&mut c).await.unwrap();
        assert_eq!(echoed, b"shell:echo");
    }

    let _ = shutdown_tx.send(());
    let _ = hub.await;
}

#[tokio::test]
async fn hub_rewrites_conflicting_serials() {
    async fn serve(addr_out: oneshot::Sender<SocketAddr>, serial: &'static str) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _ = addr_out.send(listener.local_addr().unwrap());
        mock_backend(listener, serial, "").await;
    }

    let (tx_a, rx_a) = oneshot::channel();
    let (tx_b, rx_b) = oneshot::channel();
    tokio::spawn(serve(tx_a, "SAME"));
    tokio::spawn(serve(tx_b, "SAME"));
    let addr_a = rx_a.await.unwrap();
    let addr_b = rx_b.await.unwrap();

    let hub_addr: SocketAddr = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };

    let config = HubConfig {
        listen: hub_addr,
        poll_interval: Duration::from_millis(100),
        backends: vec![
            BackendConfig {
                name: "office".into(),
                addr: addr_a,
            },
            BackendConfig {
                name: "lab".into(),
                addr: addr_b,
            },
        ],
        adb_version: 40,
    };

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        run_hub_with_shutdown(config, async move {
            let _ = shutdown_rx.await;
        })
        .await
        .unwrap();
    });

    wait_for_port(hub_addr, Duration::from_secs(2)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;

    let mut c = TcpStream::connect(hub_addr).await.unwrap();
    write_service(&mut c, "host:devices").await.unwrap();
    let body = read_okay_payload(&mut c).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("office:SAME\tdevice"), "got: {text}");
    assert!(text.contains("lab:SAME\tdevice"), "got: {text}");

    let _ = shutdown_tx.send(());
}
