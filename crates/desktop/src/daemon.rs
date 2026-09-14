//! Blocking wrappers over the asynchronous daemon IPC client.
//!
//! The picker is a short-lived, single-threaded process, so it drives the
//! async client on a current-thread runtime rather than keeping one alive.

use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result, bail};
use clip_sync_core::config::AppPaths;
use clip_sync_ipc::{
    self as ipc,
    protocol::{
        ActivateRequest, ConfigRequest, DiagnosticCheck, DiagnosticsRequest, ForgetDeviceRequest,
        HistoryRequest, HistoryUpdateAction, HistoryUpdateRequest, IPC_PROTOCOL_VERSION,
        ImagePreviewRequest, PeerInterfacesUpdateRequest, PeersRequest, PeersResponse, Request,
        SharedSettingKind, SharedSettingUpdateRequest, StatusRequest, StatusResponse,
        TransferCancelRequest, TransferItem, TransfersRequest, request, response,
    },
};

/// One clipboard history entry, flattened to what the picker draws.
pub struct HistoryItem {
    pub content_id: String,
    pub preview: String,
    pub source: String,
    pub pinned: bool,
    pub is_image: bool,
    pub size_bytes: u64,
    /// Unix milliseconds the entry was first copied.
    pub created_millis: u64,
}

/// A decoded thumbnail, already bounded by the daemon to 320x180.
pub struct ImagePreview {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// The daemon's redacted view of its own configuration.
#[derive(serde::Deserialize)]
pub struct Settings {
    pub shared: SharedSettings,
    pub local: LocalSettings,
}

#[derive(serde::Deserialize)]
pub struct SharedSettings {
    pub mesh_quota_bytes: u64,
    pub capture_threshold_bytes: u64,
    pub revision: String,
}

#[derive(serde::Deserialize)]
pub struct LocalSettings {
    pub listen_port: u16,
    pub discovery_interval_seconds: u64,
    pub reconcile_interval_seconds: u64,
    pub reconnect_min_seconds: u64,
    pub reconnect_max_seconds: u64,
    pub peer_interfaces: Vec<String>,
    pub mesh_key_file_configured: bool,
    pub config_path: String,
}

pub struct Daemon {
    socket: PathBuf,
    runtime: tokio::runtime::Runtime,
    request_id: AtomicU64,
}

impl Daemon {
    /// Connects the picker to the daemon socket named by the resolved paths.
    ///
    /// # Errors
    ///
    /// Returns an error when configuration discovery or runtime startup fails.
    pub fn discover(config_override: Option<PathBuf>) -> Result<Self> {
        let paths = AppPaths::discover(config_override).context("failed to resolve ClipSync paths")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to start the picker runtime")?;
        Ok(Self {
            socket: paths.socket,
            runtime,
            request_id: AtomicU64::new(0),
        })
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    fn send(&self, body: request::Body) -> Result<response::Body> {
        let request = Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: self.request_id.fetch_add(1, Ordering::Relaxed) + 1,
            body: Some(body),
        };
        let response = self
            .runtime
            .block_on(ipc::request(&self.socket, request))
            .with_context(|| {
                format!(
                    "ClipSync daemon is unavailable at {}",
                    self.socket.display()
                )
            })?;
        match response.body {
            Some(response::Body::Error(error)) => bail!("{}: {}", error.code, error.message),
            Some(body) => Ok(body),
            None => bail!("ClipSync daemon returned an empty response"),
        }
    }

    /// Fetches one page of clipboard history, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable or answers unexpectedly.
    pub fn history(&self, query: &str, limit: u32) -> Result<Vec<HistoryItem>> {
        let body = self.send(request::Body::History(HistoryRequest {
            query: query.to_owned(),
            limit,
            offset: 0,
        }))?;
        let response::Body::History(history) = body else {
            bail!("ClipSync daemon returned the wrong response to history");
        };
        Ok(history
            .items
            .into_iter()
            .map(|item| HistoryItem {
                is_image: item
                    .mime_types
                    .iter()
                    .any(|mime| mime.starts_with("image/")),
                content_id: item.content_id,
                preview: item.preview,
                source: if item.source_device.is_empty() {
                    item.source_node
                } else {
                    item.source_device
                },
                pinned: item.pinned,
                size_bytes: item.logical_size,
                // `origin_millis` is when the entry was copied on whichever
                // node produced it; `physical_millis` is only when this node
                // learned about it, which for synced entries is later.
                created_millis: item.origin_millis.unwrap_or(item.physical_millis),
            })
            .collect())
    }

    /// Reports the daemon's version, uptime, and peer counts.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable or answers unexpectedly.
    pub fn status(&self) -> Result<StatusResponse> {
        let body = self.send(request::Body::Status(StatusRequest {}))?;
        let response::Body::Status(status) = body else {
            bail!("ClipSync daemon returned the wrong response to status");
        };
        Ok(status)
    }

    /// Lists connected peers and the devices the mesh has authenticated.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable or answers unexpectedly.
    pub fn peers(&self) -> Result<PeersResponse> {
        let body = self.send(request::Body::Peers(PeersRequest {}))?;
        let response::Body::Peers(peers) = body else {
            bail!("ClipSync daemon returned the wrong response to peers");
        };
        Ok(peers)
    }

    /// Lists transfers the mesh is currently moving.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable or answers unexpectedly.
    pub fn transfers(&self) -> Result<Vec<TransferItem>> {
        let body = self.send(request::Body::Transfers(TransfersRequest {}))?;
        let response::Body::Transfers(transfers) = body else {
            bail!("ClipSync daemon returned the wrong response to transfers");
        };
        Ok(transfers.transfers)
    }

    /// Runs the daemon's self-checks.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable or answers unexpectedly.
    pub fn diagnostics(&self) -> Result<Vec<DiagnosticCheck>> {
        let body = self.send(request::Body::Diagnostics(DiagnosticsRequest {}))?;
        let response::Body::Diagnostics(diagnostics) = body else {
            bail!("ClipSync daemon returned the wrong response to diagnostics");
        };
        Ok(diagnostics.checks)
    }

    /// Reads back the daemon's configuration with secrets stripped out.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable, answers unexpectedly,
    /// or sends a payload this build cannot parse.
    pub fn settings(&self) -> Result<Settings> {
        let body = self.send(request::Body::Config(ConfigRequest {}))?;
        let response::Body::Config(config) = body else {
            bail!("ClipSync daemon returned the wrong response to config");
        };
        serde_json::from_slice(&config.redacted_json)
            .context("failed to parse the daemon's configuration")
    }

    /// Stops an in-flight transfer.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable, answers unexpectedly,
    /// or refuses the cancellation.
    pub fn cancel_transfer(&self, transfer_id: &str) -> Result<()> {
        let body = self.send(request::Body::TransferCancel(TransferCancelRequest {
            transfer_id: transfer_id.to_owned(),
        }))?;
        Self::mutation(body, "transfer cancellation failed").map(drop)
    }

    /// Revokes a device's membership in the mesh.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable, answers unexpectedly,
    /// or refuses to forget the device.
    pub fn forget_device(&self, device_id: &str) -> Result<()> {
        let body = self.send(request::Body::ForgetDevice(ForgetDeviceRequest {
            device_id: device_id.to_owned(),
        }))?;
        Self::mutation(body, "forgetting the device failed").map(drop)
    }

    /// Writes one mesh-wide setting, which the daemon replicates to peers.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable, answers unexpectedly,
    /// or rejects the value.
    pub fn set_shared_setting(&self, setting: SharedSettingKind, value: u64) -> Result<()> {
        let body = self.send(request::Body::SharedSettingUpdate(
            SharedSettingUpdateRequest {
                setting: setting as i32,
                value,
            },
        ))?;
        Self::mutation(body, "the setting update failed").map(drop)
    }

    /// Replaces the interface allowlist this node discovers peers on.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable, answers unexpectedly,
    /// or rejects the interface list.
    pub fn set_peer_interfaces(&self, interfaces: Vec<String>) -> Result<()> {
        let body = self.send(request::Body::PeerInterfacesUpdate(
            PeerInterfacesUpdateRequest { interfaces },
        ))?;
        Self::mutation(body, "the interface update failed").map(drop)
    }

    /// Unwraps a mutation response, turning a refusal into an error.
    fn mutation(body: response::Body, fallback: &str) -> Result<String> {
        let response::Body::Mutation(mutation) = body else {
            bail!("ClipSync daemon returned the wrong response to a mutation");
        };
        if !mutation.ok {
            bail!(
                "{}",
                if mutation.message.is_empty() {
                    fallback.to_owned()
                } else {
                    mutation.message
                }
            );
        }
        Ok(mutation.message)
    }

    /// Fetches one decoded thumbnail for an image entry.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry has no raster image the daemon can
    /// decode, or when the daemon is unreachable.
    pub fn image_preview(&self, content_id: &str) -> Result<ImagePreview> {
        let body = self.send(request::Body::ImagePreview(ImagePreviewRequest {
            content_id: content_id.to_owned(),
        }))?;
        let response::Body::ImagePreview(preview) = body else {
            bail!("ClipSync daemon returned the wrong response to an image preview");
        };
        Ok(ImagePreview {
            width: preview.width,
            height: preview.height,
            rgba: preview.rgba,
        })
    }

    /// Pins, unpins, or deletes a history entry.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable, answers unexpectedly,
    /// or reports the mutation as failed.
    pub fn update(&self, content_id: &str, action: HistoryUpdateAction) -> Result<()> {
        let body = self.send(request::Body::HistoryUpdate(HistoryUpdateRequest {
            content_id: content_id.to_owned(),
            action: action as i32,
        }))?;
        Self::mutation(body, "history update failed").map(drop)
    }

    /// Promotes a history entry back onto the local clipboard.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon is unreachable, answers unexpectedly,
    /// or reports the activation as failed.
    pub fn activate(&self, content_id: &str) -> Result<String> {
        let body = self.send(request::Body::Activate(ActivateRequest {
            content_id: content_id.to_owned(),
        }))?;
        Self::mutation(body, "clipboard activation failed")
    }
}
