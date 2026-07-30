use tokio::sync::oneshot;

use super::{
    protocol::{
        HistoryUpdateAction, IPC_PROTOCOL_VERSION, MutationResponse, Request, Response,
        SharedSettingKind, TransfersResponse, request, response,
    },
    responses::{command_processor_stopped, command_processor_unavailable, error_response},
    state::{DaemonCommand, DaemonState},
};

impl DaemonState {
    #[allow(clippy::too_many_lines)]
    pub(super) async fn handle(&self, request: Request) -> Response {
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
            Some(request::Body::Status(_)) => self.status_response().await,
            Some(request::Body::Config(_)) => match self.config_response(request_id).await {
                Ok(body) => body,
                Err(response) => return response,
            },
            Some(request::Body::History(history_request)) => {
                match self.history_response(request_id, history_request).await {
                    Ok(body) => body,
                    Err(response) => return response,
                }
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
            Some(request::Body::Peers(_)) => self.peers_response().await,
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
            Some(request::Body::Diagnostics(_)) => self.diagnostics_response().await,
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
