pub mod protocol;

#[cfg(target_os = "linux")]
use std::os::fd::AsFd;
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use bytes::Bytes;
use fs2::FileExt;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use serde::Serialize;
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{RwLock, Semaphore, mpsc, oneshot},
    task::JoinSet,
};
use tokio_util::{
    codec::{Framed, LengthDelimitedCodec},
    sync::CancellationToken,
};

use crate::{
    config::Config,
    discovery::DiscoverySnapshot,
    history_search::{HistoryQuery, HistorySearchIndex},
    ipc::protocol::{
        ConfigResponse, DeviceItem, DiagnosticCheck, DiagnosticsResponse, ErrorResponse,
        HistoryItem, HistoryResponse, HistoryUpdateAction, IPC_PROTOCOL_VERSION,
        ImagePreviewResponse, MutationResponse, PeerItem, PeersResponse, Request, Response,
        ShareClipboardResponse, SharedSettingKind, StatusResponse, TransferItem, TransfersResponse,
        request, response,
    },
    mesh::{MESH_PROTOCOL_VERSION, MeshHandle},
    transfer::TRANSFER_PROTOCOL_VERSION,
};

const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;
const MAX_IPC_CONNECTIONS: usize = 32;
const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const IPC_SHARE_REQUEST_TIMEOUT: Duration = Duration::from_mins(30);

pub struct DaemonInstance {
    _lock: File,
}

impl DaemonInstance {
    /// Acquires the per-user daemon startup lock.
    ///
    /// # Errors
    ///
    /// Returns [`IpcError::AlreadyRunning`] when another daemon startup or
    /// process owns the lock, and an I/O error when the lock cannot be secured.
    pub fn acquire(runtime_dir: &Path) -> Result<Self, IpcError> {
        std::fs::create_dir_all(runtime_dir)?;
        make_socket_parent_private(runtime_dir)?;
        let lock_path = runtime_dir.join("daemon.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;
        set_lock_permissions(&lock_path)?;
        match lock.try_lock_exclusive() {
            Ok(()) => Ok(Self { _lock: lock }),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                Err(IpcError::AlreadyRunning)
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[derive(Clone)]
pub struct DaemonState {
    inner: Arc<DaemonStateInner>,
}

struct DaemonStateInner {
    started: Instant,
    hostname: String,
    config_path: PathBuf,
    config: RwLock<Config>,
    discovery: RwLock<Option<DiscoverySnapshot>>,
    discovery_error: RwLock<Option<String>>,
    clipboard_status: RwLock<DiagnosticStatus>,
    mesh: RwLock<Option<MeshHandle>>,
    history: RwLock<HistorySearchIndex>,
    device_names: RwLock<BTreeMap<String, String>>,
    devices: RwLock<Vec<DeviceItem>>,
    commands: mpsc::UnboundedSender<DaemonCommand>,
}

#[derive(Clone)]
struct DiagnosticStatus {
    ok: bool,
    detail: String,
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
        let index = HistorySearchIndex::new(history);
        *self.inner.history.write().await = index;
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

    #[allow(clippy::too_many_lines)]
    async fn handle(&self, request: Request) -> Response {
        let request_id = request.request_id;
        if request.protocol_version != IPC_PROTOCOL_VERSION {
            return error_response(
                request_id,
                "unsupported_protocol",
                format!(
                    "client protocol {} is incompatible with daemon protocol {IPC_PROTOCOL_VERSION}",
                    request.protocol_version
                ),
            );
        }

        let body = match request.body {
            Some(request::Body::Status(_)) => {
                let discovery = self.inner.discovery.read().await;
                response::Body::Status(StatusResponse {
                    version: env!("CARGO_PKG_VERSION").to_owned(),
                    hostname: self.inner.hostname.clone(),
                    uptime_seconds: self.inner.started.elapsed().as_secs(),
                    config_path: self.inner.config_path.display().to_string(),
                    netbird_address: discovery
                        .as_ref()
                        .map(|snapshot| snapshot.local_address.to_string()),
                    discovered_peers: discovery.as_ref().map_or(0, |snapshot| {
                        u32::try_from(snapshot.peers.len()).unwrap_or(u32::MAX)
                    }),
                })
            }
            Some(request::Body::Config(_)) => {
                #[derive(Serialize)]
                struct RedactedLocal<'a> {
                    listen_port: u16,
                    discovery_interval_seconds: u64,
                    reconcile_interval_seconds: u64,
                    reconnect_min_seconds: u64,
                    reconnect_max_seconds: u64,
                    netbird_command: String,
                    mesh_key_file_configured: bool,
                    config_path: &'a str,
                }

                #[derive(Serialize)]
                struct RedactedConfig<'a> {
                    shared: &'a crate::config::SharedConfig,
                    local: RedactedLocal<'a>,
                }

                let config = self.inner.config.read().await;
                let config_path = self.inner.config_path.to_string_lossy();
                let redacted = RedactedConfig {
                    shared: &config.shared,
                    local: RedactedLocal {
                        listen_port: config.local.listen_port,
                        discovery_interval_seconds: config.local.discovery_interval_seconds,
                        reconcile_interval_seconds: config.local.reconcile_interval_seconds,
                        reconnect_min_seconds: config.local.reconnect_min_seconds,
                        reconnect_max_seconds: config.local.reconnect_max_seconds,
                        netbird_command: config.local.netbird_command.display().to_string(),
                        mesh_key_file_configured: !config
                            .local
                            .mesh_key_file
                            .as_os_str()
                            .is_empty(),
                        config_path: &config_path,
                    },
                };
                match serde_json::to_vec(&redacted) {
                    Ok(redacted_json) => response::Body::Config(ConfigResponse { redacted_json }),
                    Err(error) => {
                        return error_response(
                            request_id,
                            "serialization_failed",
                            error.to_string(),
                        );
                    }
                }
            }
            Some(request::Body::History(history_request)) => {
                let query = match HistoryQuery::parse(&history_request.query) {
                    Ok(query) => query,
                    Err(error) => {
                        return error_response(
                            request_id,
                            "invalid_history_query",
                            error.to_string(),
                        );
                    }
                };
                let items = self
                    .inner
                    .history
                    .read()
                    .await
                    .search(&query, history_request.limit);
                response::Body::History(HistoryResponse { items })
            }
            Some(request::Body::ImagePreview(preview)) => {
                let content_id = preview.content_id;
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::ImagePreview { content_id, reply })
                    .is_err()
                {
                    return command_processor_unavailable(request_id);
                }
                match result.await {
                    Ok(Ok(preview)) => response::Body::ImagePreview(preview),
                    Ok(Err(message)) => {
                        return error_response(request_id, "image_preview_unavailable", message);
                    }
                    Err(_) => return command_processor_stopped(request_id),
                }
            }
            Some(request::Body::Activate(activate)) => {
                let content_id = activate.content_id;
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::Activate {
                        content_id: content_id.clone(),
                        reply,
                    })
                    .is_err()
                {
                    return error_response(
                        request_id,
                        "daemon_unavailable",
                        "daemon command processor is unavailable",
                    );
                }
                match result.await {
                    Ok(Ok(())) => response::Body::Mutation(MutationResponse {
                        ok: true,
                        message: "clipboard activated".to_owned(),
                        resource_id: Some(content_id),
                    }),
                    Ok(Err(message)) => {
                        return error_response(request_id, "activation_failed", message);
                    }
                    Err(_) => {
                        return error_response(
                            request_id,
                            "daemon_unavailable",
                            "daemon command processor stopped",
                        );
                    }
                }
            }
            Some(request::Body::Peers(_)) => {
                let discovery = self.inner.discovery.read().await;
                let discovery_error = self.inner.discovery_error.read().await.clone();
                let peers = discovery.as_ref().map_or_else(Vec::new, |snapshot| {
                    snapshot
                        .peers
                        .iter()
                        .map(|peer| PeerItem {
                            hostname: peer.hostname.clone(),
                            address: peer.address.to_string(),
                            connected: peer.connected,
                        })
                        .collect()
                });
                response::Body::Peers(PeersResponse {
                    local_hostname: discovery.as_ref().map_or_else(
                        || self.inner.hostname.clone(),
                        |snapshot| snapshot.local_hostname.clone(),
                    ),
                    local_address: discovery
                        .as_ref()
                        .map(|snapshot| snapshot.local_address.to_string()),
                    peers,
                    discovery_error,
                    devices: self.inner.devices.read().await.clone(),
                })
            }
            Some(request::Body::HistoryUpdate(update)) => {
                let action = match HistoryUpdateAction::try_from(update.action) {
                    Ok(HistoryUpdateAction::Pin) => HistoryUpdateAction::Pin,
                    Ok(HistoryUpdateAction::Unpin) => HistoryUpdateAction::Unpin,
                    Ok(HistoryUpdateAction::Delete) => HistoryUpdateAction::Delete,
                    Ok(HistoryUpdateAction::Unspecified) | Err(_) => {
                        return error_response(
                            request_id,
                            "invalid_request",
                            "history update action is missing or unknown",
                        );
                    }
                };
                let content_id = update.content_id;
                let (reply, result) = oneshot::channel();
                let command = match action {
                    HistoryUpdateAction::Pin => DaemonCommand::SetPinned {
                        content_id: content_id.clone(),
                        pinned: true,
                        reply,
                    },
                    HistoryUpdateAction::Unpin => DaemonCommand::SetPinned {
                        content_id: content_id.clone(),
                        pinned: false,
                        reply,
                    },
                    HistoryUpdateAction::Delete => DaemonCommand::Delete {
                        content_id: content_id.clone(),
                        reply,
                    },
                    HistoryUpdateAction::Unspecified => unreachable!(),
                };
                if self.inner.commands.send(command).is_err() {
                    return error_response(
                        request_id,
                        "daemon_unavailable",
                        "daemon command processor is unavailable",
                    );
                }
                match result.await {
                    Ok(Ok(())) => response::Body::Mutation(MutationResponse {
                        ok: true,
                        message: match action {
                            HistoryUpdateAction::Pin => "history item pinned",
                            HistoryUpdateAction::Unpin => "history item unpinned",
                            HistoryUpdateAction::Delete => "history item deleted",
                            HistoryUpdateAction::Unspecified => unreachable!(),
                        }
                        .to_owned(),
                        resource_id: Some(content_id),
                    }),
                    Ok(Err(message)) => {
                        return error_response(request_id, "history_update_failed", message);
                    }
                    Err(_) => {
                        return error_response(
                            request_id,
                            "daemon_unavailable",
                            "daemon command processor stopped",
                        );
                    }
                }
            }
            Some(request::Body::Diagnostics(_)) => {
                let discovery = self.inner.discovery.read().await;
                let discovery_error = self.inner.discovery_error.read().await;
                let clipboard = self.inner.clipboard_status.read().await.clone();
                let active_config = self.inner.config.read().await.clone();
                let config_check = Config::load(&self.inner.config_path);
                let (config_ok, config_detail) = match config_check {
                    Ok(on_disk) if on_disk == active_config => (
                        true,
                        format!("{} (loaded and current)", self.inner.config_path.display()),
                    ),
                    Ok(_) => (
                        false,
                        format!(
                            "{} differs from the daemon's active configuration",
                            self.inner.config_path.display()
                        ),
                    ),
                    Err(error) => (false, error.to_string()),
                };
                let mesh = self
                    .inner
                    .mesh
                    .read()
                    .await
                    .as_ref()
                    .map(MeshHandle::status);
                let (listener_ok, listener_detail, connections_ok, connection_detail) =
                    if let Some(mesh) = mesh {
                        let listener_detail = if let Some(address) = mesh.listener_address {
                            format!("listening on {address}")
                        } else if let Some(error) = mesh.last_listener_error {
                            format!("listener unavailable: {error}")
                        } else {
                            "listener inactive while NetBird discovery is unavailable".to_owned()
                        };
                        (
                            mesh.listener_address.is_some(),
                            listener_detail,
                            true,
                            format!(
                                "{} active of {}/{} discovered addresses",
                                mesh.active_connections,
                                mesh.discovered_addresses,
                                crate::discovery::MAX_DISCOVERED_PEERS
                            ),
                        )
                    } else {
                        (
                            false,
                            "mesh supervisor is not attached".to_owned(),
                            false,
                            "mesh supervisor is not attached".to_owned(),
                        )
                    };
                let discovery_ok = discovery.is_some() && discovery_error.is_none();
                let discovery_detail = if let Some(error) = discovery_error.as_deref() {
                    error.to_owned()
                } else if let Some(snapshot) = discovery.as_ref() {
                    format!("{} peers visible", snapshot.peers.len())
                } else {
                    "waiting for the first NetBird discovery result".to_owned()
                };
                response::Body::Diagnostics(DiagnosticsResponse {
                    checks: vec![
                        DiagnosticCheck {
                            name: "daemon".to_owned(),
                            ok: true,
                            detail: format!(
                                "running for {} seconds",
                                self.inner.started.elapsed().as_secs()
                            ),
                        },
                        DiagnosticCheck {
                            name: "config".to_owned(),
                            ok: config_ok,
                            detail: config_detail,
                        },
                        DiagnosticCheck {
                            name: "encrypted_storage".to_owned(),
                            ok: true,
                            detail: "open and ready".to_owned(),
                        },
                        DiagnosticCheck {
                            name: "mesh_secret".to_owned(),
                            ok: true,
                            detail: "owner-only key file and encrypted keyslot authenticated"
                                .to_owned(),
                        },
                        DiagnosticCheck {
                            name: "clipboard".to_owned(),
                            ok: clipboard.ok,
                            detail: clipboard.detail,
                        },
                        DiagnosticCheck {
                            name: "netbird".to_owned(),
                            ok: discovery_ok,
                            detail: discovery_detail,
                        },
                        DiagnosticCheck {
                            name: "mesh_listener".to_owned(),
                            ok: listener_ok,
                            detail: listener_detail,
                        },
                        DiagnosticCheck {
                            name: "mesh_connections".to_owned(),
                            ok: connections_ok,
                            detail: connection_detail,
                        },
                        DiagnosticCheck {
                            name: "protocol_versions".to_owned(),
                            ok: true,
                            detail: format!(
                                "ipc={IPC_PROTOCOL_VERSION}, mesh={MESH_PROTOCOL_VERSION}, transfer={TRANSFER_PROTOCOL_VERSION}"
                            ),
                        },
                    ],
                })
            }
            Some(request::Body::ShareClipboard(share)) => {
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::ShareClipboard {
                        confirmed: share.confirmed,
                        reply,
                    })
                    .is_err()
                {
                    return command_processor_unavailable(request_id);
                }
                match result.await {
                    Ok(Ok(result)) => response::Body::ShareClipboard(result),
                    Ok(Err(message)) => {
                        return error_response(request_id, "share_failed", message);
                    }
                    Err(_) => return command_processor_stopped(request_id),
                }
            }
            Some(request::Body::Transfers(_)) => {
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::ListTransfers { reply })
                    .is_err()
                {
                    return command_processor_unavailable(request_id);
                }
                match result.await {
                    Ok(Ok(transfers)) => response::Body::Transfers(TransfersResponse { transfers }),
                    Ok(Err(message)) => {
                        return error_response(request_id, "transfer_list_failed", message);
                    }
                    Err(_) => return command_processor_stopped(request_id),
                }
            }
            Some(request::Body::TransferCancel(cancel)) => {
                let transfer_id = cancel.transfer_id;
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::CancelTransfer {
                        transfer_id: transfer_id.clone(),
                        reply,
                    })
                    .is_err()
                {
                    return command_processor_unavailable(request_id);
                }
                match result.await {
                    Ok(Ok(())) => response::Body::Mutation(MutationResponse {
                        ok: true,
                        message: "transfer cancelled".to_owned(),
                        resource_id: Some(transfer_id),
                    }),
                    Ok(Err(message)) => {
                        return error_response(request_id, "transfer_cancel_failed", message);
                    }
                    Err(_) => return command_processor_stopped(request_id),
                }
            }
            Some(request::Body::ForgetDevice(forget)) => {
                let device_id = forget.device_id;
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::ForgetDevice {
                        device_id: device_id.clone(),
                        reply,
                    })
                    .is_err()
                {
                    return command_processor_unavailable(request_id);
                }
                match result.await {
                    Ok(Ok(())) => response::Body::Mutation(MutationResponse {
                        ok: true,
                        message: "device forgotten".to_owned(),
                        resource_id: Some(device_id),
                    }),
                    Ok(Err(message)) => {
                        return error_response(request_id, "device_forget_failed", message);
                    }
                    Err(_) => return command_processor_stopped(request_id),
                }
            }
            Some(request::Body::SharedSettingUpdate(update)) => {
                let setting = match SharedSettingKind::try_from(update.setting) {
                    Ok(SharedSettingKind::MeshQuotaBytes) => SharedSettingKind::MeshQuotaBytes,
                    Ok(SharedSettingKind::CaptureThresholdBytes) => {
                        SharedSettingKind::CaptureThresholdBytes
                    }
                    Ok(SharedSettingKind::Unspecified) | Err(_) => {
                        return error_response(
                            request_id,
                            "invalid_request",
                            "shared setting is missing or unknown",
                        );
                    }
                };
                if update.value == 0 {
                    return error_response(
                        request_id,
                        "invalid_request",
                        "shared setting value must be greater than zero",
                    );
                }
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::UpdateSharedSetting {
                        setting,
                        value: update.value,
                        reply,
                    })
                    .is_err()
                {
                    return command_processor_unavailable(request_id);
                }
                match result.await {
                    Ok(Ok(())) => response::Body::Mutation(MutationResponse {
                        ok: true,
                        message: format!(
                            "{} updated to {} bytes",
                            match setting {
                                SharedSettingKind::MeshQuotaBytes => "mesh quota",
                                SharedSettingKind::CaptureThresholdBytes => "capture threshold",
                                SharedSettingKind::Unspecified => unreachable!(),
                            },
                            update.value
                        ),
                        resource_id: Some(
                            match setting {
                                SharedSettingKind::MeshQuotaBytes => "mesh_quota_bytes",
                                SharedSettingKind::CaptureThresholdBytes => {
                                    "capture_threshold_bytes"
                                }
                                SharedSettingKind::Unspecified => unreachable!(),
                            }
                            .to_owned(),
                        ),
                    }),
                    Ok(Err(message)) => {
                        return error_response(request_id, "setting_update_failed", message);
                    }
                    Err(_) => return command_processor_stopped(request_id),
                }
            }
            None => {
                return error_response(request_id, "missing_request", "request body is missing");
            }
        };

        Response {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id,
            body: Some(body),
        }
    }
}

/// Serves versioned local IPC until cancellation.
///
/// # Errors
///
/// Returns an error when socket setup, acceptance, or cleanup fails.
pub async fn serve(
    socket: &Path,
    state: DaemonState,
    shutdown: CancellationToken,
) -> Result<(), IpcError> {
    prepare_socket(socket).await?;
    let listener = match UnixListener::bind(socket) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(IpcError::AlreadyRunning);
        }
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = set_socket_permissions(socket) {
        let _ = remove_socket(socket).await;
        return Err(error);
    }

    let limiter = Arc::new(Semaphore::new(MAX_IPC_CONNECTIONS));
    let mut tasks = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break Ok(()),
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined
                    && !error.is_cancelled()
                {
                    tracing::debug!(%error, "local IPC connection task failed");
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(IpcError::Io(error)),
                };
                if !peer_is_current_user(&stream)? {
                    continue;
                }
                let Ok(permit) = limiter.clone().try_acquire_owned() else {
                    continue;
                };
                let state = state.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_connection(stream, state).await {
                        tracing::debug!(%error, "local IPC connection ended");
                    }
                });
            }
        }
    };

    drop(listener);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let cleanup = remove_socket(socket).await;
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}

async fn remove_socket(socket: &Path) -> Result<(), IpcError> {
    match tokio::fs::symlink_metadata(socket).await {
        Ok(metadata) if is_socket(&metadata) => {}
        Ok(_) => return Err(IpcError::SocketPathNotSocket(socket.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    match tokio::fs::remove_file(socket).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Sends one request to the daemon and waits for its response.
///
/// # Errors
///
/// Returns an error when connection, framing, encoding, decoding, response
/// correlation, or the bounded response wait fails.
pub async fn request(socket: &Path, request: Request) -> Result<Response, IpcError> {
    let request_id = request.request_id;
    let timeout = request_timeout(&request);
    let response = tokio::time::timeout(timeout, request_inner(socket, request))
        .await
        .map_err(|_| IpcError::Timeout)??;
    if response.protocol_version != IPC_PROTOCOL_VERSION {
        return Err(IpcError::ResponseProtocol {
            expected: IPC_PROTOCOL_VERSION,
            actual: response.protocol_version,
        });
    }
    if response.request_id != request_id {
        return Err(IpcError::ResponseRequestId {
            expected: request_id,
            actual: response.request_id,
        });
    }
    Ok(response)
}

fn request_timeout(request: &Request) -> Duration {
    if matches!(
        request.body.as_ref(),
        Some(request::Body::ShareClipboard(_))
    ) {
        IPC_SHARE_REQUEST_TIMEOUT
    } else {
        IPC_REQUEST_TIMEOUT
    }
}

async fn request_inner(socket: &Path, request: Request) -> Result<Response, IpcError> {
    let stream = UnixStream::connect(socket).await?;
    let mut framed = Framed::new(stream, codec());
    let mut encoded = Vec::with_capacity(request.encoded_len());
    request.encode(&mut encoded)?;
    framed.send(Bytes::from(encoded)).await?;

    let frame = framed.next().await.ok_or(IpcError::ConnectionClosed)??;
    Ok(Response::decode(frame.freeze())?)
}

async fn serve_connection(stream: UnixStream, state: DaemonState) -> Result<(), IpcError> {
    let mut framed = Framed::new(stream, codec());
    while let Some(frame) = framed.next().await {
        let request = Request::decode(frame?.freeze())?;
        let response = state.handle(request).await;
        if response.encoded_len() > MAX_IPC_FRAME_BYTES {
            let response = error_response(
                response.request_id,
                "response_too_large",
                "response exceeds the local IPC frame limit",
            );
            let mut encoded = Vec::with_capacity(response.encoded_len());
            response.encode(&mut encoded)?;
            framed.send(Bytes::from(encoded)).await?;
            continue;
        }
        let mut encoded = Vec::with_capacity(response.encoded_len());
        response.encode(&mut encoded)?;
        framed.send(Bytes::from(encoded)).await?;
    }
    Ok(())
}

fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_IPC_FRAME_BYTES)
        .new_codec()
}

async fn prepare_socket(socket: &Path) -> Result<(), IpcError> {
    let parent = socket.parent().ok_or(IpcError::MissingSocketParent)?;
    tokio::fs::create_dir_all(parent).await?;
    make_socket_parent_private(parent)?;
    match std::fs::symlink_metadata(socket) {
        Ok(_) if !is_socket_path(socket)? => {
            return Err(IpcError::SocketPathNotSocket(socket.to_owned()));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    match UnixStream::connect(socket).await {
        Ok(_) => Err(IpcError::AlreadyRunning),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            remove_stale_socket(socket).await
        }
        Err(error) => Err(error.into()),
    }
}

async fn remove_stale_socket(socket: &Path) -> Result<(), IpcError> {
    match tokio::fs::symlink_metadata(socket).await {
        Ok(metadata) if is_socket(&metadata) => remove_socket(socket).await,
        Ok(_) => Err(IpcError::SocketPathNotSocket(socket.to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn is_socket(metadata: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;

    metadata.file_type().is_socket()
}

#[cfg(unix)]
fn is_socket_path(path: &Path) -> Result<bool, IpcError> {
    use std::os::unix::fs::FileTypeExt;

    Ok(std::fs::symlink_metadata(path)?.file_type().is_socket())
}

#[cfg(not(unix))]
fn is_socket_path(_path: &Path) -> Result<bool, IpcError> {
    Ok(true)
}

#[cfg(unix)]
fn make_socket_parent_private(parent: &Path) -> Result<(), IpcError> {
    use rustix::fs::{FileType, Mode, OFlags, fchmod, fstat, open};

    let fd = open(
        parent,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let stat = fstat(&fd).map_err(std::io::Error::from)?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir()
        || stat.st_uid != rustix::process::getuid().as_raw()
    {
        return Err(IpcError::UnsafeSocketParent);
    }
    fchmod(&fd, Mode::RUSR | Mode::WUSR | Mode::XUSR).map_err(std::io::Error::from)?;
    Ok(())
}

#[cfg(not(unix))]
fn make_socket_parent_private(_parent: &Path) -> Result<(), IpcError> {
    Ok(())
}

#[cfg(target_os = "linux")]
fn peer_is_current_user(stream: &UnixStream) -> Result<bool, IpcError> {
    let credentials =
        rustix::net::sockopt::socket_peercred(stream.as_fd()).map_err(std::io::Error::from)?;
    Ok(credentials.uid == rustix::process::getuid())
}

#[cfg(not(target_os = "linux"))]
fn peer_is_current_user(_stream: &UnixStream) -> Result<bool, IpcError> {
    Ok(true)
}

#[cfg(unix)]
fn set_socket_permissions(socket: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
fn set_lock_permissions(lock: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(lock, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn error_response(
    request_id: u64,
    code: impl Into<String>,
    message: impl Into<String>,
) -> Response {
    Response {
        protocol_version: IPC_PROTOCOL_VERSION,
        request_id,
        body: Some(response::Body::Error(ErrorResponse {
            code: code.into(),
            message: message.into(),
        })),
    }
}

fn command_processor_unavailable(request_id: u64) -> Response {
    error_response(
        request_id,
        "daemon_unavailable",
        "daemon command processor is unavailable",
    )
}

fn command_processor_stopped(request_id: u64) -> Response {
    error_response(
        request_id,
        "daemon_unavailable",
        "daemon command processor stopped",
    )
}

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("another clip-sync daemon is already listening")]
    AlreadyRunning,
    #[error("IPC socket path has no parent")]
    MissingSocketParent,
    #[error("IPC socket parent is not an owned private directory")]
    UnsafeSocketParent,
    #[error("refusing to replace non-socket IPC path {0:?}")]
    SocketPathNotSocket(PathBuf),
    #[error("daemon closed the IPC connection without responding")]
    ConnectionClosed,
    #[error("daemon did not respond within the local IPC timeout")]
    Timeout,
    #[error("daemon response protocol mismatch: expected {expected}, got {actual}")]
    ResponseProtocol { expected: u32, actual: u32 },
    #[error("daemon response request ID mismatch: expected {expected}, got {actual}")]
    ResponseRequestId { expected: u64, actual: u64 },
    #[error("IPC I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid IPC message: {0}")]
    Protocol(#[from] prost::DecodeError),
    #[error("could not encode IPC message: {0}")]
    Encode(#[from] prost::EncodeError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::{
        HistoryRequest, HistoryUpdateAction, HistoryUpdateRequest, ShareClipboardRequest,
        StatusRequest, request,
    };

    #[tokio::test]
    async fn status_round_trip_over_unix_socket() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let socket = temporary.path().join("daemon.sock");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve(&server_socket, state, server_shutdown)
                .await
                .expect("serve IPC");
        });

        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(temporary.path())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let response = request(
            &socket,
            Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 7,
                body: Some(request::Body::Status(StatusRequest {})),
            },
        )
        .await
        .expect("status response");

        assert_eq!(response.request_id, 7);
        let Some(response::Body::Status(status)) = response.body else {
            panic!("expected status response");
        };
        assert_eq!(status.hostname, "test-node");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            assert_eq!(
                std::fs::metadata(temporary.path())
                    .expect("runtime directory metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                std::fs::metadata(&socket)
                    .expect("socket metadata")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }

        shutdown.cancel();
        server.await.expect("server task");
    }

    #[test]
    fn share_requests_keep_a_finite_extended_deadline() {
        let share = Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 1,
            body: Some(request::Body::ShareClipboard(ShareClipboardRequest {
                confirmed: true,
            })),
        };
        let status = Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 2,
            body: Some(request::Body::Status(StatusRequest {})),
        };

        assert_eq!(request_timeout(&share), IPC_SHARE_REQUEST_TIMEOUT);
        assert_eq!(request_timeout(&status), IPC_REQUEST_TIMEOUT);
        assert!(request_timeout(&share) > request_timeout(&status));
    }

    #[tokio::test]
    async fn shutdown_aborts_idle_connections_and_removes_socket() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let socket = temporary.path().join("daemon.sock");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let shutdown = CancellationToken::new();
        let server_shutdown = shutdown.clone();
        let server_socket = socket.clone();
        let server = tokio::spawn(async move {
            serve(&server_socket, state, server_shutdown)
                .await
                .expect("serve IPC");
        });
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let _idle = UnixStream::connect(&socket).await.expect("idle connection");

        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("server shutdown must be bounded")
            .expect("server task");
        assert!(!socket.exists());
    }

    #[tokio::test]
    async fn history_search_is_bounded_and_case_insensitive() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        state
            .set_history(vec![
                HistoryItem {
                    content_id: "alpha".to_owned(),
                    preview: "Build Finished".to_owned(),
                    mime_types: vec!["text/plain".to_owned()],
                    logical_size: 14,
                    source_node: "kiwi".to_owned(),
                    source_device: "kiwi".to_owned(),
                    pinned: false,
                    physical_millis: 2,
                },
                HistoryItem {
                    content_id: "beta".to_owned(),
                    preview: "unrelated".to_owned(),
                    mime_types: vec!["image/png".to_owned()],
                    logical_size: 20,
                    source_node: "vd".to_owned(),
                    source_device: "vd".to_owned(),
                    pinned: false,
                    physical_millis: 1,
                },
            ])
            .await;

        let response = state
            .handle(Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 8,
                body: Some(request::Body::History(HistoryRequest {
                    query: "FINISHED".to_owned(),
                    limit: 1,
                })),
            })
            .await;
        let Some(response::Body::History(history)) = response.body else {
            panic!("expected history response");
        };
        assert_eq!(history.items.len(), 1);
        assert_eq!(history.items[0].content_id, "alpha");
    }

    #[tokio::test]
    async fn history_search_uses_authenticated_device_name_aliases() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        state
            .set_device_names(BTreeMap::from([("node-id".to_owned(), "vd".to_owned())]))
            .await;
        state
            .set_history(vec![HistoryItem {
                content_id: "content".to_owned(),
                preview: "Screenshot".to_owned(),
                mime_types: vec!["image/png".to_owned()],
                logical_size: 10,
                source_node: "node-id".to_owned(),
                source_device: String::new(),
                pinned: true,
                physical_millis: 1,
            }])
            .await;

        let response = state
            .handle(Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 80,
                body: Some(request::Body::History(HistoryRequest {
                    query: "D:vd,T:image,P:true".to_owned(),
                    limit: 100,
                })),
            })
            .await;
        let Some(response::Body::History(history)) = response.body else {
            panic!("expected history response");
        };
        assert_eq!(history.items.len(), 1);
        assert_eq!(history.items[0].source_device, "vd");
    }

    #[tokio::test]
    async fn history_search_applies_typed_filters_in_newest_first_order() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        state
            .set_history(vec![
                HistoryItem {
                    content_id: "old".to_owned(),
                    preview: "Release Notes".to_owned(),
                    mime_types: vec!["text/markdown".to_owned()],
                    logical_size: 4_096,
                    source_node: "office-node".to_owned(),
                    source_device: "Office Laptop".to_owned(),
                    pinned: true,
                    physical_millis: 1_704_067_199_000,
                },
                HistoryItem {
                    content_id: "new".to_owned(),
                    preview: "Release Notes".to_owned(),
                    mime_types: vec!["text/markdown".to_owned()],
                    logical_size: 4_500,
                    source_node: "office-node".to_owned(),
                    source_device: "Office Laptop".to_owned(),
                    pinned: true,
                    physical_millis: 1_704_067_199_500,
                },
                HistoryItem {
                    content_id: "wrong-device".to_owned(),
                    preview: "Release Notes".to_owned(),
                    mime_types: vec!["text/markdown".to_owned()],
                    logical_size: 4_500,
                    source_node: "phone-node".to_owned(),
                    source_device: "Phone".to_owned(),
                    pinned: true,
                    physical_millis: 1_704_067_199_900,
                },
            ])
            .await;

        let response = state
            .handle(Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 81,
                body: Some(request::Body::History(HistoryRequest {
                    query: concat!(
                        r#""release notes" device:"office laptop" type:markdown "#,
                        "pinned:true min-size:4KiB max-size:5KB ",
                        "before:2024-01-01T00:00:00Z"
                    )
                    .to_owned(),
                    limit: 500,
                })),
            })
            .await;
        let Some(response::Body::History(history)) = response.body else {
            panic!("expected history response");
        };
        assert_eq!(
            history
                .items
                .iter()
                .map(|item| item.content_id.as_str())
                .collect::<Vec<_>>(),
            ["new", "old"]
        );
    }

    #[tokio::test]
    async fn invalid_history_query_error_is_stable_and_does_not_echo_value() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let response = state
            .handle(Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 82,
                body: Some(request::Body::History(HistoryRequest {
                    query: "pinned:private-value".to_owned(),
                    limit: 100,
                })),
            })
            .await;
        let Some(response::Body::Error(error)) = response.body else {
            panic!("expected error response");
        };
        assert_eq!(error.code, "invalid_history_query");
        assert_eq!(
            error.message,
            "invalid query at byte 0: pinned expects true or false"
        );
        assert!(!error.message.contains("private-value"));
    }

    #[tokio::test]
    async fn large_history_search_stays_responsive_and_bounded() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let items = (0_u64..50_000)
            .map(|index| HistoryItem {
                content_id: format!("content-{index:05}"),
                preview: format!("ordinary clipboard preview {index}"),
                mime_types: vec!["text/plain".to_owned()],
                logical_size: index,
                source_node: format!("device-{}", index % 8),
                source_device: format!("host-{}", index % 8),
                pinned: index % 10 == 0,
                physical_millis: index,
            })
            .collect();
        state.set_history(items).await;

        let started = Instant::now();
        let response = state
            .handle(Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 83,
                body: Some(request::Body::History(HistoryRequest {
                    query: "not-present pinned:false type:text".to_owned(),
                    limit: u32::MAX,
                })),
            })
            .await;
        let elapsed = started.elapsed();
        let Some(response::Body::History(history)) = response.body else {
            panic!("expected history response");
        };
        assert!(history.items.is_empty());
        assert!(
            elapsed < Duration::from_secs(1),
            "50k-entry metadata search took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn config_response_is_redacted_and_complete_for_local_ui_fields() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, _command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );

        let response = state
            .handle(Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 11,
                body: Some(request::Body::Config(protocol::ConfigRequest {})),
            })
            .await;
        let Some(response::Body::Config(config)) = response.body else {
            panic!("expected config response");
        };
        let value: serde_json::Value =
            serde_json::from_slice(&config.redacted_json).expect("valid config JSON");
        let local = value
            .get("local")
            .and_then(serde_json::Value::as_object)
            .expect("local config object");

        for field in [
            "listen_port",
            "discovery_interval_seconds",
            "reconcile_interval_seconds",
            "reconnect_min_seconds",
            "reconnect_max_seconds",
            "netbird_command",
            "mesh_key_file_configured",
            "config_path",
        ] {
            assert!(local.contains_key(field), "missing {field}");
        }
        assert!(!local.contains_key("mesh_key_file"));
        assert!(!String::from_utf8_lossy(&config.redacted_json).contains("/run/secrets"));
    }

    #[tokio::test]
    async fn history_mutation_reaches_daemon_command_processor() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let handler = tokio::spawn(async move {
            state
                .handle(Request {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: 9,
                    body: Some(request::Body::HistoryUpdate(HistoryUpdateRequest {
                        content_id: "content-id".to_owned(),
                        action: HistoryUpdateAction::Pin as i32,
                    })),
                })
                .await
        });

        let command = command_rx.recv().await.expect("daemon command");
        let DaemonCommand::SetPinned {
            content_id,
            pinned,
            reply,
        } = command
        else {
            panic!("expected pin command");
        };
        assert_eq!(content_id, "content-id");
        assert!(pinned);
        reply.send(Ok(())).expect("mutation reply");

        let response = handler.await.expect("handler task");
        let Some(response::Body::Mutation(mutation)) = response.body else {
            panic!("expected mutation response");
        };
        assert!(mutation.ok);
        assert_eq!(mutation.resource_id.as_deref(), Some("content-id"));
    }

    #[tokio::test]
    async fn image_preview_round_trips_through_daemon_command() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let handler = tokio::spawn(async move {
            state
                .handle(Request {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: 91,
                    body: Some(request::Body::ImagePreview(protocol::ImagePreviewRequest {
                        content_id: "content-id".to_owned(),
                    })),
                })
                .await
        });

        let DaemonCommand::ImagePreview { content_id, reply } =
            command_rx.recv().await.expect("image preview command")
        else {
            panic!("expected image preview command");
        };
        assert_eq!(content_id, "content-id");
        reply
            .send(Ok(protocol::ImagePreviewResponse {
                content_id,
                mime_type: "image/png".to_owned(),
                width: 2,
                height: 1,
                rgba: vec![255; 8],
            }))
            .expect("image preview reply");

        let response = handler.await.expect("handler task");
        let Some(response::Body::ImagePreview(preview)) = response.body else {
            panic!("expected image preview response");
        };
        assert_eq!(preview.content_id, "content-id");
        assert_eq!((preview.width, preview.height), (2, 1));
        assert_eq!(preview.rgba, vec![255; 8]);
    }

    #[tokio::test]
    async fn clipboard_share_inspection_round_trips_through_daemon_command() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let handler = tokio::spawn(async move {
            state
                .handle(Request {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: 10,
                    body: Some(request::Body::ShareClipboard(ShareClipboardRequest {
                        confirmed: false,
                    })),
                })
                .await
        });
        let DaemonCommand::ShareClipboard { confirmed, reply } =
            command_rx.recv().await.expect("share command")
        else {
            panic!("expected clipboard share command");
        };
        assert!(!confirmed);
        reply
            .send(Ok(protocol::ShareClipboardResponse {
                shared: false,
                confirmation_required: true,
                logical_size: 42,
                mime_types: vec!["text/plain".to_owned()],
                quota_exempt: false,
                transfer_id: None,
                content_id: None,
                message: "confirm".to_owned(),
            }))
            .expect("share reply");

        let response = handler.await.expect("handler task");
        let Some(response::Body::ShareClipboard(share)) = response.body else {
            panic!("expected share response");
        };
        assert!(share.confirmation_required);
        assert_eq!(share.logical_size, 42);
    }

    #[tokio::test]
    async fn transfer_list_and_cancel_round_trip_through_daemon_commands() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let list_state = state.clone();
        let list_handler = tokio::spawn(async move {
            list_state
                .handle(Request {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: 20,
                    body: Some(request::Body::Transfers(protocol::TransfersRequest {})),
                })
                .await
        });
        let DaemonCommand::ListTransfers { reply } = command_rx.recv().await.expect("list command")
        else {
            panic!("expected transfer list command");
        };
        reply
            .send(Ok(vec![protocol::TransferItem {
                transfer_id: "transfer".to_owned(),
                content_id: "content".to_owned(),
                peer: "peer".to_owned(),
                state: "replicating".to_owned(),
                completed_bytes: 5,
                total_bytes: 10,
            }]))
            .expect("list reply");
        let response = list_handler.await.expect("list handler");
        let Some(response::Body::Transfers(transfers)) = response.body else {
            panic!("expected transfers response");
        };
        assert_eq!(transfers.transfers[0].completed_bytes, 5);

        let cancel_handler = tokio::spawn(async move {
            state
                .handle(Request {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: 21,
                    body: Some(request::Body::TransferCancel(
                        protocol::TransferCancelRequest {
                            transfer_id: "transfer".to_owned(),
                        },
                    )),
                })
                .await
        });
        let DaemonCommand::CancelTransfer { transfer_id, reply } =
            command_rx.recv().await.expect("cancel command")
        else {
            panic!("expected transfer cancel command");
        };
        assert_eq!(transfer_id, "transfer");
        reply.send(Ok(())).expect("cancel reply");
        let response = cancel_handler.await.expect("cancel handler");
        let Some(response::Body::Mutation(mutation)) = response.body else {
            panic!("expected mutation response");
        };
        assert_eq!(mutation.resource_id.as_deref(), Some("transfer"));
    }

    #[tokio::test]
    async fn device_forget_and_setting_update_round_trip_through_daemon_commands() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let (commands, mut command_rx) = mpsc::unbounded_channel();
        let state = DaemonState::new(
            "test-node".to_owned(),
            temporary.path().join("config.toml"),
            Config::default(),
            commands,
        );
        let forget_state = state.clone();
        let forget_handler = tokio::spawn(async move {
            forget_state
                .handle(Request {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: 30,
                    body: Some(request::Body::ForgetDevice(protocol::ForgetDeviceRequest {
                        device_id: "device".to_owned(),
                    })),
                })
                .await
        });
        let DaemonCommand::ForgetDevice { device_id, reply } =
            command_rx.recv().await.expect("forget command")
        else {
            panic!("expected device forget command");
        };
        assert_eq!(device_id, "device");
        reply.send(Ok(())).expect("forget reply");
        let response = forget_handler.await.expect("forget handler");
        assert!(matches!(response.body, Some(response::Body::Mutation(_))));

        let setting_handler = tokio::spawn(async move {
            state
                .handle(Request {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: 31,
                    body: Some(request::Body::SharedSettingUpdate(
                        protocol::SharedSettingUpdateRequest {
                            setting: protocol::SharedSettingKind::MeshQuotaBytes as i32,
                            value: 4096,
                        },
                    )),
                })
                .await
        });
        let DaemonCommand::UpdateSharedSetting {
            setting,
            value,
            reply,
        } = command_rx.recv().await.expect("setting command")
        else {
            panic!("expected setting update command");
        };
        assert_eq!(setting, protocol::SharedSettingKind::MeshQuotaBytes);
        assert_eq!(value, 4096);
        reply.send(Ok(())).expect("setting reply");
        let response = setting_handler.await.expect("setting handler");
        let Some(response::Body::Mutation(mutation)) = response.body else {
            panic!("expected setting mutation response");
        };
        assert_eq!(mutation.resource_id.as_deref(), Some("mesh_quota_bytes"));
    }

    #[tokio::test]
    async fn live_socket_is_reported_as_an_existing_daemon() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let socket = temporary.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind live socket");

        let error = prepare_socket(&socket)
            .await
            .expect_err("live daemon must win");

        assert!(matches!(error, IpcError::AlreadyRunning));
        drop(listener);
    }

    #[test]
    fn daemon_startup_lock_is_singleton_and_recoverable() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let first = DaemonInstance::acquire(temporary.path()).expect("first daemon lock");
        let Err(second) = DaemonInstance::acquire(temporary.path()) else {
            panic!("second daemon must be rejected");
        };
        assert!(matches!(second, IpcError::AlreadyRunning));

        drop(first);
        DaemonInstance::acquire(temporary.path()).expect("lock is released on drop");
    }

    #[tokio::test]
    async fn regular_file_at_socket_path_is_never_removed() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let socket = temporary.path().join("daemon.sock");
        std::fs::write(&socket, b"keep me").expect("write sentinel");

        let error = prepare_socket(&socket)
            .await
            .expect_err("regular file must be rejected");

        assert!(matches!(error, IpcError::SocketPathNotSocket(path) if path == socket));
        assert_eq!(
            std::fs::read(&socket).expect("sentinel remains"),
            b"keep me"
        );
    }

    #[tokio::test]
    async fn client_rejects_mismatched_response_request_id() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let socket = temporary.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut framed = Framed::new(stream, codec());
            let _request = framed
                .next()
                .await
                .expect("request frame")
                .expect("valid frame");
            let response = Response {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 999,
                body: Some(response::Body::Status(StatusResponse {
                    version: "test".to_owned(),
                    hostname: "test".to_owned(),
                    uptime_seconds: 0,
                    config_path: "test".to_owned(),
                    netbird_address: None,
                    discovered_peers: 0,
                })),
            };
            let mut encoded = Vec::with_capacity(response.encoded_len());
            response.encode(&mut encoded).expect("encode response");
            framed
                .send(Bytes::from(encoded))
                .await
                .expect("send response");
        });

        let error = request(
            &socket,
            Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 12,
                body: Some(request::Body::Status(StatusRequest {})),
            },
        )
        .await
        .expect_err("mismatched response must fail");

        assert!(matches!(
            error,
            IpcError::ResponseRequestId {
                expected: 12,
                actual: 999
            }
        ));
        server.await.expect("test server");
    }
}
