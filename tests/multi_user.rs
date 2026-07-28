//! Multi-user daemon/agent smoke tests (Unix control socket; Linux UID path).

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use adb_proxy::agent::{run_agent_with_shutdown, AgentConfig};
use adb_proxy::config::{BackendConfig, HubConfig};
use adb_proxy::daemon::{run_daemon_with_shutdown, DaemonConfig};
use adb_proxy::peercred::multi_user_supported;
use adb_proxy::policy::DevicePolicyTable;
use adb_proxy::protocol::{
    read_okay_payload, write_fail, write_okay, write_okay_payload, write_packet, write_service,
};
use adb_proxy::wait_for_port;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "adb-hubd-test-{}-{}-{}.sock",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn temp_config(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "adb-hub-agent-{}-{}-{}.toml",
        name,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

async fn mock_backend(listener: TcpListener, serial: &'static str, pair: Option<&'static str>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            break;
        };
        tokio::spawn(async move {
            let Ok(req) = adb_proxy::protocol::read_packet(&mut socket).await else {
                return;
            };
            let mut service = String::from_utf8_lossy(&req).into_owned();
            if let Some(code) = pair {
                if service != format!("auth:{code}") {
                    let _ = write_fail(&mut socket, "unauthorized").await;
                    return;
                }
                let _ = write_okay(&mut socket).await;
                let Ok(req2) = adb_proxy::protocol::read_packet(&mut socket).await else {
                    return;
                };
                service = String::from_utf8_lossy(&req2).into_owned();
            }
            if service == "proxy:version" {
                let _ = write_okay_payload(&mut socket, adb_proxy::VERSION.as_bytes()).await;
            } else if service == "host:devices-l" {
                let body = format!("{serial}\tdevice\n");
                let _ = write_okay_payload(&mut socket, body.as_bytes()).await;
            } else if let Some(s) = service.strip_prefix("host:transport:") {
                if s == serial {
                    let _ = write_okay(&mut socket).await;
                    if let Ok(payload) = adb_proxy::protocol::read_packet(&mut socket).await {
                        let _ = write_packet(&mut socket, &payload).await;
                    }
                } else {
                    let _ = write_fail(&mut socket, "unknown").await;
                }
            } else {
                let _ = write_fail(&mut socket, "unsupported").await;
            }
        });
    }
}

#[tokio::test]
async fn agent_publishes_private_devices_to_daemon() {
    if !multi_user_supported() {
        // Control-plane pieces still exercise on non-Linux Unix via filesystem
        // sockets, but ADB UID routing is Linux-only — skip end-to-end here.
        eprintln!("skip: multi-user ADB UID routing unsupported on this OS");
        return;
    }

    let backend_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    tokio::spawn(mock_backend(backend_listener, "PRIV1", Some("ABCD1234")));

    let control_path = temp_path("control");
    let _ = std::fs::remove_file(&control_path);

    let listen = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        a
    };

    let (d_tx, d_rx) = oneshot::channel::<()>();
    let daemon_cfg = DaemonConfig {
        listen,
        local_adb_port: 5039,
        include_local: false,
        control_abstract: format!("unused-{}", std::process::id()),
        control_path: Some(control_path.clone()),
        poll_interval: Duration::from_millis(200),
        adb_version: 41,
    };
    let daemon = tokio::spawn(async move {
        run_daemon_with_shutdown(daemon_cfg, async move {
            let _ = d_rx.await;
        })
        .await
        .unwrap();
    });

    // Wait for control socket.
    for _ in 0..50 {
        if control_path.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(control_path.exists(), "control socket missing");

    let config_path = temp_config("agent");
    let hub_cfg = HubConfig {
        listen: "127.0.0.1:5037".parse().unwrap(),
        poll_interval: Duration::from_millis(200),
        backends: vec![BackendConfig {
            name: "office".into(),
            addr: backend_addr,
            pair_code: Some("ABCD1234".into()),
            enabled: true,
        }],
        adb_version: 41,
        include_local: false,
        local_adb_port: 5039,
        devices: DevicePolicyTable::default(),
        config_path: Some(config_path.clone()),
    };
    hub_cfg.save_file(&config_path).unwrap();

    let (a_tx, a_rx) = oneshot::channel::<()>();
    let agent_cfg = AgentConfig {
        config_path: config_path.clone(),
        control_abstract: "unused".into(),
        control_path: Some(control_path.clone()),
        poll_interval: Duration::from_millis(200),
        ..AgentConfig::default()
    };
    let agent = tokio::spawn(async move {
        run_agent_with_shutdown(agent_cfg, async move {
            let _ = a_rx.await;
        })
        .await
        .unwrap();
    });

    wait_for_port(listen, Duration::from_secs(3)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut c = TcpStream::connect(listen).await.unwrap();
    write_service(&mut c, "host:devices").await.unwrap();
    let body = read_okay_payload(&mut c).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("PRIV1\tdevice"),
        "expected private device in list, got: {text}"
    );

    let _ = a_tx.send(());
    let _ = d_tx.send(());
    let _ = agent.await;
    let _ = daemon.await;
    let _ = std::fs::remove_file(&control_path);
    let _ = std::fs::remove_file(&config_path);
}
