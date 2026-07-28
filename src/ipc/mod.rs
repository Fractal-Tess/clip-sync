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
        ConfigResponse, ErrorResponse, HistoryItem, HistoryResponse, IPC_PROTOCOL_VERSION,
        MutationResponse, Request, Response, StatusResponse, request, response,
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
    history: RwLock<Vec<HistoryItem>>,
    commands: mpsc::UnboundedSender<DaemonCommand>,
}

pub enum DaemonCommand {
    Activate {
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
                history: RwLock::new(Vec::new()),
                commands,
            }),
        }
    }

    pub async fn set_discovery(&self, discovery: DiscoverySnapshot) {
        *self.inner.discovery.write().await = Some(discovery);
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
                struct RedactedConfig<'a> {
                    shared: &'a crate::config::SharedConfig,
                    listen_port: u16,
                    discovery_interval_seconds: u64,
                    netbird_command: String,
                    mesh_key_file_configured: bool,
                }

                let redacted = RedactedConfig {
                    shared: &self.inner.config.shared,
                    listen_port: self.inner.config.local.listen_port,
                    discovery_interval_seconds: self.inner.config.local.discovery_interval_seconds,
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
                let (reply, result) = oneshot::channel();
                if self
                    .inner
                    .commands
                    .send(DaemonCommand::Activate {
                        content_id: activate.content_id,
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
                    Ok(Ok(())) => response::Body::Mutation(MutationResponse { ok: true }),
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
    use protocol::{StatusRequest, request};

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

        shutdown.cancel();
        server.await.expect("server task");
    }
}
