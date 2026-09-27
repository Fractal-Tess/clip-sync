//! On-demand transfer of a reference's bytes from the device that copied it.
//!
//! The requester sends the content ID and the operation it holds; the origin
//! answers with a header and then the raw bytes of every file (or
//! representation) in reference order, each exactly as long as the reference
//! says. The requester knows those lengths, so no further framing is needed.

use std::path::{Path, PathBuf};

use quinn::{Connection, RecvStream, SendStream};
use tokio::{io::AsyncReadExt, sync::oneshot};
use uuid::Uuid;

use clip_sync_core::{
    files,
    model::{ContentId, OpId, Payload, Reference, Representation},
    storage::LocalSource,
};

use super::super::protocol::{
    FetchHeader, FetchRequest, MAX_FETCH_CONTROL_BYTES, STREAM_KIND_FETCH, read_message_bounded,
    write_message,
};
use super::{MeshError, MeshStoreRequest, PERSIST_TIMEOUT, RuntimeContext, SourceRequest};

/// Stream reset code telling the requester the origin could not finish.
const SOURCE_FAILED: u32 = 1;
const COPY_BUFFER_BYTES: usize = 256 * 1024;

/// Bytes fetched from a reference's origin.
#[derive(Debug)]
pub enum Fetched {
    /// The copied roots now live under this directory, owner-only.
    Files(PathBuf),
    Data(Payload),
}

/// Fetches a reference from `connection`, its origin. Files are written
/// under a fresh directory beside `destination` and only moved into place
/// once complete, so an interrupted fetch never looks finished.
pub(super) async fn fetch(
    connection: &Connection,
    content_id: ContentId,
    operation: OpId,
    reference: &Reference,
    destination: &Path,
) -> Result<Fetched, MeshError> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&[STREAM_KIND_FETCH]).await?;
    write_message(
        &mut send,
        &FetchRequest {
            content_id: content_id.as_bytes().to_vec(),
            counter: operation.counter(),
        },
    )
    .await?;
    send.finish()?;
    let header: FetchHeader = read_message_bounded(&mut recv, MAX_FETCH_CONTROL_BYTES).await?;
    if !header.available {
        return Err(MeshError::SourceUnavailable(header.reason));
    }

    match reference {
        Reference::Data(descriptors) => {
            let mut representations = Vec::with_capacity(descriptors.len());
            for descriptor in descriptors {
                let length =
                    usize::try_from(descriptor.byte_len()).map_err(|_| MeshError::FetchMismatch)?;
                let mut bytes = vec![0; length];
                recv.read_exact(&mut bytes).await?;
                representations.push(Representation::new(descriptor.mime(), bytes));
            }
            expect_end(&mut recv).await?;
            Payload::from_parts(content_id, representations)
                .map(Fetched::Data)
                .map_err(|_| MeshError::FetchMismatch)
        }
        Reference::Files(entries) => {
            let parent = destination.parent().ok_or(MeshError::FetchMismatch)?;
            tokio::fs::create_dir_all(parent).await?;
            let staging = parent.join(format!(".{}.{}.partial", content_id, Uuid::new_v4()));
            files::create_received_directory(&staging)?;
            let received = receive_files(&mut recv, entries, &staging).await;
            if let Err(error) = received {
                let _ = tokio::fs::remove_dir_all(&staging).await;
                return Err(error);
            }
            if tokio::fs::rename(&staging, destination).await.is_err() {
                // Another fetch of the same item finished first.
                let _ = tokio::fs::remove_dir_all(&staging).await;
            }
            Ok(Fetched::Files(destination.to_path_buf()))
        }
    }
}

async fn receive_files(
    recv: &mut RecvStream,
    entries: &[clip_sync_core::model::FileEntry],
    staging: &Path,
) -> Result<(), MeshError> {
    for entry in entries {
        let path =
            files::destination(staging, &entry.path).map_err(|_| MeshError::FetchMismatch)?;
        if entry.directory {
            files::create_received_directory(&path)?;
            continue;
        }
        let mut file =
            tokio::fs::File::from_std(files::create_received_file(&path, entry.executable)?);
        let copied = tokio::io::copy(&mut (&mut *recv).take(entry.size), &mut file).await?;
        if copied != entry.size {
            return Err(MeshError::FetchMismatch);
        }
        file.sync_all().await?;
    }
    expect_end(recv).await
}

async fn expect_end(recv: &mut RecvStream) -> Result<(), MeshError> {
    let mut extra = [0_u8; 1];
    match recv.read(&mut extra).await? {
        None => Ok(()),
        Some(_) => Err(MeshError::FetchMismatch),
    }
}

/// Answers a fetch from a peer with bytes this device authored.
pub(super) async fn serve_fetch(
    mut send: SendStream,
    mut recv: RecvStream,
    context: &RuntimeContext,
) -> Result<(), MeshError> {
    let request: FetchRequest = read_message_bounded(&mut recv, MAX_FETCH_CONTROL_BYTES).await?;
    let content_id = ContentId::from_bytes(
        request
            .content_id
            .as_slice()
            .try_into()
            .map_err(|_| MeshError::InvalidFetchRequest)?,
    );
    let operation = OpId::new(context.config.node_id, request.counter)
        .map_err(|_| MeshError::InvalidFetchRequest)?;

    let (reply, completed) = oneshot::channel();
    context
        .store_tx
        .send(MeshStoreRequest::Source(SourceRequest {
            content_id,
            operation,
            reply,
        }))
        .await
        .map_err(|_| MeshError::PersistenceUnavailable)?;
    let source = tokio::time::timeout(PERSIST_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::PersistenceTimeout)?
        .map_err(|_| MeshError::PersistenceUnavailable)?;
    // Check every file before answering, so a moved or edited file is
    // reported plainly instead of as a stream cut off half-way.
    let source = match source {
        Ok(source) => preflight(source).await,
        Err(reason) => Err(reason),
    };
    let source = match source {
        Ok(source) => source,
        Err(reason) => {
            write_message(
                &mut send,
                &FetchHeader {
                    available: false,
                    reason,
                },
            )
            .await?;
            send.finish()?;
            return Ok(());
        }
    };

    write_message(
        &mut send,
        &FetchHeader {
            available: true,
            reason: String::new(),
        },
    )
    .await?;
    if let Err(error) = send_source(&mut send, &source).await {
        let _ = send.reset(SOURCE_FAILED.into());
        return Err(error);
    }
    send.finish()?;
    send.stopped().await?;
    Ok(())
}

async fn preflight(source: LocalSource) -> Result<LocalSource, String> {
    let LocalSource::Files(sources) = source else {
        return Ok(source);
    };
    tokio::task::spawn_blocking(move || {
        for source in sources.iter().filter(|source| source.identity.is_some()) {
            files::open_source(source).map_err(|error| error.to_string())?;
        }
        Ok(LocalSource::Files(sources))
    })
    .await
    .map_err(|_| "could not check the copied files".to_owned())?
}

async fn send_source(send: &mut SendStream, source: &LocalSource) -> Result<(), MeshError> {
    match source {
        LocalSource::Data(payload) => {
            for representation in payload.representations() {
                send.write_all(representation.bytes()).await?;
            }
        }
        LocalSource::Files(sources) => {
            let mut buffer = vec![0; COPY_BUFFER_BYTES];
            for source in sources.iter().filter(|source| source.identity.is_some()) {
                let expected = source.identity.map_or(0, |identity| identity.size);
                let opened = {
                    let source = source.clone();
                    tokio::task::spawn_blocking(move || files::open_source(&source))
                        .await
                        .map_err(|_| MeshError::FetchMismatch)?
                };
                let mut file = tokio::fs::File::from_std(opened.map_err(|error| {
                    tracing::debug!(%error, "copied file can no longer be served");
                    MeshError::SourceUnavailable(error.to_string())
                })?);
                let mut remaining = expected;
                while remaining > 0 {
                    let want = usize::try_from(remaining.min(COPY_BUFFER_BYTES as u64))
                        .unwrap_or(COPY_BUFFER_BYTES);
                    let read = file.read(&mut buffer[..want]).await?;
                    if read == 0 {
                        return Err(MeshError::FetchMismatch);
                    }
                    send.write_all(&buffer[..read]).await?;
                    remaining -= read as u64;
                }
                // A file edited while it was being read may have sent a mix
                // of old and new bytes; refuse rather than deliver that.
                let unchanged = {
                    let source = source.clone();
                    tokio::task::spawn_blocking(move || files::open_source(&source).is_ok())
                        .await
                        .unwrap_or(false)
                };
                if !unchanged {
                    return Err(MeshError::FetchMismatch);
                }
            }
        }
    }
    Ok(())
}
