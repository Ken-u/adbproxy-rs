use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::policy::{DevicePolicyEntry, DevicePolicyTable};

#[derive(Debug, Error)]
pub enum ProxyConfigError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("toml parse error: {0}")]
    Toml(#[from] toml::de::Error),

    #[error("toml serialize error: {0}")]
    TomlSer(#[from] toml::ser::Error),
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct TomlFile {
    #[serde(default)]
    device: Vec<DevicePolicyEntry>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ProxyFileConfig {
    pub devices: DevicePolicyTable,
}

impl ProxyFileConfig {
    pub fn from_toml_str(text: &str) -> Result<Self, ProxyConfigError> {
        let parsed: TomlFile = toml::from_str(text)?;
        Ok(Self {
            devices: DevicePolicyTable::from_entries(parsed.device),
        })
    }

    pub fn load_file(path: &Path) -> Result<Self, ProxyConfigError> {
        if !path.is_file() {
            return Ok(Self::default());
        }
        let text = fs::read_to_string(path)?;
        Self::from_toml_str(&text)
    }

    pub fn to_toml_string(&self) -> Result<String, ProxyConfigError> {
        let file = TomlFile {
            device: self.devices.to_disabled_entries(),
        };
        if file.device.is_empty() {
            return Ok(String::new());
        }
        Ok(toml::to_string_pretty(&file)?)
    }

    pub fn save_file(&self, path: &Path) -> Result<(), ProxyConfigError> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let text = self.to_toml_string()?;
        let tmp = path.with_extension("toml.tmp");
        fs::write(&tmp, &text)?;
        fs::rename(&tmp, path)?;
        Ok(())
    }

    pub fn set_device_enabled(&mut self, serial: &str, enabled: bool) {
        self.devices.set_enabled(serial, enabled);
    }
}

pub fn default_proxy_config_path() -> PathBuf {
    proxy_config_dir().join("config.toml")
}

pub fn proxy_config_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("adb-proxy");
        }
    }
    home_dir()
        .map(|h| h.join(".config/adb-proxy"))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Device enable policy reloaded from disk when the config mtime changes.
pub struct ReloadableProxyPolicy {
    path: PathBuf,
    cache: Mutex<PolicyCache>,
}

struct PolicyCache {
    mtime: Option<SystemTime>,
    table: DevicePolicyTable,
}

impl ReloadableProxyPolicy {
    pub fn new(path: PathBuf) -> Self {
        let table = ProxyFileConfig::load_file(&path)
            .unwrap_or_default()
            .devices;
        let mtime = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        Self {
            path,
            cache: Mutex::new(PolicyCache { mtime, table }),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn refresh(&self) -> DevicePolicyTable {
        let mut guard = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let mtime = fs::metadata(&self.path).ok().and_then(|m| m.modified().ok());
        if mtime != guard.mtime {
            let table = ProxyFileConfig::load_file(&self.path)
                .unwrap_or_default()
                .devices;
            guard.mtime = mtime;
            guard.table = table;
        }
        guard.table.clone()
    }

    pub fn is_enabled(&self, serial: &str) -> bool {
        self.refresh().is_enabled(serial)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_disabled_only() {
        let mut cfg = ProxyFileConfig::default();
        cfg.set_device_enabled("A1", false);
        cfg.set_device_enabled("A2", true); // not persisted
        let text = cfg.to_toml_string().unwrap();
        assert!(text.contains("A1"));
        assert!(!text.contains("A2"));
        let loaded = ProxyFileConfig::from_toml_str(&text).unwrap();
        assert!(!loaded.devices.is_enabled("A1"));
        assert!(loaded.devices.is_enabled("A2"));
    }

    #[test]
    fn missing_enabled_field_means_true() {
        let cfg = ProxyFileConfig::from_toml_str(
            r#"
[[device]]
serial = "X"
"#,
        )
        .unwrap();
        assert!(cfg.devices.is_enabled("X"));
    }
}
