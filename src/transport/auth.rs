//! TLS-exporter-bound application PSK authentication for QUIC connections.
//!
//! Authentication occupies the first bidirectional stream opened by the client. The protocol
//! exchanges only fixed-size role-tagged hello and proof frames. Callers receive the connection
//! back only after the peer's proof has been verified.

use std::{fmt, time::Duration};

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use quinn::{Connection, RecvStream, SendStream};
use secrecy::{ExposeSecret, SecretBox};
use sha2::Sha256;
use thiserror::Error;
use tokio::time::timeout;
use zeroize::Zeroizing;

/// Size of a PSK accepted by this protocol.
pub const PSK_LEN: usize = 32;
/// Size of each fresh hello nonce.
pub const NONCE_LEN: usize = 32;
/// Encoded size of a hello frame.
pub const HELLO_FRAME_LEN: usize = 4 + 1 + 1 + NONCE_LEN;

const PROOF_LEN: usize = 32;
const PROOF_FRAME_LEN: usize = 4 + 1 + 1 + PROOF_LEN;
const VERSION: u8 = 1;
const HELLO_MAGIC: &[u8; 4] = b"CSH0";
const PROOF_MAGIC: &[u8; 4] = b"CSP0";
const TRANSCRIPT_DOMAIN: &[u8] = b"clip-sync/quic-auth/transcript/v1";
const EXPORTER_LABEL: &[u8] = b"EXPORTER-clip-sync-quic-auth-v1";
const HKDF_SALT_DOMAIN: &[u8] = b"clip-sync/quic-auth/hkdf-salt/v1";
const CLIENT_KEY_INFO: &[u8] = b"clip-sync/quic-auth/client-proof/v1";
const SERVER_KEY_INFO: &[u8] = b"clip-sync/quic-auth/server-proof/v1";
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const AUTH_FAILURE_CODE: u32 = 0x100;
const AUTH_FAILURE_REASON: &[u8] = b"authentication failed";

type HmacSha256 = Hmac<Sha256>;

/// A connection endpoint's role in the authentication transcript.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum Role {
    /// The QUIC client, which opens the authentication stream.
    Client = 1,
    /// The QUIC server, which accepts the authentication stream.
    Server = 2,
}

impl Role {
    const fn peer(self) -> Self {
        match self {
            Self::Client => Self::Server,
            Self::Server => Self::Client,
        }
    }

    const fn key_info(self) -> &'static [u8] {
        match self {
            Self::Client => CLIENT_KEY_INFO,
            Self::Server => SERVER_KEY_INFO,
        }
    }
}

impl TryFrom<u8> for Role {
    type Error = AuthError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Client),
            2 => Ok(Self::Server),
            _ => Err(AuthError::MalformedFrame),
        }
    }
}

/// A 256-bit application pre-shared key which is redacted and zeroized on drop.
pub struct Psk(SecretBox<[u8; PSK_LEN]>);

impl Psk {
    /// Copies a PSK into protected storage.
    ///
    /// # Errors
    /// Returns [`AuthError::InvalidPskLength`] unless `bytes` is exactly [`PSK_LEN`] bytes.
    pub fn new(bytes: &[u8]) -> Result<Self, AuthError> {
        if bytes.len() != PSK_LEN {
            return Err(AuthError::InvalidPskLength);
        }
        Ok(Self(SecretBox::<[u8; PSK_LEN]>::init_with_mut(
            |key: &mut [u8; PSK_LEN]| {
                key.copy_from_slice(bytes);
            },
        )))
    }

    /// Generates a new PSK from the operating system CSPRNG.
    ///
    /// # Errors
    /// Returns [`AuthError::Entropy`] if the operating system RNG fails.
    pub fn generate() -> Result<Self, AuthError> {
        let mut key = Zeroizing::new([0; PSK_LEN]);
        getrandom::fill(key.as_mut()).map_err(|_| AuthError::Entropy)?;
        Self::new(key.as_ref())
    }

    fn expose(&self) -> &[u8; PSK_LEN] {
        self.0.expose_secret()
    }
}

impl fmt::Debug for Psk {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Psk([REDACTED])")
    }
}

/// A fixed-size, versioned, role-tagged hello frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Hello {
    role: Role,
    nonce: [u8; NONCE_LEN],
}

impl Hello {
    /// Constructs a hello around a caller-supplied nonce.
    #[must_use]
    pub const fn from_nonce(role: Role, nonce: [u8; NONCE_LEN]) -> Self {
        Self { role, nonce }
    }

    /// Generates a hello with a fresh nonce.
    ///
    /// # Errors
    /// Returns [`AuthError::Entropy`] if the operating system RNG fails.
    pub fn generate(role: Role) -> Result<Self, AuthError> {
        let mut nonce = [0; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|_| AuthError::Entropy)?;
        Ok(Self { role, nonce })
    }

    /// Returns the role encoded in this hello.
    #[must_use]
    pub const fn role(self) -> Role {
        self.role
    }

    /// Returns the nonce encoded in this hello.
    #[must_use]
    pub const fn nonce(self) -> [u8; NONCE_LEN] {
        self.nonce
    }

    /// Encodes this hello into its bounded wire representation.
    #[must_use]
    pub fn encode(self) -> [u8; HELLO_FRAME_LEN] {
        let mut frame = [0; HELLO_FRAME_LEN];
        frame[..4].copy_from_slice(HELLO_MAGIC);
        frame[4] = VERSION;
        frame[5] = self.role as u8;
        frame[6..].copy_from_slice(&self.nonce);
        frame
    }

    /// Parses exactly one bounded hello frame.
    ///
    /// # Errors
    /// Returns [`AuthError::MalformedFrame`] for an incorrect length, magic, version, or role.
    pub fn decode(frame: &[u8]) -> Result<Self, AuthError> {
        if frame.len() != HELLO_FRAME_LEN || &frame[..4] != HELLO_MAGIC || frame[4] != VERSION {
            return Err(AuthError::MalformedFrame);
        }

        let role = Role::try_from(frame[5])?;
        let nonce = frame[6..]
            .try_into()
            .map_err(|_| AuthError::MalformedFrame)?;
        Ok(Self { role, nonce })
    }
}

/// Authentication failure. Protocol failures deliberately do not disclose details to the peer.
#[derive(Debug, Error)]
pub enum AuthError {
    /// The PSK was not exactly 256 bits.
    #[error("the QUIC authentication PSK must be exactly {PSK_LEN} bytes")]
    InvalidPskLength,
    /// Secure random generation failed.
    #[error("secure random generation failed")]
    Entropy,
    /// A frame had an invalid length, magic, version, or role tag.
    #[error("malformed authentication frame")]
    MalformedFrame,
    /// A valid frame carried the wrong endpoint role.
    #[error("unexpected authentication role")]
    UnexpectedRole,
    /// The peer did not possess the same PSK or transcript.
    #[error("peer authentication proof is invalid")]
    InvalidProof,
    /// TLS exporter keying material was unavailable.
    #[error("TLS exporter keying material is unavailable")]
    Exporter,
    /// The bounded authentication exchange timed out.
    #[error("authentication timed out")]
    Timeout,
    /// The QUIC connection failed during authentication.
    #[error("QUIC connection failed during authentication: {0}")]
    Connection(#[from] quinn::ConnectionError),
    /// Writing the authentication stream failed.
    #[error("failed to write authentication stream: {0}")]
    Write(#[from] quinn::WriteError),
    /// Reading the authentication stream failed.
    #[error("failed to read authentication stream: {0}")]
    Read(#[from] quinn::ReadExactError),
    /// Finishing the authentication stream failed.
    #[error("failed to finish authentication stream: {0}")]
    Finish(#[from] quinn::ClosedStream),
}

/// A QUIC connection released only after mutual application-PSK authentication.
#[derive(Debug)]
pub struct AuthenticatedConnection(Connection);

impl AuthenticatedConnection {
    /// Borrows the authenticated QUIC connection for application traffic.
    #[must_use]
    pub const fn connection(&self) -> &Connection {
        &self.0
    }

    /// Consumes the authentication gate and returns the QUIC connection.
    #[must_use]
    pub fn into_inner(self) -> Connection {
        self.0
    }
}

/// Mutually authenticates the client side of a QUIC connection.
///
/// No application metadata is sent by this function. The connection is returned only after the
/// server's role-separated proof verifies.
///
/// # Errors
/// Returns an [`AuthError`] on timeout, malformed input, transport failure, or proof failure.
pub async fn authenticate_client(
    connection: Connection,
    psk: &Psk,
) -> Result<AuthenticatedConnection, AuthError> {
    authenticate(connection, psk, Role::Client).await
}

/// Mutually authenticates the server side of a QUIC connection.
///
/// The first client-opened bidirectional stream is reserved for authentication. Callers cannot
/// process application metadata through the returned value until the client's proof verifies.
///
/// # Errors
/// Returns an [`AuthError`] on timeout, malformed input, transport failure, or proof failure.
pub async fn authenticate_server(
    connection: Connection,
    psk: &Psk,
) -> Result<AuthenticatedConnection, AuthError> {
    authenticate(connection, psk, Role::Server).await
}

async fn authenticate(
    connection: Connection,
    psk: &Psk,
    role: Role,
) -> Result<AuthenticatedConnection, AuthError> {
    let result = timeout(AUTH_TIMEOUT, authenticate_inner(&connection, psk, role)).await;
    match result {
        Ok(Ok(())) => Ok(AuthenticatedConnection(connection)),
        Ok(Err(error)) => {
            close_for_auth_failure(&connection);
            Err(error)
        }
        Err(_) => {
            close_for_auth_failure(&connection);
            Err(AuthError::Timeout)
        }
    }
}

async fn authenticate_inner(
    connection: &Connection,
    psk: &Psk,
    role: Role,
) -> Result<(), AuthError> {
    let (mut send, mut recv) = match role {
        Role::Client => connection.open_bi().await?,
        Role::Server => connection.accept_bi().await?,
    };
    let local_hello = Hello::generate(role)?;

    let peer_hello = match role {
        Role::Client => {
            write_hello(&mut send, local_hello).await?;
            read_hello(&mut recv, role.peer()).await?
        }
        Role::Server => {
            let hello = read_hello(&mut recv, role.peer()).await?;
            write_hello(&mut send, local_hello).await?;
            hello
        }
    };

    let (client_hello, server_hello) = match role {
        Role::Client => (local_hello, peer_hello),
        Role::Server => (peer_hello, local_hello),
    };
    let transcript = transcript(client_hello, server_hello);
    let mut exporter = Zeroizing::new([0; 32]);
    connection
        .export_keying_material(exporter.as_mut(), EXPORTER_LABEL, &transcript)
        .map_err(|_| AuthError::Exporter)?;

    let local_proof = make_proof(psk, role, exporter.as_ref(), &transcript)?;
    match role {
        Role::Client => {
            write_proof(&mut send, role, &local_proof).await?;
            let peer_proof = read_proof(&mut recv, role.peer()).await?;
            verify_proof(
                psk,
                role.peer(),
                exporter.as_ref(),
                &transcript,
                peer_proof.as_ref(),
            )?;
        }
        Role::Server => {
            let peer_proof = read_proof(&mut recv, role.peer()).await?;
            verify_proof(
                psk,
                role.peer(),
                exporter.as_ref(),
                &transcript,
                peer_proof.as_ref(),
            )?;
            write_proof(&mut send, role, &local_proof).await?;
        }
    }
    send.finish()?;
    Ok(())
}

fn transcript(client: Hello, server: Hello) -> Vec<u8> {
    let mut transcript = Vec::with_capacity(TRANSCRIPT_DOMAIN.len() + 2 * HELLO_FRAME_LEN);
    transcript.extend_from_slice(TRANSCRIPT_DOMAIN);
    transcript.extend_from_slice(&client.encode());
    transcript.extend_from_slice(&server.encode());
    transcript
}

fn proof_mac(
    psk: &Psk,
    role: Role,
    exporter: &[u8],
    transcript: &[u8],
) -> Result<HmacSha256, AuthError> {
    let mut salt = Zeroizing::new(Vec::with_capacity(HKDF_SALT_DOMAIN.len() + exporter.len()));
    salt.extend_from_slice(HKDF_SALT_DOMAIN);
    salt.extend_from_slice(exporter);

    let hkdf = Hkdf::<Sha256>::new(Some(&salt), psk.expose());
    let mut proof_key = Zeroizing::new([0; PROOF_LEN]);
    hkdf.expand(role.key_info(), proof_key.as_mut())
        .map_err(|_| AuthError::Exporter)?;

    let mut mac =
        HmacSha256::new_from_slice(proof_key.as_ref()).map_err(|_| AuthError::InvalidPskLength)?;
    mac.update(transcript);
    Ok(mac)
}

fn make_proof(
    psk: &Psk,
    role: Role,
    exporter: &[u8],
    transcript: &[u8],
) -> Result<Zeroizing<[u8; PROOF_LEN]>, AuthError> {
    let tag: [u8; PROOF_LEN] = proof_mac(psk, role, exporter, transcript)?
        .finalize()
        .into_bytes()
        .into();
    let tag = Zeroizing::new(tag);
    let mut proof = Zeroizing::new([0; PROOF_LEN]);
    proof.copy_from_slice(tag.as_ref());
    Ok(proof)
}

fn verify_proof(
    psk: &Psk,
    role: Role,
    exporter: &[u8],
    transcript: &[u8],
    proof: &[u8],
) -> Result<(), AuthError> {
    // `verify_slice` compares the complete tag in constant time.
    proof_mac(psk, role, exporter, transcript)?
        .verify_slice(proof)
        .map_err(|_| AuthError::InvalidProof)
}

async fn write_hello(send: &mut SendStream, hello: Hello) -> Result<(), AuthError> {
    send.write_all(&hello.encode()).await?;
    Ok(())
}

async fn read_hello(recv: &mut RecvStream, expected_role: Role) -> Result<Hello, AuthError> {
    let mut frame = [0; HELLO_FRAME_LEN];
    recv.read_exact(&mut frame).await?;
    let hello = Hello::decode(&frame)?;
    if hello.role != expected_role {
        return Err(AuthError::UnexpectedRole);
    }
    Ok(hello)
}

async fn write_proof(
    send: &mut SendStream,
    role: Role,
    proof: &[u8; PROOF_LEN],
) -> Result<(), AuthError> {
    let mut frame = Zeroizing::new([0; PROOF_FRAME_LEN]);
    frame[..4].copy_from_slice(PROOF_MAGIC);
    frame[4] = VERSION;
    frame[5] = role as u8;
    frame[6..].copy_from_slice(proof);
    send.write_all(frame.as_ref()).await?;
    Ok(())
}

async fn read_proof(
    recv: &mut RecvStream,
    expected_role: Role,
) -> Result<Zeroizing<[u8; PROOF_LEN]>, AuthError> {
    let mut frame = Zeroizing::new([0; PROOF_FRAME_LEN]);
    recv.read_exact(frame.as_mut()).await?;
    if &frame[..4] != PROOF_MAGIC || frame[4] != VERSION {
        return Err(AuthError::MalformedFrame);
    }
    let role = Role::try_from(frame[5])?;
    if role != expected_role {
        return Err(AuthError::UnexpectedRole);
    }
    let mut proof = Zeroizing::new([0; PROOF_LEN]);
    proof.copy_from_slice(&frame[6..]);
    Ok(proof)
}

fn close_for_auth_failure(connection: &Connection) {
    connection.close(
        quinn::VarInt::from_u32(AUTH_FAILURE_CODE),
        AUTH_FAILURE_REASON,
    );
}
