use std::{path::Path, sync::Arc};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::Semaphore,
    task::JoinSet,
};
use tokio_util::{codec::Framed, sync::CancellationToken};

use super::protocol::Request;
use super::{
    IpcError,
    client::{MAX_IPC_FRAME_BYTES, codec},
    responses::error_response,
    security::{peer_is_current_user, prepare_socket, remove_socket, set_socket_permissions},
    state::DaemonState,
};

const MAX_IPC_CONNECTIONS: usize = 32;

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
    let listener = match UnixListener::bind(socket) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            return Err(IpcError::AlreadyRunning);
        }
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = set_socket_permissions(socket) {
        let _ = remove_socket(socket).await;
        return Err(error);
    }

    let limiter = Arc::new(Semaphore::new(MAX_IPC_CONNECTIONS));
    let mut tasks = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break Ok(()),
            joined = tasks.join_next(), if !tasks.is_empty() => {
                if let Some(Err(error)) = joined
                    && !error.is_cancelled()
                {
                    tracing::debug!(%error, "local IPC connection task failed");
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(accepted) => accepted,
                    Err(error) => break Err(IpcError::Io(error)),
                };
                if !peer_is_current_user(&stream)? {
                    continue;
                }
                let Ok(permit) = limiter.clone().try_acquire_owned() else {
                    continue;
                };
                let state = state.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if let Err(error) = serve_connection(stream, state).await {
                        tracing::debug!(%error, "local IPC connection ended");
                    }
                });
            }
        }
    };

    drop(listener);
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    let cleanup = remove_socket(socket).await;
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), cleanup) => cleanup,
    }
}

async fn serve_connection(stream: UnixStream, state: DaemonState) -> Result<(), IpcError> {
    let mut framed = Framed::new(stream, codec());
    while let Some(frame) = framed.next().await {
        let request = Request::decode(frame?.freeze())?;
        let response = state.handle(request).await;
        if response.encoded_len() > MAX_IPC_FRAME_BYTES {
            let response = error_response(
                response.request_id,
                "response_too_large",
                "response exceeds the local IPC frame limit",
            );
            let mut encoded = Vec::with_capacity(response.encoded_len());
            response.encode(&mut encoded)?;
            framed.send(Bytes::from(encoded)).await?;
            continue;
        }
        let mut encoded = Vec::with_capacity(response.encoded_len());
        response.encode(&mut encoded)?;
        framed.send(Bytes::from(encoded)).await?;
    }
    Ok(())
}
