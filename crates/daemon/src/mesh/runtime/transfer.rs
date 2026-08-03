use std::sync::Arc;

use quinn::Connection;
use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
    time::{MissedTickBehavior, timeout},
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use clip_sync_core::transfer::{TransferChunk, TransferId};

use super::super::protocol::{
    ChunkStreamRequest, ChunkStreamResponse, MAX_CHUNK_CONTROL_BYTES, MAX_ENCRYPTED_CHUNK_BYTES,
    STREAM_KIND_CHUNK, read_message_bounded, write_message,
};
use super::{
    CHUNK_BROKER_TIMEOUT, MAX_MISSING_CHUNKS_PER_ROUND, MeshChunkCommand, MeshError, RuntimeContext,
};

pub(super) async fn initiate_chunk_streams(
    connection: &Connection,
    context: &RuntimeContext,
    shutdown: CancellationToken,
) -> Result<(), MeshError> {
    let Some(_) = &context.chunk_tx else {
        std::future::pending::<()>().await;
        return Ok(());
    };
    let mut interval = tokio::time::interval(context.config.reconcile_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut revision = context.revision.subscribe();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return Ok(()),
            _ = interval.tick() => {}
            result = revision.changed() => {
                if result.is_err() {
                    return Ok(());
                }
            }
        }
        let missing = broker_missing(context, MAX_MISSING_CHUNKS_PER_ROUND).await?;
        let semaphore = Arc::new(Semaphore::new(context.config.max_concurrent_chunk_streams));
        let mut tasks = JoinSet::new();
        for request in missing {
            let permit = semaphore
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| MeshError::PersistenceUnavailable)?;
            let connection = connection.clone();
            let chunk_tx = context
                .chunk_tx
                .as_ref()
                .expect("checked transfer broker")
                .clone();
            tasks.spawn(async move {
                let _permit = permit;
                request_chunk(&connection, &chunk_tx, request).await
            });
        }
        while let Some(result) = tasks.join_next().await {
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(%error, "chunk request did not complete"),
                Err(error) => tracing::debug!(%error, "chunk request task failed"),
            }
        }
    }
}

async fn request_chunk(
    connection: &Connection,
    chunk_tx: &mpsc::Sender<MeshChunkCommand>,
    request: TransferChunk,
) -> Result<(), MeshError> {
    let (mut send, mut recv) = connection.open_bi().await?;
    send.write_all(&[STREAM_KIND_CHUNK]).await?;
    write_message(
        &mut send,
        &ChunkStreamRequest {
            transfer_id: request.transfer_id.as_uuid().as_bytes().to_vec(),
            manifest_id: request.manifest_id.as_bytes().to_vec(),
            chunk_id: request.chunk_id.as_bytes().to_vec(),
            logical_size: request.logical_size,
        },
    )
    .await?;
    let response: ChunkStreamResponse =
        read_message_bounded(&mut recv, MAX_CHUNK_CONTROL_BYTES).await?;
    validate_chunk_response(&response, request)?;
    if !response.available {
        send.finish()?;
        return Ok(());
    }
    let encrypted_size = usize::try_from(response.encrypted_size)
        .map_err(|_| MeshError::ChunkFrameTooLarge(usize::MAX))?;
    if encrypted_size == 0 || encrypted_size > MAX_ENCRYPTED_CHUNK_BYTES {
        return Err(MeshError::ChunkFrameTooLarge(encrypted_size));
    }
    let mut encrypted = vec![0_u8; encrypted_size];
    recv.read_exact(&mut encrypted).await?;
    send.finish()?;
    broker_import(chunk_tx, request, encrypted).await
}

pub(super) async fn answer_chunk(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
    context: &RuntimeContext,
) -> Result<(), MeshError> {
    let request: ChunkStreamRequest = read_message_bounded(recv, MAX_CHUNK_CONTROL_BYTES).await?;
    let request = parse_chunk_request(&request)?;
    let encrypted = match broker_export(context, request).await {
        Ok(encrypted) => encrypted,
        Err(error) => {
            tracing::debug!(%error, "requested chunk is unavailable");
            write_message(
                send,
                &ChunkStreamResponse {
                    available: false,
                    transfer_id: request.transfer_id.as_uuid().as_bytes().to_vec(),
                    chunk_id: request.chunk_id.as_bytes().to_vec(),
                    encrypted_size: 0,
                },
            )
            .await?;
            send.finish()?;
            return Ok(());
        }
    };
    if encrypted.is_empty() || encrypted.len() > MAX_ENCRYPTED_CHUNK_BYTES {
        return Err(MeshError::ChunkFrameTooLarge(encrypted.len()));
    }
    write_message(
        send,
        &ChunkStreamResponse {
            available: true,
            transfer_id: request.transfer_id.as_uuid().as_bytes().to_vec(),
            chunk_id: request.chunk_id.as_bytes().to_vec(),
            encrypted_size: u32::try_from(encrypted.len())
                .map_err(|_| MeshError::ChunkFrameTooLarge(encrypted.len()))?,
        },
    )
    .await?;
    send.write_all(&encrypted).await?;
    send.finish()?;
    Ok(())
}

async fn broker_missing(
    context: &RuntimeContext,
    maximum: usize,
) -> Result<Vec<TransferChunk>, MeshError> {
    let sender = context
        .chunk_tx
        .as_ref()
        .ok_or(MeshError::ChunkBrokerUnavailable)?;
    let (reply, completed) = oneshot::channel();
    sender
        .send(MeshChunkCommand::Missing { maximum, reply })
        .await
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?;
    timeout(CHUNK_BROKER_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::ChunkBrokerTimeout)?
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?
        .map_err(MeshError::ChunkBrokerRejected)
}

async fn broker_export(
    context: &RuntimeContext,
    request: TransferChunk,
) -> Result<Vec<u8>, MeshError> {
    let sender = context
        .chunk_tx
        .as_ref()
        .ok_or(MeshError::ChunkBrokerUnavailable)?;
    let (reply, completed) = oneshot::channel();
    sender
        .send(MeshChunkCommand::Export { request, reply })
        .await
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?;
    timeout(CHUNK_BROKER_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::ChunkBrokerTimeout)?
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?
        .map_err(MeshError::ChunkBrokerRejected)
}

async fn broker_import(
    sender: &mpsc::Sender<MeshChunkCommand>,
    request: TransferChunk,
    encrypted: Vec<u8>,
) -> Result<(), MeshError> {
    let (reply, completed) = oneshot::channel();
    sender
        .send(MeshChunkCommand::Import {
            request,
            encrypted,
            reply,
        })
        .await
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?;
    timeout(CHUNK_BROKER_TIMEOUT, completed)
        .await
        .map_err(|_| MeshError::ChunkBrokerTimeout)?
        .map_err(|_| MeshError::ChunkBrokerUnavailable)?
        .map_err(MeshError::ChunkBrokerRejected)
}

fn parse_chunk_request(message: &ChunkStreamRequest) -> Result<TransferChunk, MeshError> {
    if message.logical_size == 0 {
        return Err(MeshError::InvalidChunkRequest);
    }
    let transfer_id = TransferId::from_uuid(
        Uuid::from_slice(&message.transfer_id).map_err(|_| MeshError::InvalidChunkRequest)?,
    );
    let manifest_id = hex::encode(&message.manifest_id)
        .parse()
        .map_err(|_| MeshError::InvalidChunkRequest)?;
    let chunk_id = hex::encode(&message.chunk_id)
        .parse()
        .map_err(|_| MeshError::InvalidChunkRequest)?;
    Ok(TransferChunk {
        transfer_id,
        manifest_id,
        chunk_id,
        logical_size: message.logical_size,
    })
}

fn validate_chunk_response(
    response: &ChunkStreamResponse,
    request: TransferChunk,
) -> Result<(), MeshError> {
    if response.transfer_id != request.transfer_id.as_uuid().as_bytes()
        || response.chunk_id != request.chunk_id.as_bytes()
        || response.available != (response.encrypted_size != 0)
    {
        return Err(MeshError::InvalidChunkResponse);
    }
    Ok(())
}
