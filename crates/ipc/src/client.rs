use std::{path::Path, time::Duration};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use prost::Message;
use tokio::net::UnixStream;
use tokio_util::codec::Framed;

use super::{
    IpcError,
    framing::codec,
    protocol::{IPC_PROTOCOL_VERSION, Request, Response, request},
};

const IPC_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const IPC_SHARE_REQUEST_TIMEOUT: Duration = Duration::from_mins(30);

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
        protocol::{ShareClipboardRequest, StatusRequest, StatusResponse, response},
    };

    #[test]
    fn share_requests_keep_a_finite_extended_deadline() {
        let share = Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 1,
            body: Some(request::Body::ShareClipboard(ShareClipboardRequest {
                confirmed: true,
            })),
        };
        let status = Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id: 2,
            body: Some(request::Body::Status(StatusRequest {})),
        };

        assert_eq!(request_timeout(&share), IPC_SHARE_REQUEST_TIMEOUT);
        assert_eq!(request_timeout(&status), IPC_REQUEST_TIMEOUT);
        assert!(request_timeout(&share) > request_timeout(&status));
    }

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
