//! Validated clipboard representations and content snapshots.

use std::{collections::HashSet, sync::Arc};

use thiserror::Error;

use super::{
    capture::{Generation, MAX_CAPTURE_BYTES},
    feedback::FeedbackMarker,
    mime::{MAX_MIME_TYPES_PER_OFFER, MimeType, OfferError, OfferMimeList},
};

/// One lazily served or captured MIME representation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardRepresentation {
    mime_type: MimeType,
    bytes: Arc<[u8]>,
}

impl ClipboardRepresentation {
    /// Builds a representation from a validated MIME type and owned bytes.
    #[must_use]
    pub fn new(mime_type: MimeType, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            mime_type,
            bytes: Arc::from(bytes.into().into_boxed_slice()),
        }
    }

    /// Returns the representation MIME type.
    #[must_use]
    pub fn mime_type(&self) -> &MimeType {
        &self.mime_type
    }

    /// Returns the representation bytes.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns a cheap shared clone of the representation bytes.
    #[must_use]
    pub fn bytes_arc(&self) -> Arc<[u8]> {
        self.bytes.clone()
    }

    /// Builds a representation from already shared bytes.
    #[must_use]
    pub fn from_shared_bytes(mime_type: MimeType, bytes: Arc<[u8]>) -> Self {
        Self { mime_type, bytes }
    }
}

/// A bounded set of clipboard MIME byte representations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardContent {
    representations: Vec<ClipboardRepresentation>,
}

/// Size-only snapshot of the current live offer used for explicit sharing.
#[derive(Clone, Debug)]
pub struct CurrentClipboardInspection {
    generation: Generation,
    mime_list: OfferMimeList,
    logical_size: u64,
}

impl CurrentClipboardInspection {
    #[must_use]
    pub const fn new(generation: Generation, mime_list: OfferMimeList, logical_size: u64) -> Self {
        Self {
            generation,
            mime_list,
            logical_size,
        }
    }

    #[must_use]
    pub const fn generation(&self) -> Generation {
        self.generation
    }

    #[must_use]
    pub const fn mime_list(&self) -> &OfferMimeList {
        &self.mime_list
    }

    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }
}

impl ClipboardContent {
    /// Builds clipboard content after enforcing count, uniqueness, and size
    /// bounds.
    ///
    /// # Errors
    ///
    /// Returns an error when there are no usable representations, too many MIME
    /// types, duplicate MIME names, internal marker MIME names, or more than
    /// [`MAX_CAPTURE_BYTES`] aggregate bytes.
    pub fn new(
        representations: Vec<ClipboardRepresentation>,
    ) -> Result<Self, ClipboardContentError> {
        Self::new_with_max(representations, MAX_CAPTURE_BYTES)
    }

    /// Builds clipboard content with a caller-provided aggregate byte limit.
    ///
    /// This is used by the daemon's replicated automatic-capture threshold,
    /// confirmed explicit shares, and activation.
    ///
    /// # Errors
    ///
    /// Returns the same structural errors as [`Self::new`], or
    /// [`ClipboardContentError::TooLarge`] when `max_bytes` is zero or
    /// exceeded.
    pub fn new_with_max(
        representations: Vec<ClipboardRepresentation>,
        max_bytes: u64,
    ) -> Result<Self, ClipboardContentError> {
        if max_bytes == 0 {
            return Err(ClipboardContentError::TooLarge { total_bytes: 0 });
        }
        if representations.is_empty() {
            return Err(ClipboardContentError::Empty);
        }
        if representations.len() > MAX_MIME_TYPES_PER_OFFER {
            return Err(ClipboardContentError::TooManyMimeTypes {
                count: representations.len(),
                max: MAX_MIME_TYPES_PER_OFFER,
            });
        }

        let mut names = HashSet::with_capacity(representations.len());
        let mut total_bytes = 0_u64;
        for representation in &representations {
            if FeedbackMarker::from_mime_type(representation.mime_type()).is_some() {
                return Err(ClipboardContentError::InternalMimeType {
                    mime_type: representation.mime_type().to_string(),
                });
            }
            if !names.insert(representation.mime_type().to_string()) {
                return Err(ClipboardContentError::DuplicateMimeType {
                    mime_type: representation.mime_type().to_string(),
                });
            }
            total_bytes = total_bytes
                .saturating_add(u64::try_from(representation.bytes().len()).unwrap_or(u64::MAX));
        }

        if total_bytes > max_bytes {
            return Err(ClipboardContentError::TooLarge { total_bytes });
        }

        Ok(Self { representations })
    }

    /// Compatibility alias for explicit-share callers.
    ///
    /// # Errors
    ///
    /// Returns the same validation errors as [`Self::new_with_max`].
    pub fn new_with_limit(
        representations: Vec<ClipboardRepresentation>,
        maximum_bytes: u64,
    ) -> Result<Self, ClipboardContentError> {
        Self::new_with_max(representations, maximum_bytes)
    }

    /// Returns the MIME byte representations in offer order.
    #[must_use]
    pub fn representations(&self) -> &[ClipboardRepresentation] {
        &self.representations
    }

    /// Returns the aggregate byte size across all representations.
    #[must_use]
    pub fn total_bytes(&self) -> u64 {
        self.representations
            .iter()
            .map(|representation| u64::try_from(representation.bytes().len()).unwrap_or(u64::MAX))
            .fold(0_u64, u64::saturating_add)
    }

    /// Returns the bytes for a MIME type if this content can serve it.
    #[must_use]
    pub fn bytes_for_mime(&self, mime_type: &str) -> Option<Arc<[u8]>> {
        self.representations
            .iter()
            .find(|representation| representation.mime_type().as_str() == mime_type)
            .map(ClipboardRepresentation::bytes_arc)
    }

    /// Returns a MIME-only view of the user-visible representations.
    ///
    /// # Errors
    ///
    /// Propagates the bounded offer error if the content somehow exceeds MIME
    /// count limits.
    pub fn mime_list(&self) -> Result<OfferMimeList, OfferError> {
        OfferMimeList::new(
            self.representations
                .iter()
                .map(|representation| representation.mime_type().clone())
                .collect(),
        )
    }
}

/// Errors when constructing [`ClipboardContent`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ClipboardContentError {
    #[error("clipboard content must contain at least one MIME representation")]
    Empty,
    #[error("clipboard content has {count} MIME types, exceeding the {max} limit")]
    TooManyMimeTypes { count: usize, max: usize },
    #[error("clipboard content contains duplicate MIME type {mime_type}")]
    DuplicateMimeType { mime_type: String },
    #[error("clipboard content uses internal feedback MIME type {mime_type}")]
    InternalMimeType { mime_type: String },
    #[error("clipboard content is {total_bytes} bytes, exceeding the capture limit")]
    TooLarge { total_bytes: u64 },
}
