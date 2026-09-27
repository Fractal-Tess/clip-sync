use std::{path::Path, time::Duration};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use super::{
    IpcError,
    framing::codec,
    protocol::{IPC_PROTOCOL_VERSION, Request, Response},
};

const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Sends one request to the daemon and waits for its response.
///
/// # Errors
///
/// Returns an error when connection, framing, encoding, decoding, response
/// correlation, or the bounded response wait fails.
pub async fn request(socket: &Path, request: Request) -> Result<Response, IpcError> {
    let request_id = request.request_id;
    let response = tokio::time::timeout(IPC_REQUEST_TIMEOUT, request_inner(socket, request))
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

async fn request_inner(socket: &Path, request: Request) -> Result<Response, IpcError> {
    let stream = UnixStream::connect(socket).await?;
    let mut framed = Framed::new(stream, codec());
    let mut encoded = Vec::with_capacity(request.encoded_len());
    request.encode(&mut encoded)?;
    framed.send(Bytes::from(encoded)).await?;

    let frame = framed.next().await.ok_or(IpcError::ConnectionClosed)??;
    Ok(Response::decode(frame.freeze())?)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use futures_util::{SinkExt, StreamExt};
    use prost::Message;
    use tokio::net::UnixListener;
    use tokio_util::codec::Framed;

    use super::*;
    use crate::{
        framing::codec,
        protocol::{StatusRequest, StatusResponse, request, response},
    };

    #[tokio::test]
    async fn client_rejects_mismatched_response_request_id() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let socket = temporary.path().join("daemon.sock");
        let listener = UnixListener::bind(&socket).expect("bind test socket");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let mut framed = Framed::new(stream, codec());
            let _request = framed
                .next()
                .await
                .expect("request frame")
                .expect("valid frame");
            let response = Response {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 999,
                body: Some(response::Body::Status(StatusResponse {
                    version: "test".to_owned(),
                    hostname: "test".to_owned(),
                    uptime_seconds: 0,
                    config_path: "test".to_owned(),
                    local_addresses: Vec::new(),
                    discovered_peers: 0,
                    connected_peers: 0,
                })),
            };
            let mut encoded = Vec::with_capacity(response.encoded_len());
            response.encode(&mut encoded).expect("encode response");
            framed
                .send(Bytes::from(encoded))
                .await
                .expect("send response");
        });

        let error = request(
            &socket,
            Request {
                protocol_version: IPC_PROTOCOL_VERSION,
                request_id: 12,
                body: Some(request::Body::Status(StatusRequest {})),
            },
        )
        .await
        .expect_err("mismatched response must fail");

        assert!(matches!(
            error,
            IpcError::ResponseRequestId {
                expected: 12,
                actual: 999
            }
        ));
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .expect("test server timeout")
            .expect("test server");
    }
}
