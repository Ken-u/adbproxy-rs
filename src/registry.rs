use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

use crate::config::BackendConfig;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceEntry {
    /// Serial as seen by clients (may be rewritten on conflict).
    pub public_serial: String,
    /// Serial to send to the upstream adb server.
    pub upstream_serial: String,
    pub state: String,
    /// Extra columns from `devices -l` (without leading serial/state).
    pub extras: String,
    pub backend_name: String,
    pub backend_addr: SocketAddr,
    pub pair_code: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceSnapshot {
    pub devices: Vec<DeviceEntry>,
}

impl DeviceSnapshot {
    pub fn format_devices(&self, long: bool) -> String {
        let mut out = String::new();
        for d in &self.devices {
            if long {
                if d.extras.is_empty() {
                    out.push_str(&format!("{}\t{}\n", d.public_serial, d.state));
                } else {
                    out.push_str(&format!(
                        "{}\t{} {}\n",
                        d.public_serial, d.state, d.extras
                    ));
                }
            } else {
                out.push_str(&format!("{}\t{}\n", d.public_serial, d.state));
            }
        }
        out
    }

    pub fn find(&self, public_serial: &str) -> Option<&DeviceEntry> {
        self.devices.iter().find(|d| d.public_serial == public_serial)
    }

    pub fn online_count(&self) -> usize {
        self.devices
            .iter()
            .filter(|d| d.state == "device")
            .count()
    }
}

#[derive(Clone)]
pub struct DeviceRegistry {
    inner: Arc<RwLock<DeviceSnapshot>>,
    notify: broadcast::Sender<()>,
}

impl DeviceRegistry {
    pub fn new() -> Self {
        let (notify, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(RwLock::new(DeviceSnapshot::default())),
            notify,
        }
    }

    pub async fn snapshot(&self) -> DeviceSnapshot {
        self.inner.read().await.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.notify.subscribe()
    }

    /// Replace registry contents from per-backend raw device lists.
    ///
    /// `lists` entries are `(backend, raw body of host:devices-l)`.
    pub async fn update_from_backend_lists(
        &self,
        lists: &[(BackendConfig, String)],
    ) {
        let merged = merge_device_lists(lists);
        let mut guard = self.inner.write().await;
        let changed = guard.devices != merged.devices;
        *guard = merged;
        drop(guard);
        if changed {
            let _ = self.notify.send(());
        }
    }
}

impl Default for DeviceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse one line of `adb devices` / `devices -l` output.
pub fn parse_device_line(line: &str) -> Option<(String, String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('*') || line.starts_with("List of") {
        return None;
    }
    let mut parts = line.split_whitespace();
    let serial = parts.next()?.to_string();
    let state = parts.next()?.to_string();
    let extras: Vec<&str> = parts.collect();
    let extras = extras.join(" ");
    Some((serial, state, extras))
}

pub fn merge_device_lists(lists: &[(BackendConfig, String)]) -> DeviceSnapshot {
    // First pass: count serial occurrences across backends.
    let mut serial_counts: HashMap<String, usize> = HashMap::new();
    let mut parsed: Vec<(BackendConfig, String, String, String)> = Vec::new();

    for (backend, body) in lists {
        for line in body.lines() {
            if let Some((serial, state, extras)) = parse_device_line(line) {
                *serial_counts.entry(serial.clone()).or_insert(0) += 1;
                parsed.push((backend.clone(), serial, state, extras));
            }
        }
    }

    let mut used_public = HashSet::new();
    let mut devices = Vec::new();

    for (backend, serial, state, extras) in parsed {
        let conflict = serial_counts.get(&serial).copied().unwrap_or(0) > 1;
        let mut public = if conflict {
            format!("{}:{}", backend.name, serial)
        } else {
            serial.clone()
        };
        // Extremely unlikely, but keep public serials unique.
        if !used_public.insert(public.clone()) {
            public = format!("{}:{}:{}", backend.name, serial, devices.len());
            used_public.insert(public.clone());
        }
        devices.push(DeviceEntry {
            public_serial: public,
            upstream_serial: serial,
            state,
            extras,
            backend_name: backend.name.clone(),
            backend_addr: backend.addr,
            pair_code: backend.pair_code.clone(),
        });
    }

    DeviceSnapshot { devices }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(name: &str, addr: &str) -> BackendConfig {
        BackendConfig {
            name: name.into(),
            addr: addr.parse().unwrap(),
            pair_code: None,
        }
    }

    #[test]
    fn merge_unique_serials() {
        let lists = vec![
            (
                backend("a", "1.1.1.1:5038"),
                "ABC\tdevice\n".to_string(),
            ),
            (
                backend("b", "2.2.2.2:5038"),
                "DEF\tdevice usb:1\n".to_string(),
            ),
        ];
        let snap = merge_device_lists(&lists);
        assert_eq!(snap.devices.len(), 2);
        assert_eq!(snap.devices[0].public_serial, "ABC");
        assert_eq!(snap.devices[1].public_serial, "DEF");
        assert_eq!(snap.devices[1].extras, "usb:1");
    }

    #[test]
    fn merge_rewrites_conflicts() {
        let lists = vec![
            (
                backend("office", "1.1.1.1:5038"),
                "SAME\tdevice\n".to_string(),
            ),
            (
                backend("lab", "2.2.2.2:5038"),
                "SAME\toffline\n".to_string(),
            ),
        ];
        let snap = merge_device_lists(&lists);
        assert_eq!(snap.devices[0].public_serial, "office:SAME");
        assert_eq!(snap.devices[1].public_serial, "lab:SAME");
        assert_eq!(snap.devices[0].upstream_serial, "SAME");
    }

    #[test]
    fn format_short_and_long() {
        let snap = DeviceSnapshot {
            devices: vec![DeviceEntry {
                public_serial: "X".into(),
                upstream_serial: "X".into(),
                state: "device".into(),
                extras: "product:foo".into(),
                backend_name: "a".into(),
                backend_addr: "1.1.1.1:1".parse().unwrap(),
                pair_code: None,
            }],
        };
        assert_eq!(snap.format_devices(false), "X\tdevice\n");
        assert_eq!(snap.format_devices(true), "X\tdevice product:foo\n");
    }
}
