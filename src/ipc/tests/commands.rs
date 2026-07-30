use tokio::sync::mpsc;

use crate::{
    config::Config,
    ipc::{
        DaemonCommand, DaemonState,
        protocol::{
            self, HistoryUpdateAction, HistoryUpdateRequest, IPC_PROTOCOL_VERSION, Request,
            ShareClipboardRequest, request, response,
        },
    },
};

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
