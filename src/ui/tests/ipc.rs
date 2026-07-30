use super::*;

#[tokio::test]
async fn unavailable_daemon_after_start_attempt_returns_actionable_error() {
    let temporary = tempfile::tempdir().expect("temporary directory");
    let socket = temporary.path().join("missing.sock");
    let error = wait_for_daemon_ready(
        &socket,
        daemon_readiness_probe(),
        1,
        Duration::ZERO,
        "test start",
    )
    .await
    .expect_err("missing daemon must fail");

    assert!(error.contains(&socket.display().to_string()));
    assert!(error.contains("clip-sync daemon"));
}

#[test]
fn pending_state_gates_share_against_every_mutation() {
    assert_eq!(
        UiCommand::ShareClipboard {
            confirmed: true,
            generation: 9,
        }
        .pending_scope(),
        Some(PendingScope::Share)
    );
    assert_eq!(
        UiCommand::Activate {
            content_id: "content".to_owned(),
            kind: MutationKind::ActivateQuick,
        }
        .pending_scope(),
        Some(PendingScope::HistoryMutation)
    );
    let mut pending = HashSet::new();
    pending.insert(PendingScope::Share);
    assert_eq!(
        mutation_block_reason(&pending, PendingScope::HistoryMutation),
        Some("Wait for clipboard sharing to finish before making another change.")
    );
    assert_eq!(
        mutation_block_reason(&pending, PendingScope::TransferCancel),
        Some("Wait for clipboard sharing to finish before making another change.")
    );

    pending.clear();
    pending.insert(PendingScope::HistoryMutation);
    assert_eq!(
        mutation_block_reason(&pending, PendingScope::Share),
        Some("Wait for the pending change to finish before sharing the clipboard.")
    );
    assert!(mutation_block_reason(&pending, PendingScope::TransferCancel).is_none());
    assert_eq!(
        UiCommand::History {
            query: String::new(),
            generation: 1,
        }
        .pending_scope(),
        None
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "the concurrent mock-daemon scenario is clearest as one end-to-end test"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn confirmed_share_keeps_history_responsive_and_gates_mutations() {
    use bytes::Bytes;
    use futures_util::{SinkExt as _, StreamExt as _};
    use prost::Message as _;
    use tokio::net::UnixListener;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let temporary = tempfile::tempdir().expect("temporary directory");
    let socket = temporary.path().join("daemon.sock");
    let listener = UnixListener::bind(&socket).expect("bind mock daemon");
    let server = tokio::spawn(async move {
        let mut connections = tokio::task::JoinSet::new();
        for _ in 0..3 {
            let (stream, _) = listener.accept().await.expect("accept UI request");
            connections.spawn(async move {
                let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
                let frame = framed
                    .next()
                    .await
                    .expect("request frame")
                    .expect("read request");
                let request = Request::decode(frame.freeze()).expect("decode request");
                let body = match request.body {
                    Some(request::Body::ShareClipboard(_)) => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        response::Body::ShareClipboard(ShareClipboardResponse {
                            shared: true,
                            confirmation_required: true,
                            logical_size: 1,
                            mime_types: vec!["text/plain".to_owned()],
                            quota_exempt: false,
                            transfer_id: Some("transfer".to_owned()),
                            content_id: Some("content".to_owned()),
                            message: "shared".to_owned(),
                        })
                    }
                    Some(request::Body::History(_)) => response::Body::History(HistoryResponse {
                        items: vec![history_item("fresh")],
                    }),
                    Some(request::Body::Activate(_)) => {
                        response::Body::Mutation(MutationResponse {
                            ok: true,
                            message: "activated".to_owned(),
                            resource_id: Some("fresh".to_owned()),
                        })
                    }
                    _ => panic!("unexpected mock request"),
                };
                let response = Response {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    body: Some(body),
                };
                let mut encoded = Vec::new();
                response.encode(&mut encoded).expect("encode response");
                framed
                    .send(Bytes::from(encoded))
                    .await
                    .expect("send response");
            });
        }
        while let Some(result) = connections.join_next().await {
            result.expect("mock connection task");
        }
    });

    let (event_tx, event_rx) = std_mpsc::channel();
    let worker = spawn_ipc_worker(socket, event_tx, egui::Context::default());
    worker
        .send(UiCommand::ShareClipboard {
            confirmed: true,
            generation: 11,
        })
        .expect("queue share");
    worker
        .send(UiCommand::History {
            query: String::new(),
            generation: 7,
        })
        .expect("queue history");
    worker
        .send(UiCommand::Activate {
            content_id: "fresh".to_owned(),
            kind: MutationKind::Activate,
        })
        .expect("queue activation");

    assert!(matches!(
        event_rx
            .recv_timeout(Duration::from_millis(200))
            .expect("History remains responsive"),
        UiEvent::History {
            generation: 7,
            result: Ok(_)
        }
    ));
    assert!(
        event_rx.recv_timeout(Duration::from_millis(50)).is_err(),
        "the mutation must not be dispatched while Share is pending"
    );
    assert!(matches!(
        event_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("share completes"),
        UiEvent::Share {
            generation: 11,
            result: Ok(_)
        }
    ));
    assert!(matches!(
        event_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("gated mutation completes after Share"),
        UiEvent::Mutation {
            kind: MutationKind::Activate,
            result: Ok(_)
        }
    ));
    drop(worker);
    server.await.expect("mock daemon task");
}

#[allow(
    clippy::too_many_lines,
    reason = "the cold-start concurrency scenario is clearest as one end-to-end test"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cold_start_gate_does_not_serialize_requests_behind_a_slow_share() {
    use bytes::Bytes;
    use futures_util::{SinkExt as _, StreamExt as _};
    use prost::Message as _;
    use tokio::net::UnixListener;
    use tokio_util::codec::{Framed, LengthDelimitedCodec};

    let temporary = tempfile::tempdir().expect("temporary directory");
    let socket = temporary.path().join("daemon.sock");
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let listener = UnixListener::bind(&server_socket).expect("bind delayed mock daemon");
        let mut connections = tokio::task::JoinSet::new();
        for _ in 0..4 {
            let (stream, _) = listener.accept().await.expect("accept UI request");
            connections.spawn(async move {
                let mut framed = Framed::new(stream, LengthDelimitedCodec::new());
                let frame = framed
                    .next()
                    .await
                    .expect("request frame")
                    .expect("read request");
                let request = Request::decode(frame.freeze()).expect("decode request");
                let body = match request.body {
                    Some(request::Body::Status(_)) => response::Body::Status(StatusResponse {
                        version: "test".to_owned(),
                        hostname: "test".to_owned(),
                        uptime_seconds: 1,
                        config_path: "/tmp/test".to_owned(),
                        netbird_address: None,
                        discovered_peers: 0,
                    }),
                    Some(request::Body::ShareClipboard(_)) => {
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        response::Body::ShareClipboard(ShareClipboardResponse {
                            shared: true,
                            confirmation_required: false,
                            logical_size: 1,
                            mime_types: vec!["text/plain".to_owned()],
                            quota_exempt: false,
                            transfer_id: Some("transfer".to_owned()),
                            content_id: Some("content".to_owned()),
                            message: "shared".to_owned(),
                        })
                    }
                    Some(request::Body::History(_)) => response::Body::History(HistoryResponse {
                        items: vec![history_item("fresh")],
                    }),
                    _ => panic!("unexpected mock request"),
                };
                let response = Response {
                    protocol_version: IPC_PROTOCOL_VERSION,
                    request_id: request.request_id,
                    body: Some(body),
                };
                let mut encoded = Vec::new();
                response.encode(&mut encoded).expect("encode response");
                framed
                    .send(Bytes::from(encoded))
                    .await
                    .expect("send response");
            });
        }
        while let Some(result) = connections.join_next().await {
            result.expect("mock connection task");
        }
    });

    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let share_socket = socket.clone();
    let share_gate = Arc::clone(&gate);
    let share = tokio::spawn(async move {
        request_with_shared_daemon_start(
            &share_socket,
            Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 1,
                body: Some(request::Body::ShareClipboard(ShareClipboardRequest {
                    confirmed: true,
                })),
            },
            &share_gate,
        )
        .await
    });
    let history_socket = socket;
    let history_gate = Arc::clone(&gate);
    let history = tokio::spawn(async move {
        request_with_shared_daemon_start(
            &history_socket,
            Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 2,
                body: Some(request::Body::History(HistoryRequest {
                    query: String::new(),
                    limit: 10,
                })),
            },
            &history_gate,
        )
        .await
    });

    let history_response = tokio::time::timeout(Duration::from_secs(1), history)
        .await
        .expect("History is not serialized behind Share")
        .expect("History task")
        .expect("History response");
    assert!(matches!(
        history_response.body,
        Some(response::Body::History(_))
    ));
    assert!(!share.is_finished(), "slow Share is still in flight");
    assert!(matches!(
        share
            .await
            .expect("Share task")
            .expect("Share response")
            .body,
        Some(response::Body::ShareClipboard(_))
    ));
    server.await.expect("mock daemon task");
}
