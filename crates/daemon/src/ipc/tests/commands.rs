use tokio::sync::mpsc;

use clip_sync_core::config::Config;
use clip_sync_ipc::protocol::{
    self, HistoryUpdateAction, HistoryUpdateRequest, IPC_PROTOCOL_VERSION, Request, request,
    response,
};

use crate::ipc::{DaemonCommand, DaemonState};

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
async fn activation_reports_what_the_daemon_did() {
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
                request_id: 20,
                body: Some(request::Body::Activate(protocol::ActivateRequest {
                    content_id: "content".to_owned(),
                })),
            })
            .await
    });
    let DaemonCommand::Activate { content_id, reply } =
        command_rx.recv().await.expect("activate command")
    else {
        panic!("expected activate command");
    };
    assert_eq!(content_id, "content");
    reply
        .send(Ok("fetching from kiwi".to_owned()))
        .expect("activate reply");
    let Some(response::Body::Mutation(mutation)) = handler.await.expect("handler").body else {
        panic!("expected mutation response");
    };
    assert_eq!(mutation.message, "fetching from kiwi");
}

#[tokio::test]
async fn device_forget_round_trips_through_daemon_command() {
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
}
