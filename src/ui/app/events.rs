use std::{collections::HashSet, time::Instant};

use super::{ClipSyncApp, WindowState};
use crate::ui::{
    history::{
        history_poll_cadence, preserve_history_selection, replace_history_snapshot,
        should_dispatch_coalesced_history,
    },
    ipc_types::{
        ImagePreviewState, MutationKind, PendingScope, ShareCompletion, UiCommand, UiEvent,
        activation_result_closes, preview_texture,
    },
    style::{Notice, config_pointer_u64, format_bytes_input},
};

impl ClipSyncApp {
    #[allow(
        clippy::too_many_lines,
        reason = "event handling keeps each typed IPC result and its UI state transition together"
    )]
    pub(super) fn poll_events(&mut self, history_poll_eligible: bool) {
        while let Ok(event) = self.event_rx.try_recv() {
            match event {
                UiEvent::Status(result) => match result {
                    Ok(status) => {
                        self.status = Some(status);
                        self.daemon_error = None;
                    }
                    Err(error) => {
                        self.status = None;
                        self.daemon_error = Some(error);
                    }
                },
                UiEvent::History { generation, result } => {
                    let succeeded = result.is_ok();
                    let cadence = history_poll_cadence(self.viewport_focused);
                    let dispatch_coalesced =
                        self.history_refresh
                            .finish(Instant::now(), succeeded, cadence);
                    if generation != self.history_generation {
                        if should_dispatch_coalesced_history(
                            dispatch_coalesced,
                            history_poll_eligible,
                        ) {
                            self.dispatch_history_request();
                        }
                        continue;
                    }
                    self.history_loading = false;
                    let previous_index = self.selected_history;
                    match replace_history_snapshot(&mut self.history, result) {
                        Ok(()) => {
                            for item in &self.history {
                                if !item.source_device.is_empty() {
                                    self.known_devices.insert(item.source_device.clone());
                                }
                                for mime in &item.mime_types {
                                    let normalized = mime.to_ascii_lowercase();
                                    self.known_types.insert(normalized.clone());
                                    if normalized.starts_with("image/") {
                                        self.known_types.insert("image".to_owned());
                                    }
                                    if normalized.starts_with("text/")
                                        || matches!(
                                            normalized.as_str(),
                                            "string" | "text" | "utf8_string"
                                        )
                                    {
                                        self.known_types.insert("text".to_owned());
                                    }
                                    if normalized == "text/uri-list" {
                                        self.known_types.insert("files".to_owned());
                                    }
                                }
                            }
                            let visible_content_ids = self
                                .history
                                .iter()
                                .map(|item| item.content_id.as_str())
                                .collect::<HashSet<_>>();
                            self.image_previews.retain(|content_id, _| {
                                visible_content_ids.contains(content_id.as_str())
                            });
                            let (index, content_id) = preserve_history_selection(
                                &self.history,
                                self.selected_content_id.as_deref(),
                                previous_index,
                            );
                            self.selected_history = index;
                            self.selected_content_id = content_id;
                            self.history_error = None;
                        }
                        Err(error) => {
                            self.history_error = Some(error);
                            self.refresh_status();
                        }
                    }
                    if should_dispatch_coalesced_history(dispatch_coalesced, history_poll_eligible)
                    {
                        self.dispatch_history_request();
                    }
                }
                UiEvent::ImagePreview { content_id, result } => {
                    if !self
                        .history
                        .iter()
                        .any(|item| item.content_id == content_id)
                    {
                        self.image_previews.remove(&content_id);
                        continue;
                    }
                    let preview = result
                        .and_then(|preview| preview_texture(&self.context, &content_id, &preview));
                    self.image_previews.insert(
                        content_id,
                        preview.map_or(ImagePreviewState::Unavailable, ImagePreviewState::Ready),
                    );
                }
                UiEvent::Peers(result) => {
                    self.peers_refresh_pending = false;
                    match result {
                        Ok(peers) => {
                            self.peers = Some(peers);
                            self.peers_error = None;
                        }
                        Err(error) => self.peers_error = Some(error),
                    }
                }
                UiEvent::Config(result) => {
                    self.config_refresh_pending = false;
                    match result {
                        Ok(config) => match serde_json::from_slice(&config.redacted_json) {
                            Ok(config) => {
                                if self.mesh_quota_input.is_empty() {
                                    self.mesh_quota_input =
                                        config_pointer_u64(&config, "/shared/mesh_quota_bytes")
                                            .map_or_else(String::new, format_bytes_input);
                                }
                                if self.capture_threshold_input.is_empty() {
                                    self.capture_threshold_input = config_pointer_u64(
                                        &config,
                                        "/shared/capture_threshold_bytes",
                                    )
                                    .map_or_else(String::new, |value| value.to_string());
                                }
                                self.config = Some(config);
                                self.config_error = None;
                            }
                            Err(error) => {
                                self.config_error =
                                    Some(format!("invalid config response: {error}"));
                            }
                        },
                        Err(error) => self.config_error = Some(error),
                    }
                }
                UiEvent::Diagnostics(result) => {
                    self.diagnostics_refresh_pending = false;
                    match result {
                        Ok(diagnostics) => {
                            self.diagnostics = diagnostics.checks;
                            self.diagnostics_error = None;
                        }
                        Err(error) => self.diagnostics_error = Some(error),
                    }
                }
                UiEvent::Transfers(result) => {
                    self.transfer_refresh_pending = false;
                    self.last_transfer_refresh = Instant::now();
                    match result {
                        Ok(transfers) => {
                            self.transfers = transfers.transfers;
                            self.transfers_error = None;
                        }
                        Err(error) => self.transfers_error = Some(error),
                    }
                }
                UiEvent::Share { generation, result } => {
                    match self.share_generation.complete(generation) {
                        ShareCompletion::Apply => self.finish_pending(PendingScope::Share),
                        ShareCompletion::Discard => {
                            self.finish_pending(PendingScope::Share);
                            continue;
                        }
                        ShareCompletion::Ignore => continue,
                    }
                    match result {
                        Ok(result) if result.shared => {
                            self.share_inspection = None;
                            self.notice = Some(Notice::success(format!(
                                "{} · transfer {} · content {}",
                                result.message,
                                result.transfer_id.as_deref().unwrap_or("unavailable"),
                                result.content_id.as_deref().unwrap_or("unavailable"),
                            )));
                            self.refresh_history();
                            self.refresh_transfers();
                        }
                        Ok(result) if result.confirmation_required => {
                            self.notice = None;
                            self.share_inspection = Some(result);
                        }
                        Ok(result) => {
                            self.notice = Some(Notice::error(result.message));
                        }
                        Err(error) => self.notice = Some(Notice::error(error)),
                    }
                }
                UiEvent::Mutation { kind, result } => {
                    self.finish_pending(kind.pending_scope());
                    match result {
                        Ok(result) if result.ok => {
                            self.notice = Some(Notice::success(result.message));
                            if activation_result_closes(kind, self.presentation) {
                                self.window_state = WindowState::Close;
                            } else if kind.refreshes_history() {
                                self.refresh_history();
                            }
                            if kind == MutationKind::TransferCancel {
                                self.pending_transfer_cancel = None;
                                self.refresh_transfers();
                                self.refresh_history();
                            }
                            if kind == MutationKind::ForgetDevice {
                                self.pending_forget_device = None;
                                self.forget_device_id.clear();
                                self.send(UiCommand::Peers);
                            }
                            if kind == MutationKind::Setting {
                                self.pending_setting = None;
                                self.mesh_quota_input.clear();
                                self.capture_threshold_input.clear();
                                self.send(UiCommand::Config);
                            }
                        }
                        Ok(_) => {
                            self.notice = Some(Notice::error(
                                "the daemon rejected the operation without a reason",
                            ));
                        }
                        Err(error) => self.notice = Some(Notice::error(error)),
                    }
                }
            }
        }
    }
}
