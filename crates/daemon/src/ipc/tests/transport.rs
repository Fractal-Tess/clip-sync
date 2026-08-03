use std::time::Duration;

use clip_sync_core::config::Config;
use clip_sync_ipc::{
    IpcError,
    protocol::{IPC_PROTOCOL_VERSION, Request, StatusRequest, request as request_body, response},
    request,
};
use tokio::{
    net::{UnixListener, UnixStream},
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use crate::ipc::{DaemonInstance, DaemonState, security::prepare_socket, serve};

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
        tokio::time::sleep(Duration::from_millis(2)).await;
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
            body: Some(request_body::Body::Status(StatusRequest {})),
        },
    )
    .await
    .expect("status response");

    assert_eq!(response.request_id, 7);
    let Some(response::Body::Status(status)) = response.body else {
        panic!("expected status response");
    };
    assert_eq!(status.hostname, "test-node");

    shutdown.cancel();
    server.await.expect("server task");
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
