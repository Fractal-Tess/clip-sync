//! Where the bytes of references authored on this device live.
//!
//! This is local state, never replicated: a reference only says what an item
//! is, and this table says where this device reads it from when a peer asks.

use std::{
    ffi::OsString,
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
};

use prost::{Message, Oneof};
use rusqlite::{OptionalExtension, params};
use zeroize::Zeroizing;

use crate::{
    files::{FileIdentity, SourceFile},
    model::{ContentId, Payload, Representation},
};

use super::{EncryptedStorage, Result, StorageError};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocalSource {
    /// One entry per reference entry, in the same order.
    Files(Vec<SourceFile>),
    /// Non-file content too large to replicate inline.
    Data(Payload),
}

impl EncryptedStorage {
    /// Records, or replaces, where a reference's bytes live on this device.
    ///
    /// # Errors
    ///
    /// Returns a database error.
    pub fn put_local_source(&mut self, content_id: ContentId, source: &LocalSource) -> Result<()> {
        let encoded = Zeroizing::new(SourceMessage::from(source).encode_to_vec());
        self.connection.execute(
            "INSERT INTO local_sources (content_id, source) VALUES (?1, ?2)
             ON CONFLICT(content_id) DO UPDATE SET source = excluded.source",
            params![&content_id.as_bytes()[..], encoded.as_slice()],
        )?;
        Ok(())
    }

    /// Where a reference authored here reads its bytes from, if it was.
    ///
    /// # Errors
    ///
    /// Returns a database or decoding error.
    pub fn local_source(&self, content_id: ContentId) -> Result<Option<LocalSource>> {
        let encoded = self
            .connection
            .query_row(
                "SELECT source FROM local_sources WHERE content_id = ?1",
                [&content_id.as_bytes()[..]],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .optional()?;
        encoded
            .map(|encoded| {
                let encoded = Zeroizing::new(encoded);
                SourceMessage::decode(encoded.as_slice())
                    .map_err(|_| StorageError::CorruptOperation("malformed local source".into()))
                    .and_then(|message| decode_source(content_id, message))
            })
            .transpose()
    }
}

fn decode_source(content_id: ContentId, message: SourceMessage) -> Result<LocalSource> {
    let corrupt = |reason: &str| StorageError::CorruptOperation(format!("local source: {reason}"));
    match message.kind.ok_or_else(|| corrupt("missing kind"))? {
        SourceKind::Files(files) => files
            .files
            .into_iter()
            .map(|file| {
                let identity = file
                    .identity
                    .map(|identity| {
                        let modified_nanos = i128::from_be_bytes(
                            identity
                                .modified_nanos
                                .as_slice()
                                .try_into()
                                .map_err(|_| corrupt("modification time"))?,
                        );
                        Ok::<_, StorageError>(FileIdentity {
                            device: identity.device,
                            inode: identity.inode,
                            size: identity.size,
                            modified_nanos,
                        })
                    })
                    .transpose()?;
                Ok(SourceFile {
                    path: PathBuf::from(OsString::from_vec(file.path)),
                    identity,
                })
            })
            .collect::<Result<Vec<_>>>()
            .map(LocalSource::Files),
        SourceKind::Data(data) => Payload::from_parts(
            content_id,
            data.representations
                .into_iter()
                .map(|representation| {
                    Representation::new(representation.mime, representation.bytes)
                })
                .collect(),
        )
        .map(LocalSource::Data)
        .map_err(|_| corrupt("payload")),
    }
}

#[derive(Clone, PartialEq, Message)]
struct SourceMessage {
    #[prost(oneof = "SourceKind", tags = "1, 2")]
    kind: Option<SourceKind>,
}

#[derive(Clone, PartialEq, Oneof)]
enum SourceKind {
    #[prost(message, tag = "1")]
    Files(FilesMessage),
    #[prost(message, tag = "2")]
    Data(DataMessage),
}

#[derive(Clone, PartialEq, Message)]
struct FilesMessage {
    #[prost(message, repeated, tag = "1")]
    files: Vec<SourceFileMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct SourceFileMessage {
    /// Raw path bytes, so non-UTF-8 parent directories survive.
    #[prost(bytes = "vec", tag = "1")]
    path: Vec<u8>,
    #[prost(message, optional, tag = "2")]
    identity: Option<IdentityMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct IdentityMessage {
    #[prost(uint64, tag = "1")]
    device: u64,
    #[prost(uint64, tag = "2")]
    inode: u64,
    #[prost(uint64, tag = "3")]
    size: u64,
    /// Big-endian `i128` nanoseconds since the Unix epoch.
    #[prost(bytes = "vec", tag = "4")]
    modified_nanos: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct DataMessage {
    #[prost(message, repeated, tag = "1")]
    representations: Vec<RepresentationMessage>,
}

#[derive(Clone, PartialEq, Message)]
struct RepresentationMessage {
    #[prost(string, tag = "1")]
    mime: String,
    #[prost(bytes = "vec", tag = "2")]
    bytes: Vec<u8>,
}

impl From<&LocalSource> for SourceMessage {
    fn from(source: &LocalSource) -> Self {
        let kind = match source {
            LocalSource::Files(files) => SourceKind::Files(FilesMessage {
                files: files
                    .iter()
                    .map(|file| SourceFileMessage {
                        path: file.path.as_os_str().as_bytes().to_vec(),
                        identity: file.identity.map(|identity| IdentityMessage {
                            device: identity.device,
                            inode: identity.inode,
                            size: identity.size,
                            modified_nanos: identity.modified_nanos.to_be_bytes().to_vec(),
                        }),
                    })
                    .collect(),
            }),
            LocalSource::Data(payload) => SourceKind::Data(DataMessage {
                representations: payload
                    .representations()
                    .iter()
                    .map(|representation| RepresentationMessage {
                        mime: representation.mime().to_owned(),
                        bytes: representation.bytes().to_vec(),
                    })
                    .collect(),
            }),
        };
        Self { kind: Some(kind) }
    }
}
