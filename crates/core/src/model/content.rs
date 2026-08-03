use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use thiserror::Error;

const CONTENT_DOMAIN: &[u8] = b"clip-sync/content-id/v1\0";
pub const MAX_PAYLOAD_REPRESENTATIONS: usize = 128;
pub const MAX_PAYLOAD_MIME_BYTES: usize = 256;

/// Keyed identity of an exact set of clipboard MIME representations.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ContentId([u8; blake3::OUT_LEN]);

impl ContentId {
    /// Computes an ID from exact MIME names and bytes in canonical MIME order.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate MIME names or an unrepresentable length.
    pub fn from_representations(
        key: &[u8; blake3::KEY_LEN],
        representations: &[Representation],
    ) -> Result<Self, ContentError> {
        let ordered = canonical_order(representations)?;
        let mut hasher = blake3::Hasher::new_keyed(key);
        hasher.update(CONTENT_DOMAIN);
        hash_u64(&mut hasher, usize_to_u64(ordered.len())?);

        for representation in ordered {
            hash_u64(&mut hasher, usize_to_u64(representation.mime.len())?);
            hasher.update(representation.mime.as_bytes());
            hash_u64(&mut hasher, usize_to_u64(representation.bytes.len())?);
            hasher.update(&representation.bytes);
        }

        Ok(Self(*hasher.finalize().as_bytes()))
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; blake3::OUT_LEN] {
        &self.0
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ContentId")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl FromStr for ContentId {
    type Err = ContentIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != blake3::OUT_LEN * 2 {
            return Err(ContentIdParseError::WrongLength(value.len()));
        }

        let mut bytes = [0; blake3::OUT_LEN];
        hex::decode_to_slice(value, &mut bytes).map_err(ContentIdParseError::InvalidHex)?;
        Ok(Self(bytes))
    }
}

impl Serialize for ContentId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ContentId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        encoded.parse().map_err(de::Error::custom)
    }
}

/// One exact MIME representation. Debug output intentionally excludes bytes.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Representation {
    mime: String,
    bytes: Vec<u8>,
}

impl Representation {
    pub fn new(mime: impl Into<String>, bytes: impl Into<Vec<u8>>) -> Self {
        Self {
            mime: mime.into(),
            bytes: bytes.into(),
        }
    }

    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl fmt::Debug for Representation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Representation")
            .field("mime", &self.mime)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepresentationDescriptor {
    mime: String,
    byte_len: u64,
}

impl RepresentationDescriptor {
    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    #[must_use]
    pub const fn byte_len(&self) -> u64 {
        self.byte_len
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PayloadDescriptor {
    content_id: ContentId,
    logical_size: u64,
    representations: Vec<RepresentationDescriptor>,
}

impl PayloadDescriptor {
    #[must_use]
    pub const fn content_id(&self) -> ContentId {
        self.content_id
    }

    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub fn representations(&self) -> &[RepresentationDescriptor] {
        &self.representations
    }
}

/// Milestone-1 payload container. Debug output is descriptor-only, ensuring
/// clipboard bytes cannot be emitted accidentally through normal diagnostics.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Payload {
    descriptor: PayloadDescriptor,
    representations: Vec<Representation>,
}

impl Payload {
    /// Builds a descriptor and stores representations in canonical MIME order.
    ///
    /// # Errors
    ///
    /// Returns an error for duplicate MIME names, aggregate size overflow, or
    /// an individual length that cannot be represented.
    pub fn new(
        key: &[u8; blake3::KEY_LEN],
        mut representations: Vec<Representation>,
    ) -> Result<Self, ContentError> {
        validate_representation_set(&representations)?;
        representations
            .sort_unstable_by(|left, right| left.mime.as_bytes().cmp(right.mime.as_bytes()));
        reject_duplicate_mime(&representations)?;

        let logical_size = representations
            .iter()
            .try_fold(0_u64, |total, representation| {
                let byte_len = usize_to_u64(representation.bytes.len())?;
                total
                    .checked_add(byte_len)
                    .ok_or(ContentError::SizeOverflow)
            })?;
        let descriptors = representations
            .iter()
            .map(|representation| {
                Ok(RepresentationDescriptor {
                    mime: representation.mime.clone(),
                    byte_len: usize_to_u64(representation.bytes.len())?,
                })
            })
            .collect::<Result<Vec<_>, ContentError>>()?;
        let content_id = ContentId::from_representations(key, &representations)?;

        Ok(Self {
            descriptor: PayloadDescriptor {
                content_id,
                logical_size,
                representations: descriptors,
            },
            representations,
        })
    }

    #[must_use]
    pub const fn descriptor(&self) -> &PayloadDescriptor {
        &self.descriptor
    }

    #[must_use]
    pub fn representations(&self) -> &[Representation] {
        &self.representations
    }

    /// Revalidates data after crossing an untrusted serialization boundary.
    ///
    /// # Errors
    ///
    /// Returns an error if bytes are noncanonical, duplicate a MIME name, have
    /// invalid sizes, or do not match the serialized descriptor.
    pub fn validate(&self, key: &[u8; blake3::KEY_LEN]) -> Result<(), ContentError> {
        self.validate_structure()?;
        let content_id = ContentId::from_representations(key, &self.representations)?;
        if content_id != self.descriptor.content_id {
            return Err(ContentError::DescriptorMismatch);
        }
        Ok(())
    }

    /// Validates serialization-level invariants without requiring the
    /// mesh-secret-derived content key.
    ///
    /// # Errors
    ///
    /// Rejects empty/oversized representation sets, invalid MIME names,
    /// noncanonical ordering, and descriptor mismatches.
    pub fn validate_structure(&self) -> Result<(), ContentError> {
        validate_representation_set(&self.representations)?;
        if self
            .representations
            .windows(2)
            .any(|pair| pair[0].mime.as_bytes() >= pair[1].mime.as_bytes())
        {
            return Err(ContentError::NonCanonicalRepresentations);
        }

        if self.descriptor.representations.len() != self.representations.len() {
            return Err(ContentError::DescriptorMismatch);
        }
        let mut logical_size = 0_u64;
        for (descriptor, representation) in self
            .descriptor
            .representations
            .iter()
            .zip(&self.representations)
        {
            let byte_len = usize_to_u64(representation.bytes.len())?;
            logical_size = logical_size
                .checked_add(byte_len)
                .ok_or(ContentError::SizeOverflow)?;
            if descriptor.mime != representation.mime || descriptor.byte_len != byte_len {
                return Err(ContentError::DescriptorMismatch);
            }
        }
        if self.descriptor.logical_size != logical_size {
            return Err(ContentError::DescriptorMismatch);
        }
        Ok(())
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Payload")
            .field("descriptor", &self.descriptor)
            .field("representations", &"<redacted>")
            .finish_non_exhaustive()
    }
}

fn canonical_order(
    representations: &[Representation],
) -> Result<Vec<&Representation>, ContentError> {
    let mut ordered: Vec<_> = representations.iter().collect();
    ordered.sort_unstable_by(|left, right| left.mime.as_bytes().cmp(right.mime.as_bytes()));

    reject_duplicate_mime_refs(&ordered)?;
    Ok(ordered)
}

fn reject_duplicate_mime(representations: &[Representation]) -> Result<(), ContentError> {
    for adjacent in representations.windows(2) {
        if adjacent[0].mime == adjacent[1].mime {
            return Err(ContentError::DuplicateMime(adjacent[0].mime.clone()));
        }
    }
    Ok(())
}

fn reject_duplicate_mime_refs(representations: &[&Representation]) -> Result<(), ContentError> {
    for adjacent in representations.windows(2) {
        if adjacent[0].mime == adjacent[1].mime {
            return Err(ContentError::DuplicateMime(adjacent[0].mime.clone()));
        }
    }
    Ok(())
}

fn validate_representation_set(representations: &[Representation]) -> Result<(), ContentError> {
    if representations.is_empty() {
        return Err(ContentError::EmptyPayload);
    }
    if representations.len() > MAX_PAYLOAD_REPRESENTATIONS {
        return Err(ContentError::TooManyRepresentations {
            count: representations.len(),
            max: MAX_PAYLOAD_REPRESENTATIONS,
        });
    }
    for representation in representations {
        if representation.mime.is_empty()
            || representation.mime.len() > MAX_PAYLOAD_MIME_BYTES
            || representation.mime.bytes().any(|byte| byte == 0)
        {
            return Err(ContentError::InvalidMime(representation.mime.clone()));
        }
    }
    Ok(())
}

fn hash_u64(hasher: &mut blake3::Hasher, value: u64) {
    hasher.update(&value.to_be_bytes());
}

fn usize_to_u64(value: usize) -> Result<u64, ContentError> {
    value.try_into().map_err(|_| ContentError::SizeOverflow)
}

#[derive(Debug, Error)]
pub enum ContentIdParseError {
    #[error("content ID must contain 64 hexadecimal characters, got {0}")]
    WrongLength(usize),
    #[error("content ID is not valid hexadecimal")]
    InvalidHex(#[source] hex::FromHexError),
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ContentError {
    #[error("a payload must contain at least one MIME representation")]
    EmptyPayload,
    #[error("payload has {count} representations, exceeding the {max} limit")]
    TooManyRepresentations { count: usize, max: usize },
    #[error("payload contains invalid MIME type {0:?}")]
    InvalidMime(String),
    #[error("a payload cannot contain MIME type {0:?} more than once")]
    DuplicateMime(String),
    #[error("payload representations are not in canonical MIME order")]
    NonCanonicalRepresentations,
    #[error("payload size cannot be represented")]
    SizeOverflow,
    #[error("payload descriptor does not match its exact representation bytes")]
    DescriptorMismatch,
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    const KEY: [u8; blake3::KEY_LEN] = [7; blake3::KEY_LEN];

    fn plain(bytes: &[u8]) -> Representation {
        Representation::new("text/plain", bytes)
    }

    #[test]
    fn canonical_identity_ignores_input_order() {
        let first = vec![
            Representation::new("text/plain", b"hello"),
            Representation::new("text/html", b"<b>hello</b>"),
        ];
        let second = vec![first[1].clone(), first[0].clone()];

        assert_eq!(
            ContentId::from_representations(&KEY, &first).unwrap(),
            ContentId::from_representations(&KEY, &second).unwrap()
        );
        assert_eq!(
            Payload::new(&KEY, first).unwrap(),
            Payload::new(&KEY, second).unwrap()
        );
    }

    #[test]
    fn lengths_make_canonical_encoding_unambiguous() {
        let split_one = vec![
            Representation::new("a", b"bc"),
            Representation::new("def", b""),
        ];
        let split_two = vec![
            Representation::new("ab", b"c"),
            Representation::new("def", b""),
        ];
        assert_ne!(
            ContentId::from_representations(&KEY, &split_one).unwrap(),
            ContentId::from_representations(&KEY, &split_two).unwrap()
        );
    }

    #[test]
    fn duplicate_mime_is_rejected() {
        let representations = vec![plain(b"one"), plain(b"two")];
        assert!(matches!(
            Payload::new(&KEY, representations),
            Err(ContentError::DuplicateMime(mime)) if mime == "text/plain"
        ));
    }

    #[test]
    fn malformed_representation_sets_are_rejected() {
        assert!(matches!(
            Payload::new(&KEY, Vec::new()),
            Err(ContentError::EmptyPayload)
        ));
        assert!(matches!(
            Payload::new(&KEY, vec![Representation::new("", b"value")]),
            Err(ContentError::InvalidMime(_))
        ));
        assert!(matches!(
            Payload::new(&KEY, vec![Representation::new("text/\0plain", b"value")]),
            Err(ContentError::InvalidMime(_))
        ));
    }

    #[test]
    fn debug_never_contains_representation_bytes() {
        let secret = "secret-clipboard-value";
        let payload = Payload::new(&KEY, vec![plain(secret.as_bytes())]).unwrap();
        assert!(!format!("{payload:?}").contains(secret));
        assert!(!format!("{:?}", payload.representations()[0]).contains(secret));
    }

    #[test]
    fn content_id_text_round_trip() {
        let id = ContentId::from_representations(&KEY, &[plain(b"hello")]).unwrap();
        assert_eq!(id.to_string().parse::<ContentId>().unwrap(), id);
    }

    proptest! {
        #[test]
        fn any_exact_byte_change_changes_identity(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
            let original = ContentId::from_representations(&KEY, &[plain(&bytes)]).unwrap();
            let mut changed = bytes;
            changed.push(0);
            let changed = ContentId::from_representations(&KEY, &[plain(&changed)]).unwrap();
            prop_assert_ne!(original, changed);
        }

        #[test]
        fn keyed_ids_are_domain_private(bytes in prop::collection::vec(any::<u8>(), 0..2048)) {
            let other_key = [8; blake3::KEY_LEN];
            let first = ContentId::from_representations(&KEY, &[plain(&bytes)]).unwrap();
            let second = ContentId::from_representations(&other_key, &[plain(&bytes)]).unwrap();
            prop_assert_ne!(first, second);
        }
    }
}
