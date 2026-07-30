use std::{path::Path, time::Duration};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::UnixStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

use super::{
    IpcError,
    protocol::{IPC_PROTOCOL_VERSION, Request, Response, request},
};

pub(super) const MAX_IPC_FRAME_BYTES: usize = 1024 * 1024;
pub(super) const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
pub(super) const IPC_SHARE_REQUEST_TIMEOUT: Duration = Duration::from_mins(30);

/// Sends one request to the daemon and waits for its response.
///
/// # Errors
///
/// Returns an error when connection, framing, encoding, decoding, response
/// correlation, or the bounded response wait fails.
pub async fn request(socket: &Path, request: Request) -> Result<Response, IpcError> {
    let request_id = request.request_id;
    let timeout = request_timeout(&request);
    let response = tokio::time::timeout(timeout, request_inner(socket, request))
        .await
        .map_err(|_| IpcError::Timeout)??;
    if response.protocol_version != IPC_PROTOCOL_VERSION {
        return Err(IpcError::ResponseProtocol {
            expected: IPC_PROTOCOL_VERSION,
            actual: response.protocol_version,
        });
    }
    if response.request_id != request_id {
        return Err(IpcError::ResponseRequestId {
            expected: request_id,
            actual: response.request_id,
        });
    }
    Ok(response)
}

pub(super) fn request_timeout(request: &Request) -> Duration {
    if matches!(
        request.body.as_ref(),
        Some(request::Body::ShareClipboard(_))
    ) {
        IPC_SHARE_REQUEST_TIMEOUT
    } else {
        IPC_REQUEST_TIMEOUT
    }
}

async fn request_inner(socket: &Path, request: Request) -> Result<Response, IpcError> {
    let stream = UnixStream::connect(socket).await?;
    let mut framed = Framed::new(stream, codec());
    let mut encoded = Vec::with_capacity(request.encoded_len());
    request.encode(&mut encoded)?;
    framed.send(Bytes::from(encoded)).await?;

    let frame = framed.next().await.ok_or(IpcError::ConnectionClosed)??;
    Ok(Response::decode(frame.freeze())?)
}

pub(super) fn codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_IPC_FRAME_BYTES)
        .new_codec()
}
