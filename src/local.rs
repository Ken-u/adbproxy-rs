use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use tokio::net::TcpStream;
use tokio::time::sleep;
use tracing::{info, warn};

/// Manages a real `adb` server on a side port so adb-hub can own :5037.
pub struct LocalAdb {
    pub addr: SocketAddr,
    adb_path: PathBuf,
    started_by_us: bool,
}

impl LocalAdb {
    /// Free the default adb port (5037) if a server is listening there, then
    /// ensure a real adb server is running on `port`.
    pub async fn prepare(port: u16) -> io::Result<Self> {
        let adb_path = find_adb()?;

        // Hub needs :5037; stop the default adb server if present.
        let _ = Command::new(&adb_path).arg("kill-server").status();
        // Brief pause so the OS releases the port.
        sleep(Duration::from_millis(150)).await;

        let addr: SocketAddr = format!("127.0.0.1:{port}")
            .parse()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

        if tcp_ready(addr).await {
            info!(%addr, "reusing existing local adb server");
            return Ok(Self {
                addr,
                adb_path,
                started_by_us: false,
            });
        }

        info!(%addr, adb = %adb_path.display(), "starting local adb server");
        let status = Command::new(&adb_path)
            .args(["-P", &port.to_string(), "start-server"])
            .status()?;
        if !status.success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("adb -P {port} start-server failed with {status}"),
            ));
        }

        wait_until_ready(addr, Duration::from_secs(5)).await?;
        Ok(Self {
            addr,
            adb_path,
            started_by_us: true,
        })
    }

    pub fn backend_name() -> &'static str {
        "local"
    }
}

impl Drop for LocalAdb {
    fn drop(&mut self) {
        if !self.started_by_us {
            return;
        }
        let port = self.addr.port();
        match Command::new(&self.adb_path)
            .args(["-P", &port.to_string(), "kill-server"])
            .status()
        {
            Ok(status) if status.success() => {
                info!(port, "stopped local adb server started by adb-hub");
            }
            Ok(status) => {
                warn!(port, %status, "adb kill-server returned non-zero");
            }
            Err(err) => {
                warn!(port, error = %err, "failed to kill local adb server");
            }
        }
    }
}

fn find_adb() -> io::Result<PathBuf> {
    if let Ok(path) = std::env::var("ADB") {
        let p = PathBuf::from(path);
        if p.is_file() {
            return Ok(p);
        }
    }
    which("adb").ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "adb not found in PATH (set ADB=/path/to/adb)",
        )
    })
}

fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(bin);
        if candidate.is_file() {
            return Some(candidate);
        }
        #[cfg(windows)]
        {
            let exe = dir.join(format!("{bin}.exe"));
            if exe.is_file() {
                return Some(exe);
            }
        }
    }
    None
}

async fn tcp_ready(addr: SocketAddr) -> bool {
    TcpStream::connect(addr).await.is_ok()
}

async fn wait_until_ready(addr: SocketAddr, max_wait: Duration) -> io::Result<()> {
    let start = Instant::now();
    loop {
        if tcp_ready(addr).await {
            return Ok(());
        }
        if start.elapsed() >= max_wait {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("local adb server at {addr} did not become ready"),
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}
