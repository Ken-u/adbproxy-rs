use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

use crate::config::BackendConfig;
use crate::registry::{merge_device_lists, DeviceSnapshot};

/// OS user identifier. On Linux, this is the UID from `SO_PEERCRED` /
/// `NETLINK_SOCK_DIAG`.
pub type Uid = u32;

/// Multi-tenant device registry.
///
/// `visible_devices(uid) = shared_local_devices + private_devices(agent(uid))`
///
/// Shared devices are updated by the daemon from the local ADB backend.
/// Private devices are pushed by each user's agent via the IPC protocol.
/// Pair codes never appear in this registry.
pub struct TenantRegistry {
    shared: Arc<RwLock<DeviceSnapshot>>,
    /// uid → private device snapshot (route_id set; pair_code always None)
    private: Arc<RwLock<HashMap<Uid, DeviceSnapshot>>>,
    notify: broadcast::Sender<()>,
}

impl TenantRegistry {
    pub fn new() -> Self {
        let (notify, _) = broadcast::channel(64);
        Self {
            shared: Arc::new(RwLock::new(DeviceSnapshot::default())),
            private: Arc::new(RwLock::new(HashMap::new())),
            notify,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.notify.subscribe()
    }

    /// Update the shared (local ADB) device list visible to all users.
    pub async fn update_shared(&self, snap: DeviceSnapshot) {
        let mut guard = self.shared.write().await;
        let changed = guard.devices != snap.devices;
        *guard = snap;
        drop(guard);
        if changed {
            let _ = self.notify.send(());
        }
    }

    /// Update shared devices from raw backend lists.
    pub async fn update_shared_from_lists(&self, lists: &[(BackendConfig, String)]) {
        let merged = merge_device_lists(lists);
        self.update_shared(merged).await;
    }

    /// Register or replace an agent's private device list.
    ///
    /// Callers must ensure every device has `route_id` set and `pair_code` is
    /// None — pair codes must never cross the agent/daemon boundary.
    pub async fn update_agent_devices(&self, uid: Uid, devices: DeviceSnapshot) {
        debug_assert!(
            devices
                .devices
                .iter()
                .all(|d| d.route_id.is_some() && d.pair_code.is_none()),
            "agent devices must carry route_id and no pair_code"
        );
        let mut guard = self.private.write().await;
        let changed = guard
            .get(&uid)
            .map(|prev| prev.devices != devices.devices)
            .unwrap_or(true);
        guard.insert(uid, devices);
        drop(guard);
        if changed {
            let _ = self.notify.send(());
        }
    }

    /// Remove an agent (on disconnect). Only that user's private devices
    /// are removed; shared devices remain.
    pub async fn remove_agent(&self, uid: Uid) {
        let mut guard = self.private.write().await;
        if guard.remove(&uid).is_some() {
            drop(guard);
            let _ = self.notify.send(());
        }
    }

    /// Build the combined device view for a specific user:
    /// `shared + private(uid)`.
    pub async fn snapshot_for(&self, uid: Uid) -> DeviceSnapshot {
        let shared = self.shared.read().await.clone();
        let private = self.private.read().await;

        let mut devices = shared.devices;
        if let Some(priv_snap) = private.get(&uid) {
            devices.extend(priv_snap.devices.iter().cloned());
        }

        DeviceSnapshot { devices }
    }

    /// Snapshot of shared devices only.
    pub async fn snapshot_shared(&self) -> DeviceSnapshot {
        self.shared.read().await.clone()
    }

    /// Check whether a given UID has a registered agent with private devices.
    pub async fn has_agent(&self, uid: Uid) -> bool {
        self.private.read().await.contains_key(&uid)
    }
}

impl Default for TenantRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for TenantRegistry {
    fn clone(&self) -> Self {
        Self {
            shared: self.shared.clone(),
            private: self.private.clone(),
            notify: self.notify.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::DeviceEntry;

    fn shared_device(serial: &str) -> DeviceEntry {
        DeviceEntry {
            public_serial: serial.into(),
            upstream_serial: serial.into(),
            state: "device".into(),
            extras: String::new(),
            backend_name: "local".into(),
            backend_addr: "127.0.0.1:5039".parse().unwrap(),
            pair_code: None,
            route_id: None,
        }
    }

    fn private_device(serial: &str, backend: &str) -> DeviceEntry {
        DeviceEntry {
            public_serial: serial.into(),
            upstream_serial: serial.into(),
            state: "device".into(),
            extras: String::new(),
            backend_name: backend.into(),
            backend_addr: "127.0.0.1:0".parse().unwrap(),
            pair_code: None,
            route_id: Some(format!("route-{serial}")),
        }
    }

    #[tokio::test]
    async fn shared_visible_to_all() {
        let reg = TenantRegistry::new();
        reg.update_shared(DeviceSnapshot {
            devices: vec![shared_device("USB1")],
        })
        .await;

        let alice = reg.snapshot_for(1000).await;
        let bob = reg.snapshot_for(1001).await;
        assert_eq!(alice.devices.len(), 1);
        assert_eq!(bob.devices.len(), 1);
        assert_eq!(alice.devices[0].public_serial, "USB1");
    }

    #[tokio::test]
    async fn private_only_visible_to_owner() {
        let reg = TenantRegistry::new();
        reg.update_shared(DeviceSnapshot {
            devices: vec![shared_device("USB1")],
        })
        .await;
        reg.update_agent_devices(
            1000,
            DeviceSnapshot {
                devices: vec![private_device("ALICE_DEV", "office")],
            },
        )
        .await;

        let alice = reg.snapshot_for(1000).await;
        let bob = reg.snapshot_for(1001).await;

        assert_eq!(alice.devices.len(), 2);
        assert!(alice.find("ALICE_DEV").is_some());
        assert_eq!(bob.devices.len(), 1);
        assert!(bob.find("ALICE_DEV").is_none());
    }

    #[tokio::test]
    async fn agent_disconnect_removes_private() {
        let reg = TenantRegistry::new();
        reg.update_agent_devices(
            1000,
            DeviceSnapshot {
                devices: vec![private_device("PRIV1", "office")],
            },
        )
        .await;
        assert_eq!(reg.snapshot_for(1000).await.devices.len(), 1);

        reg.remove_agent(1000).await;
        assert_eq!(reg.snapshot_for(1000).await.devices.len(), 0);
    }

    #[tokio::test]
    async fn users_isolated() {
        let reg = TenantRegistry::new();
        reg.update_agent_devices(
            1000,
            DeviceSnapshot {
                devices: vec![private_device("A1", "alice-office")],
            },
        )
        .await;
        reg.update_agent_devices(
            1001,
            DeviceSnapshot {
                devices: vec![private_device("B1", "bob-lab")],
            },
        )
        .await;

        let alice = reg.snapshot_for(1000).await;
        let bob = reg.snapshot_for(1001).await;

        assert!(alice.find("A1").is_some());
        assert!(alice.find("B1").is_none());
        assert!(bob.find("B1").is_some());
        assert!(bob.find("A1").is_none());
    }
}
