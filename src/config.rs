use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackendConfig {
    pub name: String,
    pub addr: SocketAddr,
    /// Pair code for remote adb-proxy auth; None for local adb.
    pub pair_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HubConfig {
    pub listen: SocketAddr,
    pub poll_interval: Duration,
    pub backends: Vec<BackendConfig>,
    /// ADB server version reported by `host:version` (decimal, encoded as %04x).
    pub adb_version: u32,
    /// Start/reuse a real local adb server and aggregate it as backend `local`.
    pub include_local: bool,
    /// Side port for the real local adb server (hub keeps :5037).
    pub local_adb_port: u16,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("invalid config: {0}")]
    Invalid(String),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),
}

#[derive(Debug, Deserialize, Serialize)]
struct TomlFile {
    #[serde(default = "default_listen")]
    listen: String,
    #[serde(default = "default_poll_ms")]
    poll_interval_ms: u64,
    #[serde(default = "default_adb_version")]
    adb_version: u32,
    #[serde(default = "default_include_local")]
    include_local: bool,
    #[serde(default = "default_local_adb_port")]
    local_adb_port: u16,
    #[serde(default)]
    backend: Vec<TomlBackend>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TomlBackend {
    name: Option<String>,
    addr: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pair_code: Option<String>,
}

fn default_listen() -> String {
    "127.0.0.1:5037".to_string()
}

fn default_poll_ms() -> u64 {
    1000
}

fn default_adb_version() -> u32 {
    // Must match ADB_SERVER_VERSION of the installed adb client (1.0.41 → 41).
    // A mismatch makes the client try to kill/restart the server and appear to hang.
    41
}

fn default_include_local() -> bool {
    true
}

fn default_local_adb_port() -> u16 {
    5039
}

impl HubConfig {
    pub fn default_listen() -> SocketAddr {
        "127.0.0.1:5037".parse().expect("valid default listen")
    }

    pub fn from_toml_str(text: &str) -> Result<Self, ConfigError> {
        let parsed: TomlFile = toml::from_str(text)?;
        Self::from_toml_file(parsed)
    }

    fn from_toml_file(parsed: TomlFile) -> Result<Self, ConfigError> {
        let listen: SocketAddr = parsed
            .listen
            .parse()
            .map_err(|e| ConfigError::Invalid(format!("listen: {e}")))?;

        if parsed.local_adb_port == 0 {
            return Err(ConfigError::Invalid("local_adb_port must be non-zero".into()));
        }
        if listen.port() == parsed.local_adb_port {
            return Err(ConfigError::Invalid(
                "local_adb_port must differ from listen port".into(),
            ));
        }

        let mut backends = Vec::new();
        for (idx, b) in parsed.backend.into_iter().enumerate() {
            let addr: SocketAddr = b
                .addr
                .parse()
                .map_err(|e| ConfigError::Invalid(format!("backend[{idx}].addr: {e}")))?;
            let name = b.name.unwrap_or_else(|| default_backend_name(addr));
            if let Some(ref code) = b.pair_code {
                crate::auth::validate_pair_code(code)
                    .map_err(|e| ConfigError::Invalid(format!("backend[{idx}].pair_code: {e}")))?;
            }
            backends.push(BackendConfig {
                name,
                addr,
                pair_code: b.pair_code,
            });
        }

        if backends.is_empty() && !parsed.include_local {
            return Err(ConfigError::Invalid(
                "at least one [[backend]] is required when include_local = false".into(),
            ));
        }

        Ok(HubConfig {
            listen,
            poll_interval: Duration::from_millis(parsed.poll_interval_ms.max(100)),
            backends,
            adb_version: parsed.adb_version,
            include_local: parsed.include_local,
            local_adb_port: parsed.local_adb_port,
        })
    }

    pub fn load_file(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    /// Serialize current config to TOML text.
    pub fn to_toml_string(&self) -> Result<String, ConfigError> {
        let file = TomlFile {
            listen: self.listen.to_string(),
            poll_interval_ms: self.poll_interval.as_millis() as u64,
            adb_version: self.adb_version,
            include_local: self.include_local,
            local_adb_port: self.local_adb_port,
            backend: self
                .backends
                .iter()
                .map(|b| TomlBackend {
                    name: Some(b.name.clone()),
                    addr: b.addr.to_string(),
                    pair_code: b.pair_code.clone(),
                })
                .collect(),
        };
        Ok(toml::to_string_pretty(&file)?)
    }

    /// Write config atomically (temp file + rename).
    pub fn save_file(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = self.to_toml_string()?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Upsert a backend by name (preferred) or matching addr; used by `adb-hub pair`.
    pub fn upsert_backend(&mut self, backend: BackendConfig) {
        if let Some(existing) = self.backends.iter_mut().find(|b| b.name == backend.name) {
            existing.addr = backend.addr;
            existing.pair_code = backend.pair_code;
            return;
        }
        if let Some(existing) = self.backends.iter_mut().find(|b| b.addr == backend.addr) {
            existing.name = backend.name;
            existing.pair_code = backend.pair_code;
            return;
        }
        self.backends.push(backend);
    }

    /// Load legacy `~/.adbproxy` (`host=` / `port=` key=value).
    pub fn from_legacy_adbproxy(text: &str) -> Result<Self, ConfigError> {
        let mut host = None;
        let mut port = None;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            match key.trim() {
                "host" => host = Some(value.trim().to_string()),
                "port" => port = Some(value.trim().to_string()),
                _ => {}
            }
        }

        let host = host.ok_or_else(|| ConfigError::Invalid("legacy config missing host".into()))?;
        let port = port.ok_or_else(|| ConfigError::Invalid("legacy config missing port".into()))?;
        let addr: SocketAddr = format!("{host}:{port}")
            .parse()
            .map_err(|e| ConfigError::Invalid(format!("legacy host:port: {e}")))?;

        Ok(HubConfig {
            listen: Self::default_listen(),
            poll_interval: Duration::from_millis(default_poll_ms()),
            backends: vec![BackendConfig {
                name: default_backend_name(addr),
                addr,
                pair_code: None,
            }],
            adb_version: default_adb_version(),
            include_local: true,
            local_adb_port: default_local_adb_port(),
        })
    }

    pub fn load_legacy_file(path: &Path) -> Result<Self, ConfigError> {
        let text = fs::read_to_string(path)?;
        Self::from_legacy_adbproxy(&text)
    }

    /// Local-only defaults when no config / backends are provided.
    pub fn local_only() -> Self {
        Self {
            listen: Self::default_listen(),
            poll_interval: Duration::from_millis(default_poll_ms()),
            backends: Vec::new(),
            adb_version: default_adb_version(),
            include_local: true,
            local_adb_port: default_local_adb_port(),
        }
    }
}

pub fn default_backend_name(addr: SocketAddr) -> String {
    match addr {
        SocketAddr::V4(v4) => format!("{}_{}", v4.ip(), v4.port()),
        SocketAddr::V6(v6) => format!("{}_{}", v6.ip(), v6.port()).replace(':', "_"),
    }
}

pub fn default_config_path() -> PathBuf {
    dirs_next_home()
        .map(|h| h.join(".config/adb-hub/config.toml"))
        .unwrap_or_else(|| PathBuf::from("config.toml"))
}

pub fn legacy_config_path() -> PathBuf {
    dirs_next_home()
        .map(|h| h.join(".adbproxy"))
        .unwrap_or_else(|| PathBuf::from(".adbproxy"))
}

fn dirs_next_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Parse CLI `--backend name=host:port` or `host:port`.
pub fn parse_backend_arg(s: &str) -> Result<BackendConfig, ConfigError> {
    if let Some((name, addr_s)) = s.split_once('=') {
        let addr: SocketAddr = addr_s
            .parse()
            .map_err(|e| ConfigError::Invalid(format!("backend addr: {e}")))?;
        if name.is_empty() {
            return Err(ConfigError::Invalid("backend name must not be empty".into()));
        }
        Ok(BackendConfig {
            name: name.to_string(),
            addr,
            pair_code: None,
        })
    } else {
        let addr: SocketAddr = s
            .parse()
            .map_err(|e| ConfigError::Invalid(format!("backend addr: {e}")))?;
        Ok(BackendConfig {
            name: default_backend_name(addr),
            addr,
            pair_code: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_toml_backends() {
        let cfg = HubConfig::from_toml_str(
            r#"
listen = "127.0.0.1:5037"
[[backend]]
name = "office"
addr = "192.168.1.10:5038"
pair_code = "ABCD1234"
[[backend]]
addr = "10.0.0.2:5038"
"#,
        )
        .unwrap();
        assert_eq!(cfg.backends.len(), 2);
        assert_eq!(cfg.backends[0].name, "office");
        assert_eq!(cfg.backends[0].pair_code.as_deref(), Some("ABCD1234"));
        assert_eq!(cfg.backends[1].name, "10.0.0.2_5038");
        assert!(cfg.backends[1].pair_code.is_none());
        assert!(cfg.include_local);
        assert_eq!(cfg.local_adb_port, 5039);
    }

    #[test]
    fn parse_toml_local_only() {
        let cfg = HubConfig::from_toml_str(
            r#"
include_local = true
local_adb_port = 5040
"#,
        )
        .unwrap();
        assert!(cfg.backends.is_empty());
        assert_eq!(cfg.local_adb_port, 5040);
    }

    #[test]
    fn reject_empty_without_local() {
        let err = HubConfig::from_toml_str("include_local = false\n").unwrap_err();
        assert!(err.to_string().contains("at least one"));
    }

    #[test]
    fn parse_legacy() {
        let cfg = HubConfig::from_legacy_adbproxy("host=192.168.1.5\nport=5038\n").unwrap();
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].addr.to_string(), "192.168.1.5:5038");
        assert!(cfg.include_local);
    }

    #[test]
    fn parse_backend_cli() {
        let b = parse_backend_arg("lab=1.2.3.4:5038").unwrap();
        assert_eq!(b.name, "lab");
        assert_eq!(b.addr.to_string(), "1.2.3.4:5038");
        assert!(b.pair_code.is_none());
    }

    #[test]
    fn roundtrip_toml_with_pair_code() {
        let mut cfg = HubConfig::local_only();
        cfg.include_local = true;
        cfg.upsert_backend(BackendConfig {
            name: "office".into(),
            addr: "192.168.1.10:5038".parse().unwrap(),
            pair_code: Some("ABCD1234".into()),
        });
        let text = cfg.to_toml_string().unwrap();
        let loaded = HubConfig::from_toml_str(&text).unwrap();
        assert_eq!(loaded.backends[0].pair_code.as_deref(), Some("ABCD1234"));
    }

    #[test]
    fn upsert_updates_by_name() {
        let mut cfg = HubConfig::local_only();
        cfg.upsert_backend(BackendConfig {
            name: "office".into(),
            addr: "10.0.0.1:5038".parse().unwrap(),
            pair_code: Some("AAAAAAAA".into()),
        });
        cfg.upsert_backend(BackendConfig {
            name: "office".into(),
            addr: "10.0.0.2:5038".parse().unwrap(),
            pair_code: Some("BBBBBBBB".into()),
        });
        assert_eq!(cfg.backends.len(), 1);
        assert_eq!(cfg.backends[0].addr.to_string(), "10.0.0.2:5038");
        assert_eq!(cfg.backends[0].pair_code.as_deref(), Some("BBBBBBBB"));
    }
}
