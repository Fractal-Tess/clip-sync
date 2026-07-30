use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::Instant};

use tokio::sync::{RwLock, mpsc, oneshot};

use crate::{
    config::Config, discovery::DiscoverySnapshot, history_search::HistorySearchIndex,
    mesh::MeshHandle,
};

use super::protocol::{
    DeviceItem, HistoryItem, ImagePreviewResponse, ShareClipboardResponse, SharedSettingKind,
    TransferItem,
};

#[derive(Clone)]
pub struct DaemonState {
    pub(super) inner: Arc<DaemonStateInner>,
}

pub(super) struct DaemonStateInner {
    pub(super) started: Instant,
    pub(super) hostname: String,
    pub(super) config_path: PathBuf,
    pub(super) config: RwLock<Config>,
    pub(super) discovery: RwLock<Option<DiscoverySnapshot>>,
    pub(super) discovery_error: RwLock<Option<String>>,
    pub(super) clipboard_status: RwLock<DiagnosticStatus>,
    pub(super) mesh: RwLock<Option<MeshHandle>>,
    pub(super) history: RwLock<HistorySearchIndex>,
    pub(super) peer_history_stats: RwLock<BTreeMap<String, PeerHistoryStats>>,
    pub(super) device_names: RwLock<BTreeMap<String, String>>,
    pub(super) devices: RwLock<Vec<DeviceItem>>,
    pub(super) commands: mpsc::UnboundedSender<DaemonCommand>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PeerHistoryStats {
    pub(super) shared_items: u64,
    pub(super) shared_bytes: u64,
    pub(super) pinned_items: u64,
    pub(super) last_shared_millis: Option<u64>,
}

#[derive(Clone)]
pub(super) struct DiagnosticStatus {
    pub(super) ok: bool,
    pub(super) detail: String,
}

pub enum DaemonCommand {
    Activate {
        content_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    SetPinned {
        content_id: String,
        pinned: bool,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Delete {
        content_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ShareClipboard {
        confirmed: bool,
        reply: oneshot::Sender<Result<ShareClipboardResponse, String>>,
    },
    ListTransfers {
        reply: oneshot::Sender<Result<Vec<TransferItem>, String>>,
    },
    CancelTransfer {
        transfer_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ForgetDevice {
        device_id: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    UpdateSharedSetting {
        setting: SharedSettingKind,
        value: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
    ImagePreview {
        content_id: String,
        reply: oneshot::Sender<Result<ImagePreviewResponse, String>>,
    },
}

impl DaemonState {
    #[must_use]
    pub fn new(
        hostname: String,
        config_path: PathBuf,
        config: Config,
        commands: mpsc::UnboundedSender<DaemonCommand>,
    ) -> Self {
        Self {
            inner: Arc::new(DaemonStateInner {
                started: Instant::now(),
                hostname,
                config_path,
                config: RwLock::new(config),
                discovery: RwLock::new(None),
                discovery_error: RwLock::new(None),
                clipboard_status: RwLock::new(DiagnosticStatus {
                    ok: true,
                    detail: "clipboard monitoring is starting".to_owned(),
                }),
                mesh: RwLock::new(None),
                history: RwLock::new(HistorySearchIndex::default()),
                peer_history_stats: RwLock::new(BTreeMap::new()),
                device_names: RwLock::new(BTreeMap::new()),
                devices: RwLock::new(Vec::new()),
                commands,
            }),
        }
    }

    pub async fn set_discovery(&self, discovery: DiscoverySnapshot) {
        *self.inner.discovery.write().await = Some(discovery);
        *self.inner.discovery_error.write().await = None;
    }

    pub async fn set_discovery_error(&self, error: impl Into<String>) {
        *self.inner.discovery.write().await = None;
        *self.inner.discovery_error.write().await = Some(error.into());
    }

    pub async fn set_clipboard_status(&self, ok: bool, detail: impl Into<String>) {
        *self.inner.clipboard_status.write().await = DiagnosticStatus {
            ok,
            detail: detail.into(),
        };
    }

    pub async fn set_mesh(&self, mesh: MeshHandle) {
        *self.inner.mesh.write().await = Some(mesh);
    }

    pub async fn set_history(&self, mut history: Vec<HistoryItem>) {
        let device_names = self.inner.device_names.read().await;
        for item in &mut history {
            if let Some(device_name) = device_names.get(&item.source_node) {
                item.source_device = device_name.clone();
            }
        }
        drop(device_names);
        let mut stats = BTreeMap::<String, PeerHistoryStats>::new();
        for item in &history {
            let entry = stats.entry(item.source_node.clone()).or_default();
            entry.shared_items = entry.shared_items.saturating_add(1);
            entry.shared_bytes = entry.shared_bytes.saturating_add(item.logical_size);
            entry.pinned_items = entry.pinned_items.saturating_add(u64::from(item.pinned));
            let origin_millis = item.origin_millis.unwrap_or(item.physical_millis);
            entry.last_shared_millis = Some(
                entry
                    .last_shared_millis
                    .map_or(origin_millis, |current| current.max(origin_millis)),
            );
        }
        let index = HistorySearchIndex::new(history);
        *self.inner.history.write().await = index;
        *self.inner.peer_history_stats.write().await = stats;
    }

    pub async fn set_device_names(&self, device_names: BTreeMap<String, String>) {
        *self.inner.device_names.write().await = device_names;
    }

    pub async fn set_devices(&self, devices: Vec<DeviceItem>) {
        *self.inner.devices.write().await = devices;
    }

    pub async fn set_config(&self, config: Config) {
        *self.inner.config.write().await = config;
    }
}
