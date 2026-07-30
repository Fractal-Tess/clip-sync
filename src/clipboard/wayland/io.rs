//! Asynchronous serving of daemon-owned clipboard payloads.

use std::{
    io::{self, ErrorKind},
    os::fd::OwnedFd,
    sync::Arc,
};

use tokio::{io::unix::AsyncFd, task};
use tokio_util::sync::CancellationToken;

pub(super) fn spawn_source_writer(
    mime_type: String,
    fd: OwnedFd,
    payload: Arc<[u8]>,
    shutdown: CancellationToken,
    permit: tokio::sync::OwnedSemaphorePermit,
) {
    task::spawn(async move {
        let _permit = permit;
        if let Err(error) = write_payload(fd, &payload, &shutdown).await {
            tracing::debug!(
                mime_type,
                error = %error,
                "failed to serve daemon-owned clipboard MIME data"
            );
        }
    });
}

async fn write_payload(
    fd: OwnedFd,
    payload: &[u8],
    shutdown: &CancellationToken,
) -> Result<(), io::Error> {
    use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};

    let flags = fcntl_getfl(&fd).map_err(io::Error::from)?;
    fcntl_setfl(&fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)?;
    let async_fd = AsyncFd::new(fd)?;
    let mut offset = 0;
    while offset < payload.len() {
        let writable = tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            result = async_fd.writable() => result?,
        };
        let mut writable = writable;
        match writable.try_io(|inner| {
            rustix::io::write(inner.get_ref(), &payload[offset..]).map_err(Into::into)
        }) {
            Ok(Ok(0)) => {
                return Err(io::Error::new(
                    ErrorKind::WriteZero,
                    "clipboard destination accepted zero bytes",
                ));
            }
            Ok(Ok(written)) => offset += written,
            Ok(Err(error)) => return Err(error),
            Err(_) => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{os::unix::net::UnixStream, time::Duration};

    use super::*;

    #[tokio::test]
    async fn blocked_source_writer_observes_shutdown() {
        let (_read_end, write_end) = UnixStream::pair().expect("socket pair");
        let fd: OwnedFd = write_end.into();
        let shutdown = CancellationToken::new();
        let writer_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            let payload = vec![0x5a; 8 * 1024 * 1024];
            write_payload(fd, &payload, &writer_shutdown).await
        });

        tokio::time::sleep(Duration::from_millis(20)).await;
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(1), writer)
            .await
            .expect("writer stopped after cancellation")
            .expect("writer task")
            .expect("cancellation is a clean writer stop");
    }
}
