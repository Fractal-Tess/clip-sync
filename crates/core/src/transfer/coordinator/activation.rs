use tokio_util::sync::CancellationToken;

use crate::{
    clipboard::types::{ClipboardContent, ClipboardRepresentation, MimeType, MimeTypeError},
    model::{ContentId, Payload, Projection, Representation},
    payload::{ManifestId, StoredManifest},
};

use super::{ActivatedClipboard, TransferCoordinator, TransferCoordinatorError};

impl TransferCoordinator {
    /// Reconstructs an authenticated MIME bundle or safe file snapshot.
    ///
    /// # Errors
    ///
    /// Returns unavailable/cancelled content, authentication, identity,
    /// clipboard validation, free-space, or materialization errors.
    pub fn activate(
        &self,
        content_id: ContentId,
        projection: &Projection,
        content_key: &[u8; 32],
        maximum_bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<ActivatedClipboard, TransferCoordinatorError> {
        let (_, manifest_id, _) = projection
            .completed_manifest_for_content(content_id)
            .ok_or(TransferCoordinatorError::ContentUnavailable(content_id))?;
        let manifest = self.store.manifest(manifest_id)?;
        match manifest {
            StoredManifest::MimeBundle(bundle) => {
                let representations = self
                    .store
                    .read_mime_bundle(&bundle, cancellation)?
                    .into_iter()
                    .map(|(mime, bytes)| Representation::new(mime, bytes))
                    .collect::<Vec<_>>();
                let payload = Payload::new(content_key, representations)?;
                if payload.descriptor().content_id() != content_id {
                    return Err(TransferCoordinatorError::ContentIdentityMismatch);
                }
                let content = payload
                    .representations()
                    .iter()
                    .map(|representation| {
                        Ok(ClipboardRepresentation::new(
                            MimeType::new(representation.mime())?,
                            representation.bytes(),
                        ))
                    })
                    .collect::<Result<Vec<_>, MimeTypeError>>()?;
                Ok(ActivatedClipboard {
                    content: ClipboardContent::new_with_limit(content, maximum_bytes)?,
                    materialized_manifest: None,
                })
            }
            StoredManifest::Files(_) => {
                let materialization =
                    self.materializer
                        .materialize(&self.store, manifest_id, cancellation)?;
                let content = ClipboardContent::new_with_limit(
                    vec![ClipboardRepresentation::new(
                        MimeType::new("text/uri-list")?,
                        materialization.uri_list(),
                    )],
                    maximum_bytes,
                )?;
                Ok(ActivatedClipboard {
                    content,
                    materialized_manifest: Some(manifest_id),
                })
            }
            StoredManifest::Blob(_) => Err(TransferCoordinatorError::UnsupportedManifest),
        }
    }

    /// Cleans a prior file activation after clipboard ownership changes.
    ///
    /// # Errors
    ///
    /// Returns safe materialization cleanup errors.
    pub fn cleanup_materialization(
        &self,
        manifest_id: ManifestId,
    ) -> Result<bool, TransferCoordinatorError> {
        self.materializer.cleanup(manifest_id).map_err(Into::into)
    }
}
