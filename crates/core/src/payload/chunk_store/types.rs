use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use zeroize::Zeroizing;

use super::super::FileSnapshot;
use super::{ChunkStoreError, DEFAULT_CHUNK_BYTES, DEFAULT_MAX_CHUNKS, DEFAULT_MAX_PAYLOAD_BYTES};

#[derive(Clone)]
pub struct ChunkStoreKey(pub(super) Zeroizing<[u8; 32]>);

impl ChunkStoreKey {
    #[must_use]
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ChunkStoreKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ChunkStoreKey([REDACTED])")
    }
}

macro_rules! digest_id {
    ($name:ident) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub(super) [u8; 32]);

        impl $name {
            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&self.to_string())
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&hex::encode(self.0))
            }
        }

        impl FromStr for $name {
            type Err = ChunkStoreError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if value.len() != 64 {
                    return Err(ChunkStoreError::InvalidIdentifier);
                }
                let mut bytes = [0; 32];
                hex::decode_to_slice(value, &mut bytes)
                    .map_err(|_| ChunkStoreError::InvalidIdentifier)?;
                Ok(Self(bytes))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.to_string())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                String::deserialize(deserializer)?
                    .parse()
                    .map_err(de::Error::custom)
            }
        }
    };
}

digest_id!(ChunkId);
digest_id!(ManifestId);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkRef {
    pub(super) id: ChunkId,
    pub(super) logical_size: u32,
}

impl ChunkRef {
    #[must_use]
    pub const fn from_parts(id: ChunkId, logical_size: u32) -> Self {
        Self { id, logical_size }
    }

    #[must_use]
    pub const fn id(&self) -> ChunkId {
        self.id
    }

    #[must_use]
    pub const fn logical_size(&self) -> u32 {
        self.logical_size
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobManifest {
    pub(super) logical_size: u64,
    pub(super) chunks: Vec<ChunkRef>,
}

impl BlobManifest {
    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub fn chunks(&self) -> &[ChunkRef] {
        &self.chunks
    }
}

/// One MIME representation stored as an ordered encrypted blob.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MimeBlob {
    pub(super) mime: String,
    pub(super) blob: BlobManifest,
}

impl MimeBlob {
    #[must_use]
    pub fn mime(&self) -> &str {
        &self.mime
    }

    #[must_use]
    pub const fn blob(&self) -> &BlobManifest {
        &self.blob
    }
}

/// Canonical multi-MIME clipboard payload whose bytes live in encrypted chunks.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MimeBundleManifest {
    pub(super) logical_size: u64,
    pub(super) representations: Vec<MimeBlob>,
}

impl MimeBundleManifest {
    #[must_use]
    pub const fn logical_size(&self) -> u64 {
        self.logical_size
    }

    #[must_use]
    pub fn representations(&self) -> &[MimeBlob] {
        &self.representations
    }
}

/// Encrypted manifest body retained in the `SQLCipher` catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum StoredManifest {
    Blob(BlobManifest),
    MimeBundle(MimeBundleManifest),
    Files(FileSnapshot),
}

impl StoredManifest {
    #[must_use]
    pub fn logical_size(&self) -> u64 {
        match self {
            Self::Blob(blob) => blob.logical_size(),
            Self::MimeBundle(bundle) => bundle.logical_size(),
            Self::Files(files) => files.logical_size(),
        }
    }

    pub(crate) fn visit_chunks(&self, mut visitor: impl FnMut(&ChunkRef)) {
        match self {
            Self::Blob(blob) => {
                for chunk in blob.chunks() {
                    visitor(chunk);
                }
            }
            Self::MimeBundle(bundle) => {
                for representation in bundle.representations() {
                    for chunk in representation.blob().chunks() {
                        visitor(chunk);
                    }
                }
            }
            Self::Files(files) => {
                for entry in files.entries() {
                    if let Some(blob) = entry.blob() {
                        for chunk in blob.chunks() {
                            visitor(chunk);
                        }
                    }
                }
            }
        }
    }

    /// Returns the canonical chunk references used by this manifest.
    #[must_use]
    pub fn chunks(&self) -> Vec<ChunkRef> {
        let mut chunks = Vec::new();
        self.visit_chunks(|chunk| chunks.push(chunk.clone()));
        chunks
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChunkStoreConfig {
    pub chunk_bytes: usize,
    pub max_payload_bytes: u64,
    pub max_chunks_per_manifest: usize,
}

impl Default for ChunkStoreConfig {
    fn default() -> Self {
        Self {
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            max_chunks_per_manifest: DEFAULT_MAX_CHUNKS,
        }
    }
}
