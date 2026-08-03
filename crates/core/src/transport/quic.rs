//! Hardened QUIC endpoint configuration for the authenticated mesh.

use std::{net::SocketAddr, sync::Arc, time::Duration};

use quinn::{ClientConfig, Endpoint, EndpointConfig, ServerConfig, TransportConfig};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};
use thiserror::Error;

const ALPN: &[u8] = b"clip-sync-mesh/1";
const MAX_BIDI_STREAMS: u32 = 16;
const IDLE_TIMEOUT: Duration = Duration::from_secs(45);

/// Creates a server-capable endpoint with a matching client configuration.
///
/// The ephemeral certificate provides TLS encryption and signature integrity,
/// while peer trust is established by the exporter-bound application PSK gate.
///
/// # Errors
///
/// Returns an error when certificate generation, TLS setup, or socket binding
/// fails.
pub fn mesh_endpoint(bind_address: SocketAddr) -> Result<Endpoint, QuicConfigError> {
    let certified = rcgen::generate_simple_self_signed(vec!["clip-sync.mesh".to_owned()])?;
    let certificate = CertificateDer::from(certified.cert);
    let private_key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());

    let mut tls_server = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], private_key.into())?;
    tls_server.alpn_protocols = vec![ALPN.to_vec()];
    let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls_server)?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(server_crypto));
    server_config.transport_config(transport_config()?);

    let mut tls_client = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(SelfSignedVerifier::new())
        .with_no_client_auth();
    tls_client.alpn_protocols = vec![ALPN.to_vec()];
    let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls_client)?;
    let mut client_config = ClientConfig::new(Arc::new(client_crypto));
    client_config.transport_config(transport_config()?);

    let socket = std::net::UdpSocket::bind(bind_address)?;
    socket.set_nonblocking(true)?;
    let runtime = quinn::default_runtime().ok_or(QuicConfigError::MissingRuntime)?;
    let mut endpoint = Endpoint::new(
        EndpointConfig::default(),
        Some(server_config),
        socket,
        runtime,
    )?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}

fn transport_config() -> Result<Arc<TransportConfig>, QuicConfigError> {
    let mut config = TransportConfig::default();
    config.max_concurrent_bidi_streams(MAX_BIDI_STREAMS.into());
    config.max_concurrent_uni_streams(0_u32.into());
    config.keep_alive_interval(Some(Duration::from_secs(10)));
    config.max_idle_timeout(Some(IDLE_TIMEOUT.try_into()?));
    Ok(Arc::new(config))
}

/// Certificate verifier used only before the exporter-bound PSK exchange.
///
/// It accepts the peer's ephemeral self-signed chain but retains verification
/// of TLS `CertificateVerify` signatures, preventing an invalid TLS transcript
/// from reaching application authentication.
#[derive(Debug)]
struct SelfSignedVerifier(Arc<CryptoProvider>);

impl SelfSignedVerifier {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

impl ServerCertVerifier for SelfSignedVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug, Error)]
pub enum QuicConfigError {
    #[error("could not generate an ephemeral TLS certificate: {0}")]
    Certificate(#[from] rcgen::Error),
    #[error("could not configure TLS: {0}")]
    Tls(#[from] rustls::Error),
    #[error("could not configure QUIC/TLS: {0}")]
    QuicTls(#[from] quinn::crypto::rustls::NoInitialCipherSuite),
    #[error("invalid QUIC idle timeout: {0}")]
    IdleTimeout(#[from] quinn::VarIntBoundsExceeded),
    #[error("no async QUIC runtime is installed")]
    MissingRuntime,
    #[error("could not bind the QUIC endpoint: {0}")]
    Io(#[from] std::io::Error),
}
