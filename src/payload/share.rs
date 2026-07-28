use std::{io::Read, path::PathBuf};

use thiserror::Error;
use tokio_util::sync::CancellationToken;

use super::{
    ChunkStore, ChunkStoreError, FileSnapshotError, FileSnapshotLimits, ManifestId, StoredManifest,
    snapshot_file_uris,
};

/// Resource policy evaluated before explicit clipboard streaming begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplicitSharePolicy {
    pub automatic_capture_threshold_bytes: u64,
    pub mesh_quota_bytes: u64,
    /// Hard local safety ceiling even for quota-exempt shares.
    pub maximum_explicit_share_bytes: u64,
    pub free_space_reserve_bytes: u64,
}

impl ExplicitSharePolicy {
    /// Inspects a live offer using its preflight logical size.
    ///
    /// This performs no streaming or chunk allocation. Call [`Self::authorize`]
    /// only after presenting the returned warning state to the user.
    ///
    /// # Errors
    ///
    /// Returns a typed resource/limit error before any share begins.
    pub fn inspect(
        self,
        logical_size: u64,
        available_space: u64,
    ) -> Result<ExplicitShareInspection, ExplicitShareError> {
        self.validate()?;
        if logical_size == 0 {
            return Err(ExplicitShareError::Empty);
        }
        if logical_size > self.maximum_explicit_share_bytes {
            return Err(ExplicitShareError::HardLimit {
                observed: logical_size,
                maximum: self.maximum_explicit_share_bytes,
            });
        }
        let required_space = logical_size
            .checked_add(self.free_space_reserve_bytes)
            .ok_or(ExplicitShareError::SizeOverflow)?;
        if available_space < required_space {
            return Err(ExplicitShareError::InsufficientSpace {
                required: required_space,
                available: available_space,
            });
        }
        Ok(ExplicitShareInspection {
            logical_size,
            required_space,
            confirmation_required: logical_size > self.automatic_capture_threshold_bytes,
            quota_exempt: logical_size > self.mesh_quota_bytes,
        })
    }

    /// Converts an inspection into an allocation decision. Oversized content is
    /// never authorized until `confirmed` is true.
    ///
    /// # Errors
    ///
    /// Returns [`ExplicitShareError::ConfirmationRequired`] when appropriate.
    pub fn authorize(
        self,
        inspection: ExplicitShareInspection,
        confirmed: bool,
    ) -> Result<ExplicitShareDecision, ExplicitShareError> {
        self.validate()?;
        if inspection.confirmation_required && !confirmed {
            return Err(ExplicitShareError::ConfirmationRequired);
        }
        Ok(ExplicitShareDecision {
            logical_size: inspection.logical_size,
            quota_exempt: inspection.quota_exempt,
        })
    }

    pub(crate) fn validate(self) -> Result<(), ExplicitShareError> {
        if self.automatic_capture_threshold_bytes == 0
            || self.mesh_quota_bytes == 0
            || self.maximum_explicit_share_bytes == 0
            || self.maximum_explicit_share_bytes < self.automatic_capture_threshold_bytes
        {
            return Err(ExplicitShareError::InvalidPolicy);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplicitShareInspection {
    logical_size: u64,
    required_space: u64,
    confirmation_required: bool,
    quota_exempt: bool,
}

impl ExplicitShareInspection {
    #[must_use]
    pub const fn logical_size(self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub const fn required_space(self) -> u64 {
        self.required_space
    }

    #[must_use]
    pub const fn confirmation_required(self) -> bool {
        self.confirmation_required
    }

    #[must_use]
    pub const fn quota_exempt(self) -> bool {
        self.quota_exempt
    }

    #[must_use]
    pub fn human_size(self) -> String {
        human_size(self.logical_size)
    }
}

/// Proof that warning/confirmation and preflight checks completed before capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExplicitShareDecision {
    logical_size: u64,
    quota_exempt: bool,
}

impl ExplicitShareDecision {
    #[must_use]
    pub const fn logical_size(self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub const fn quota_exempt(self) -> bool {
        self.quota_exempt
    }

    /// Streams and atomically commits an arbitrary explicit payload only after
    /// authorization has produced this decision.
    ///
    /// The observed byte count must equal the inspected count. On failure,
    /// unreferenced chunks are reclaimed before returning where possible.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, size changes, storage, encryption,
    /// or cleanup failures.
    pub fn capture_blob(
        self,
        store: &mut ChunkStore,
        reader: &mut impl Read,
        cancellation: &CancellationToken,
    ) -> Result<CapturedExplicitShare, ExplicitShareCaptureError> {
        let blob = match store.stage_reader(reader, self.logical_size, cancellation) {
            Ok(blob) => blob,
            Err(error) => {
                let _ = store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        if blob.logical_size() != self.logical_size {
            store.cleanup_unreferenced()?;
            return Err(ExplicitShareCaptureError::SourceSizeChanged {
                inspected: self.logical_size,
                observed: blob.logical_size(),
            });
        }
        let manifest = StoredManifest::Blob(blob);
        let manifest_id = store.commit_manifest(&manifest)?;
        Ok(CapturedExplicitShare {
            manifest_id,
            manifest,
            quota_exempt: self.quota_exempt,
        })
    }

    /// Snapshots and atomically commits explicitly authorized file URIs.
    ///
    /// # Errors
    ///
    /// Returns an error for cancellation, source mutation, safety checks, size
    /// changes, chunk storage, or cleanup failures.
    pub fn capture_files(
        self,
        paths: &[PathBuf],
        store: &mut ChunkStore,
        limits: FileSnapshotLimits,
        cancellation: &CancellationToken,
    ) -> Result<CapturedExplicitShare, ExplicitShareCaptureError> {
        let snapshot = match snapshot_file_uris(paths, store, limits, cancellation) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = store.cleanup_unreferenced();
                return Err(error.into());
            }
        };
        if snapshot.logical_size() != self.logical_size {
            store.cleanup_unreferenced()?;
            return Err(ExplicitShareCaptureError::SourceSizeChanged {
                inspected: self.logical_size,
                observed: snapshot.logical_size(),
            });
        }
        let manifest = StoredManifest::Files(snapshot);
        let manifest_id = store.commit_manifest(&manifest)?;
        Ok(CapturedExplicitShare {
            manifest_id,
            manifest,
            quota_exempt: self.quota_exempt,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedExplicitShare {
    manifest_id: ManifestId,
    manifest: StoredManifest,
    quota_exempt: bool,
}

impl CapturedExplicitShare {
    #[must_use]
    pub const fn manifest_id(&self) -> ManifestId {
        self.manifest_id
    }

    #[must_use]
    pub const fn manifest(&self) -> &StoredManifest {
        &self.manifest
    }

    #[must_use]
    pub fn logical_size(&self) -> u64 {
        self.manifest.logical_size()
    }

    #[must_use]
    pub const fn quota_exempt(&self) -> bool {
        self.quota_exempt
    }
}

fn human_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    if bytes >= GIB {
        format_units(bytes, GIB, "GiB")
    } else if bytes >= MIB {
        format_units(bytes, MIB, "MiB")
    } else if bytes >= KIB {
        format_units(bytes, KIB, "KiB")
    } else {
        format!("{bytes} B")
    }
}

fn format_units(bytes: u64, unit: u64, label: &str) -> String {
    let whole = bytes / unit;
    let hundredths = (bytes % unit) * 100 / unit;
    format!("{whole}.{hundredths:02} {label}")
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum ExplicitShareError {
    #[error("explicit-share policy is invalid")]
    InvalidPolicy,
    #[error("clipboard offer is empty")]
    Empty,
    #[error("clipboard offer is {observed} bytes, exceeding the hard {maximum}-byte limit")]
    HardLimit { observed: u64, maximum: u64 },
    #[error("share size overflow")]
    SizeOverflow,
    #[error("local storage has {available} bytes free but the share requires {required} bytes")]
    InsufficientSpace { required: u64, available: u64 },
    #[error("explicit confirmation is required before capture begins")]
    ConfirmationRequired,
}

#[derive(Debug, Error)]
pub enum ExplicitShareCaptureError {
    #[error("clipboard source changed size after inspection: expected {inspected}, got {observed}")]
    SourceSizeChanged { inspected: u64, observed: u64 },
    #[error(transparent)]
    ChunkStore(#[from] ChunkStoreError),
    #[error(transparent)]
    FileSnapshot(#[from] FileSnapshotError),
}
