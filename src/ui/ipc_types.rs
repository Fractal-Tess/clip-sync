use std::{collections::HashSet, path::Path, time::Duration};

use eframe::egui;

use crate::{
    ipc::{
        self,
        protocol::{
            ActivateRequest, ConfigRequest, ConfigResponse, DiagnosticsRequest,
            DiagnosticsResponse, ForgetDeviceRequest, HistoryRequest, HistoryResponse,
            HistoryUpdateAction, HistoryUpdateRequest, ImagePreviewRequest, ImagePreviewResponse,
            MutationResponse, PeersRequest, PeersResponse, Request, Response,
            ShareClipboardRequest, ShareClipboardResponse, SharedSettingKind,
            SharedSettingUpdateRequest, StatusRequest, StatusResponse, TransferCancelRequest,
            TransfersRequest, TransfersResponse, request,
        },
    },
    ui::{
        Presentation,
        ipc_worker::{
            daemon_readiness_probe, is_daemon_absent, start_user_service, wait_for_daemon_ready,
        },
    },
};

mod responses;

pub(super) use responses::preview_texture;
use responses::{
    expect_config, expect_diagnostics, expect_history, expect_image_preview, expect_mutation,
    expect_peers, expect_share, expect_status, expect_transfers,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PendingScope {
    Share,
    HistoryMutation,
    TransferCancel,
    ForgetDevice,
    Setting,
}

impl PendingScope {
    pub(super) const fn is_share(self) -> bool {
        matches!(self, Self::Share)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum IpcDispatchClass {
    ReadOnly,
    Mutation,
    Share,
}

pub(super) fn mutation_block_reason(
    pending: &HashSet<PendingScope>,
    requested: PendingScope,
) -> Option<&'static str> {
    if requested.is_share() {
        if pending.contains(&PendingScope::Share) {
            Some("Clipboard sharing is already in progress.")
        } else if pending.is_empty() {
            None
        } else {
            Some("Wait for the pending change to finish before sharing the clipboard.")
        }
    } else if pending.contains(&PendingScope::Share) {
        Some("Wait for clipboard sharing to finish before making another change.")
    } else if pending.contains(&requested) {
        Some("This change is already in progress.")
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShareCompletion {
    Apply,
    Discard,
    Ignore,
}

#[derive(Debug, Default)]
pub(super) struct ShareGenerationState {
    current: u64,
    active: Option<u64>,
}

impl ShareGenerationState {
    pub(super) fn start_request(&mut self) -> u64 {
        self.current = self.current.saturating_add(1);
        self.active = Some(self.current);
        self.current
    }

    pub(super) fn invalidate(&mut self) {
        self.current = self.current.saturating_add(1);
    }

    pub(super) fn cancel_active(&mut self, generation: u64) {
        if self.active == Some(generation) {
            self.active = None;
        }
    }

    pub(super) fn complete(&mut self, generation: u64) -> ShareCompletion {
        if self.active != Some(generation) {
            return ShareCompletion::Ignore;
        }
        self.active = None;
        if generation == self.current {
            ShareCompletion::Apply
        } else {
            ShareCompletion::Discard
        }
    }
}

pub(super) enum ImagePreviewState {
    Loading,
    Ready(egui::TextureHandle),
    Unavailable,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum HistoryAction {
    Activate(String),
    Pin { content_id: String, pinned: bool },
    Delete(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum MutationKind {
    Activate,
    ActivateQuick,
    Pin,
    Delete,
    TransferCancel,
    ForgetDevice,
    Setting,
}

impl MutationKind {
    pub(super) const fn refreshes_history(self) -> bool {
        matches!(self, Self::Activate | Self::Pin | Self::Delete)
    }

    pub(super) const fn pending_scope(self) -> PendingScope {
        match self {
            Self::Activate | Self::ActivateQuick | Self::Pin | Self::Delete => {
                PendingScope::HistoryMutation
            }
            Self::TransferCancel => PendingScope::TransferCancel,
            Self::ForgetDevice => PendingScope::ForgetDevice,
            Self::Setting => PendingScope::Setting,
        }
    }
}

pub(super) const fn activation_result_closes(
    kind: MutationKind,
    presentation: Presentation,
) -> bool {
    matches!(kind, MutationKind::ActivateQuick) && matches!(presentation, Presentation::Quick)
}

pub(super) const fn share_confirmation_visible(
    presentation: Presentation,
    has_inspection: bool,
) -> bool {
    has_inspection && matches!(presentation, Presentation::Management)
}

#[derive(Clone, Copy)]
pub(super) struct PendingSetting {
    pub(super) kind: SharedSettingKind,
    pub(super) value: u64,
}

impl PendingSetting {
    pub(super) const fn label(self) -> &'static str {
        match self.kind {
            SharedSettingKind::MeshQuotaBytes => "mesh quota",
            SharedSettingKind::CaptureThresholdBytes => "capture threshold",
            SharedSettingKind::Unspecified => "shared setting",
        }
    }
}

pub(super) enum UiCommand {
    Status,
    RetryStatus,
    History {
        query: String,
        generation: u64,
    },
    ImagePreview {
        content_id: String,
    },
    Peers,
    Config,
    Diagnostics,
    Transfers,
    ShareClipboard {
        confirmed: bool,
        generation: u64,
    },
    TransferCancel {
        transfer_id: String,
    },
    ForgetDevice {
        device_id: String,
    },
    UpdateSharedSetting {
        setting: SharedSettingKind,
        value: u64,
    },
    Activate {
        content_id: String,
        kind: MutationKind,
    },
    HistoryUpdate {
        content_id: String,
        action: HistoryUpdateAction,
        kind: MutationKind,
    },
}

pub(super) enum UiEvent {
    Status(Result<StatusResponse, String>),
    History {
        generation: u64,
        result: Result<HistoryResponse, String>,
    },
    ImagePreview {
        content_id: String,
        result: Result<ImagePreviewResponse, String>,
    },
    Peers(Result<PeersResponse, String>),
    Config(Result<ConfigResponse, String>),
    Diagnostics(Result<DiagnosticsResponse, String>),
    Transfers(Result<TransfersResponse, String>),
    Share {
        generation: u64,
        result: Result<ShareClipboardResponse, String>,
    },
    Mutation {
        kind: MutationKind,
        result: Result<MutationResponse, String>,
    },
}
impl UiCommand {
    pub(super) const fn pending_scope(&self) -> Option<PendingScope> {
        match self {
            Self::ShareClipboard { .. } => Some(PendingScope::Share),
            Self::TransferCancel { .. } => Some(PendingScope::TransferCancel),
            Self::ForgetDevice { .. } => Some(PendingScope::ForgetDevice),
            Self::UpdateSharedSetting { .. } => Some(PendingScope::Setting),
            Self::Activate { kind, .. } | Self::HistoryUpdate { kind, .. } => {
                Some(kind.pending_scope())
            }
            Self::Status
            | Self::RetryStatus
            | Self::History { .. }
            | Self::ImagePreview { .. }
            | Self::Peers
            | Self::Config
            | Self::Diagnostics
            | Self::Transfers => None,
        }
    }

    pub(super) const fn dispatch_class(&self) -> IpcDispatchClass {
        match self.pending_scope() {
            Some(PendingScope::Share) => IpcDispatchClass::Share,
            Some(_) => IpcDispatchClass::Mutation,
            None => IpcDispatchClass::ReadOnly,
        }
    }

    pub(super) const fn share_generation(&self) -> Option<u64> {
        match self {
            Self::ShareClipboard { generation, .. } => Some(*generation),
            _ => None,
        }
    }

    pub(super) fn request_body(self) -> (request::Body, EventTarget) {
        match self {
            Self::Status | Self::RetryStatus => {
                (request::Body::Status(StatusRequest {}), EventTarget::Status)
            }
            Self::History { query, generation } => (
                request::Body::History(HistoryRequest { query, limit: 200 }),
                EventTarget::History(generation),
            ),
            Self::ImagePreview { content_id } => (
                request::Body::ImagePreview(ImagePreviewRequest {
                    content_id: content_id.clone(),
                }),
                EventTarget::ImagePreview(content_id),
            ),
            Self::Peers => (request::Body::Peers(PeersRequest {}), EventTarget::Peers),
            Self::Config => (request::Body::Config(ConfigRequest {}), EventTarget::Config),
            Self::Diagnostics => (
                request::Body::Diagnostics(DiagnosticsRequest {}),
                EventTarget::Diagnostics,
            ),
            Self::Transfers => (
                request::Body::Transfers(TransfersRequest {}),
                EventTarget::Transfers,
            ),
            Self::ShareClipboard {
                confirmed,
                generation,
            } => (
                request::Body::ShareClipboard(ShareClipboardRequest { confirmed }),
                EventTarget::Share(generation),
            ),
            Self::TransferCancel { transfer_id } => (
                request::Body::TransferCancel(TransferCancelRequest { transfer_id }),
                EventTarget::Mutation(MutationKind::TransferCancel),
            ),
            Self::ForgetDevice { device_id } => (
                request::Body::ForgetDevice(ForgetDeviceRequest { device_id }),
                EventTarget::Mutation(MutationKind::ForgetDevice),
            ),
            Self::UpdateSharedSetting { setting, value } => (
                request::Body::SharedSettingUpdate(SharedSettingUpdateRequest {
                    setting: setting as i32,
                    value,
                }),
                EventTarget::Mutation(MutationKind::Setting),
            ),
            Self::Activate { content_id, kind } => (
                request::Body::Activate(ActivateRequest { content_id }),
                EventTarget::Mutation(kind),
            ),
            Self::HistoryUpdate {
                content_id,
                action,
                kind,
            } => (
                request::Body::HistoryUpdate(HistoryUpdateRequest {
                    content_id,
                    action: action as i32,
                }),
                EventTarget::Mutation(kind),
            ),
        }
    }
}

pub(super) enum EventTarget {
    Status,
    History(u64),
    ImagePreview(String),
    Peers,
    Config,
    Diagnostics,
    Transfers,
    Share(u64),
    Mutation(MutationKind),
}

impl EventTarget {
    pub(super) fn into_event(self, result: Result<Response, String>) -> UiEvent {
        match self {
            Self::Status => UiEvent::Status(expect_status(result)),
            Self::History(generation) => UiEvent::History {
                generation,
                result: expect_history(result),
            },
            Self::ImagePreview(content_id) => UiEvent::ImagePreview {
                content_id,
                result: expect_image_preview(result),
            },
            Self::Peers => UiEvent::Peers(expect_peers(result)),
            Self::Config => UiEvent::Config(expect_config(result)),
            Self::Diagnostics => UiEvent::Diagnostics(expect_diagnostics(result)),
            Self::Transfers => UiEvent::Transfers(expect_transfers(result)),
            Self::Share(generation) => UiEvent::Share {
                generation,
                result: expect_share(result),
            },
            Self::Mutation(kind) => UiEvent::Mutation {
                kind,
                result: expect_mutation(result),
            },
        }
    }
}

pub(super) async fn request_with_shared_daemon_start(
    socket: &Path,
    request: Request,
    daemon_start_gate: &tokio::sync::Mutex<()>,
) -> Result<Response, String> {
    match ipc::request(socket, request.clone()).await {
        Ok(response) => return Ok(response),
        Err(error) if !is_daemon_absent(&error) => return Err(error.to_string()),
        Err(_) => {}
    }

    {
        let _start_guard = daemon_start_gate.lock().await;
        ensure_daemon_ready(socket).await?;
    }

    ipc::request(socket, request)
        .await
        .map_err(|error| error.to_string())
}

pub(super) async fn ensure_daemon_ready(socket: &Path) -> Result<(), String> {
    let probe = daemon_readiness_probe();
    match ipc::request(socket, probe.clone()).await {
        Ok(_) => return Ok(()),
        Err(error) if !is_daemon_absent(&error) => return Err(error.to_string()),
        Err(_) => {}
    }

    let start_detail = start_user_service().await;
    wait_for_daemon_ready(socket, probe, 20, Duration::from_millis(100), &start_detail).await
}
