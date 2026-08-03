//! Validated, bounded clipboard MIME offers.

use std::{collections::HashSet, fmt};

use thiserror::Error;

use super::feedback::FeedbackMarker;

/// Maximum number of MIME types we accept in a single offer.
///
/// Compositors can advertise an arbitrary number of types; we cap at a
/// reasonable bound to avoid unbounded allocations from a misbehaving source.
pub const MAX_MIME_TYPES_PER_OFFER: usize = 128;

/// Maximum byte length of a single MIME type string.
///
/// RFC 6838 limits type/subtype to 127 octets each plus the slash, and
/// parameters can extend further, but real-world clipboard types stay well
/// under 256 bytes.
pub const MAX_MIME_NAME_BYTES: usize = 256;

/// A validated MIME type name from a clipboard offer.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MimeType(pub(super) String);

impl MimeType {
    /// Validates and wraps a MIME type string.
    ///
    /// # Errors
    ///
    /// Returns an error if the name is empty, exceeds [`MAX_MIME_NAME_BYTES`],
    /// or contains a NUL byte (which would be truncated by the Wayland wire
    /// format).
    pub fn new(name: impl Into<String>) -> Result<Self, MimeTypeError> {
        let name = name.into();
        if name.is_empty() {
            return Err(MimeTypeError::Empty);
        }
        if name.len() > MAX_MIME_NAME_BYTES {
            return Err(MimeTypeError::TooLong {
                len: name.len(),
                max: MAX_MIME_NAME_BYTES,
            });
        }
        if name.bytes().any(|b| b == 0) {
            return Err(MimeTypeError::ContainsNul);
        }
        Ok(Self(name))
    }

    /// Returns the validated name.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("MimeType").field(&self.0).finish()
    }
}

impl fmt::Display for MimeType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Errors when constructing a [`MimeType`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum MimeTypeError {
    #[error("MIME type name must not be empty")]
    Empty,
    #[error("MIME type name is {len} bytes, exceeding the {max}-byte limit")]
    TooLong { len: usize, max: usize },
    #[error("MIME type name must not contain NUL bytes")]
    ContainsNul,
}

/// A snapshot of the MIME types advertised by a single clipboard offer.
#[derive(Clone, Debug)]
pub struct OfferMimeList {
    types: Vec<MimeType>,
}

impl OfferMimeList {
    /// Builds an offer from accumulated MIME type events.
    ///
    /// # Errors
    ///
    /// Returns an error if the count exceeds [`MAX_MIME_TYPES_PER_OFFER`].
    pub fn new(types: Vec<MimeType>) -> Result<Self, OfferError> {
        let mut seen = HashSet::with_capacity(types.len());
        let types = types
            .into_iter()
            .filter(|mime_type| seen.insert(mime_type.clone()))
            .collect::<Vec<_>>();
        if types.len() > MAX_MIME_TYPES_PER_OFFER {
            return Err(OfferError::TooManyMimeTypes {
                count: types.len(),
                max: MAX_MIME_TYPES_PER_OFFER,
            });
        }
        Ok(Self { types })
    }

    /// Returns the MIME types in the order they were advertised.
    #[must_use]
    pub fn types(&self) -> &[MimeType] {
        &self.types
    }

    /// Number of MIME types in this offer.
    #[must_use]
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Whether this offer advertises zero MIME types.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Returns this list without internal feedback marker MIME types.
    ///
    /// The marker is an implementation detail for loop prevention and should
    /// not become part of captured user clipboard content.
    #[must_use]
    pub fn without_feedback_markers(&self) -> Self {
        let types = self
            .types
            .iter()
            .filter(|mime_type| FeedbackMarker::from_mime_type(mime_type).is_none())
            .cloned()
            .collect();
        Self { types }
    }

    /// Finds the first internal feedback marker advertised by this offer.
    #[must_use]
    pub fn feedback_marker(&self) -> Option<FeedbackMarker> {
        self.types.iter().find_map(FeedbackMarker::from_mime_type)
    }
}

/// Errors when constructing an [`OfferMimeList`].
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum OfferError {
    #[error("offer has {count} MIME types, exceeding the {max} limit")]
    TooManyMimeTypes { count: usize, max: usize },
}

/// Bounded MIME accumulator for a Wayland offer.
#[derive(Clone, Debug, Default)]
pub struct BoundedMimeOffer {
    types: Vec<MimeType>,
    invalid_count: usize,
    truncated_count: usize,
}

impl BoundedMimeOffer {
    /// Adds one MIME name, keeping only the bounded, valid prefix.
    pub fn push(&mut self, mime_type: String) {
        let Ok(mime_type) = MimeType::new(mime_type) else {
            self.invalid_count += 1;
            return;
        };

        if self.types.contains(&mime_type) {
            return;
        }

        if self.types.len() == MAX_MIME_TYPES_PER_OFFER {
            self.truncated_count += 1;
            return;
        }

        self.types.push(mime_type);
    }

    /// Finishes the offer into a bounded MIME list.
    ///
    /// # Errors
    ///
    /// Propagates [`OfferError`] if the internal bound is violated.
    pub fn finish(self) -> Result<OfferMimeList, OfferError> {
        OfferMimeList::new(self.types)
    }

    /// Number of invalid MIME names ignored while collecting this offer.
    #[must_use]
    pub fn invalid_count(&self) -> usize {
        self.invalid_count
    }

    /// Number of valid MIME names ignored after the offer reached the limit.
    #[must_use]
    pub fn truncated_count(&self) -> usize {
        self.truncated_count
    }
}
