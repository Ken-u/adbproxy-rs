use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Per-device enable policy. Missing from config / `enabled = true` means enabled.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DevicePolicyEntry {
    pub serial: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// Lookup table: serial → enabled. Absent serial defaults to enabled.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DevicePolicyTable {
    /// Only stores explicit entries; typically disabled ones.
    entries: HashMap<String, bool>,
}

impl DevicePolicyTable {
    pub fn from_entries(entries: impl IntoIterator<Item = DevicePolicyEntry>) -> Self {
        let mut table = Self::default();
        for e in entries {
            table.entries.insert(e.serial, e.enabled);
        }
        table
    }

    pub fn is_enabled(&self, serial: &str) -> bool {
        self.entries.get(serial).copied().unwrap_or(true)
    }

    /// Enable: drop the entry (缺省即启用). Disable: store `false`.
    pub fn set_enabled(&mut self, serial: &str, enabled: bool) {
        if enabled {
            self.entries.remove(serial);
        } else {
            self.entries.insert(serial.to_string(), false);
        }
    }

    /// Entries to persist: only disabled devices (keeps config minimal).
    pub fn to_disabled_entries(&self) -> Vec<DevicePolicyEntry> {
        let mut out: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, &en)| !en)
            .map(|(serial, _)| DevicePolicyEntry {
                serial: serial.clone(),
                enabled: false,
            })
            .collect();
        out.sort_by(|a, b| a.serial.cmp(&b.serial));
        out
    }

    /// All explicitly tracked serials (for CLI list of offline-but-disabled).
    pub fn explicit_serials(&self) -> impl Iterator<Item = (&str, bool)> + '_ {
        self.entries.iter().map(|(s, e)| (s.as_str(), *e))
    }
}

/// Filter a raw `host:devices` / `devices-l` body, dropping disabled serials.
pub fn filter_devices_body(raw: &str, is_enabled: impl Fn(&str) -> bool) -> String {
    let mut out = String::new();
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('*') || trimmed.starts_with("List of") {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let serial = trimmed.split_whitespace().next().unwrap_or("");
        if serial.is_empty() || !is_enabled(serial) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Extract serial from transport-style service strings.
pub fn transport_serial(service: &str) -> Option<&str> {
    if let Some(s) = service.strip_prefix("host:transport:") {
        if s == "any" || s == "usb" || s == "local" {
            return None;
        }
        return Some(s);
    }
    if let Some(s) = service.strip_prefix("host:tport:serial:") {
        return Some(s);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_enabled() {
        let t = DevicePolicyTable::default();
        assert!(t.is_enabled("ANY"));
    }

    #[test]
    fn disable_and_enable() {
        let mut t = DevicePolicyTable::default();
        t.set_enabled("A1", false);
        assert!(!t.is_enabled("A1"));
        t.set_enabled("A1", true);
        assert!(t.is_enabled("A1"));
        assert!(t.to_disabled_entries().is_empty());
    }

    #[test]
    fn filter_body_drops_disabled() {
        let body = "A1\tdevice\nA2\toffline\nA3\tdevice usb:1\n";
        let filtered = filter_devices_body(body, |s| s != "A2");
        assert_eq!(filtered, "A1\tdevice\nA3\tdevice usb:1\n");
    }

    #[test]
    fn transport_serial_parse() {
        assert_eq!(transport_serial("host:transport:ABC"), Some("ABC"));
        assert_eq!(transport_serial("host:tport:serial:X"), Some("X"));
        assert_eq!(transport_serial("host:transport-any"), None);
        assert_eq!(transport_serial("host:transport:any"), None);
    }
}
