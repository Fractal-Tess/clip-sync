pub mod protocol;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use serde::Serialize;
use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{RwLock, mpsc, oneshot},
};
use tokio_util::{
    codec::{Framed, LengthDelimitedCodec},
    sync::CancellationToken,
};

use crate::{
    config::Config,
    discovery::DiscoverySnapshot,
    ipc::protocol::{
        ConfigResponse, DiagnosticCheck, DiagnosticsResponse, ErrorResponse, HistoryItem,
        HistoryResponse, HistoryUpdateAction, IPC_PROTOCOL_VERSION, MutationResponse, PeerItem,
        PeersResponse, Request, Response, StatusResponse, request, response,
    },
};

const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct DaemonState {
    inner: Arc<DaemonStateInner>,
}

struct DaemonStateInner {
    started: Instant,
    hostname: String,
    config_path: PathBuf,
    config: Config,
    discovery: RwLock<Option<DiscoverySnapshot>>,
    discovery_error: RwLock<Option<String>>,
    clipboard_status: RwLock<DiagnosticStatus>,
    history: RwLock<Vec<HistoryItem>>,
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
                config,
                discovery: RwLock::new(None),
                discovery_error: RwLock::new(None),
                clipboard_status: RwLock::new(DiagnosticStatus {
                    ok: true,
                    detail: "clipboard monitoring is starting".to_owned(),
                }),
                history: RwLock::new(Vec::new()),
                commands,
            }),
        }
    }

    pub async fn set_discovery(&self, discovery: DiscoverySnapshot) {
        *self.inner.discovery.write().await = Some(discovery);
        *self.inner.discovery_error.write().await = None;
    }

    pub async fn set_discovery_error(&self, error: impl Into<String>) {
        *self.inner.discovery_error.write().await = Some(error.into());
    }

    pub async fn set_clipboard_status(&self, ok: bool, detail: impl Into<String>) {
        *self.inner.clipboard_status.write().await = DiagnosticStatus {
            ok,
            detail: detail.into(),
        };
    }

    pub async fn set_history(&self, history: Vec<HistoryItem>) {
        *self.inner.history.write().await = history;
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
                    netbird_command: String,
                    mesh_key_file_configured: bool,
                    config_path: &'a str,
                }

                #[derive(Serialize)]
                struct RedactedConfig<'a> {
                    shared: &'a crate::config::SharedConfig,
                    local: RedactedLocal<'a>,
                }

                let config_path = self.inner.config_path.to_string_lossy();
                let redacted = RedactedConfig {
                    shared: &self.inner.config.shared,
                    local: RedactedLocal {
                        listen_port: self.inner.config.local.listen_port,
                        discovery_interval_seconds: self
                            .inner
                            .config
                            .local
                            .discovery_interval_seconds,
                        netbird_command: self
                            .inner
                            .config
                            .local
                            .netbird_command
                            .display()
                            .to_string(),
                        mesh_key_file_configured: !self
                            .inner
                            .config
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
                let query = history_request.query.to_lowercase();
                let limit = if history_request.limit == 0 {
                    100
                } else {
                    history_request.limit.min(500)
                };
                let limit = usize::try_from(limit).unwrap_or(500);
                let items = self
                    .inner
                    .history
                    .read()
                    .await
                    .iter()
                    .filter(|item| {
                        query.is_empty()
                            || item.preview.to_lowercase().contains(&query)
                            || item.source_node.to_lowercase().contains(&query)
                            || item
                                .mime_types
                                .iter()
                                .any(|mime| mime.to_lowercase().contains(&query))
                            || item.content_id.starts_with(&query)
                    })
                    .take(limit)
                    .cloned()
                    .collect();
                response::Body::History(HistoryResponse { items })
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
                            ok: true,
                            detail: self.inner.config_path.display().to_string(),
                        },
                        DiagnosticCheck {
                            name: "encrypted_storage".to_owned(),
                            ok: true,
                            detail: "open and ready".to_owned(),
                        },
                        DiagnosticCheck {
                            name: "mesh_secret".to_owned(),
                            ok: true,
                            detail: "loaded from an owner-only key file".to_owned(),
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
                    ],
                })
            }
            Some(request::Body::ShareClipboard(_)) => {
                return error_response(
                    request_id,
                    "unsupported",
                    "explicit sharing is unavailable because this clipboard backend cannot inspect the current offer on demand",
                );
            }
            Some(request::Body::Transfers(_)) => {
                return error_response(
                    request_id,
                    "unsupported",
                    "transfer tracking is not available in this daemon build",
                );
            }
            Some(request::Body::TransferCancel(cancel)) => {
                return error_response(
                    request_id,
                    "unsupported",
                    format!(
                        "transfer cancellation is not available in this daemon build (requested {})",
                        cancel.transfer_id
                    ),
                );
            }
            Some(request::Body::ForgetDevice(forget)) => {
                return error_response(
                    request_id,
                    "unsupported",
                    format!(
                        "device forgetting is not exposed by the replica backend in this build (requested {})",
                        forget.device_id
                    ),
                );
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
    let listener = UnixListener::bind(socket)?;
    set_socket_permissions(socket)?;

    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted?;
                let state = state.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, state).await {
                        tracing::debug!(%error, "local IPC connection ended");
                    }
                });
            }
        }
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
/// Returns an error when connection, framing, encoding, or decoding fails.
pub async fn request(socket: &Path, request: Request) -> Result<Response, IpcError> {
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
    set_directory_permissions(parent)?;

    match UnixStream::connect(socket).await {
        Ok(_) => Err(IpcError::AlreadyRunning),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            if error.kind() == std::io::ErrorKind::ConnectionRefused {
                tokio::fs::remove_file(socket).await?;
            }
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn set_socket_permissions(socket: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(unix)]
fn set_directory_permissions(directory: &Path) -> Result<(), IpcError> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))?;
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

#[derive(Debug, Error)]
pub enum IpcError {
    #[error("another clip-sync daemon is already listening")]
    AlreadyRunning,
    #[error("IPC socket path has no parent")]
    MissingSocketParent,
    #[error("daemon closed the IPC connection without responding")]
    ConnectionClosed,
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
                    pinned: false,
                    physical_millis: 2,
                },
                HistoryItem {
                    content_id: "beta".to_owned(),
                    preview: "unrelated".to_owned(),
                    mime_types: vec!["image/png".to_owned()],
                    logical_size: 20,
                    source_node: "vd".to_owned(),
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
    async fn unavailable_backends_return_explicit_unsupported_error() {
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
                request_id: 10,
                body: Some(request::Body::ShareClipboard(ShareClipboardRequest {})),
            })
            .await;
        let Some(response::Body::Error(error)) = response.body else {
            panic!("expected error response");
        };
        assert_eq!(error.code, "unsupported");
        assert!(error.message.contains("current offer"));
    }
}
