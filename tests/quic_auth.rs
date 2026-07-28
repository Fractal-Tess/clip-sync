use std::{
    error::Error,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use clip_sync::transport::{
    AuthError, AuthenticatedConnection, HELLO_FRAME_LEN, Hello, PSK_LEN, Psk, Role,
    authenticate_client, authenticate_server,
};
use quinn::{ClientConfig, Endpoint, ServerConfig};
use rustls::{
    DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime},
};

#[derive(Debug)]
struct SelfSignedVerifier(Arc<CryptoProvider>);

impl SelfSignedVerifier {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

// This deliberately skips certificate-chain authentication because the application PSK is the
// peer identity in this spike. Unlike a blanket signature bypass, it retains rustls' verification
// of the TLS CertificateVerify signature, following Quinn's insecure_connection example.
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

fn endpoint_pair() -> Result<(Endpoint, Endpoint), Box<dyn Error>> {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let certificate = CertificateDer::from(certified.cert);
    let private_key = PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der());
    let server_config = ServerConfig::with_single_cert(vec![certificate], private_key.into())?;
    let server = Endpoint::server(
        server_config,
        SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
    )?;

    let rustls_config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(SelfSignedVerifier::new())
        .with_no_client_auth();
    let quic_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(rustls_config)?;
    let mut client = Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))?;
    client.set_default_client_config(ClientConfig::new(Arc::new(quic_crypto)));
    Ok((client, server))
}

async fn connect_pair(
    client: &Endpoint,
    server: &Endpoint,
) -> Result<(quinn::Connection, quinn::Connection), Box<dyn Error>> {
    let client_connecting = client.connect(server.local_addr()?, "localhost")?;
    let server_incoming = server.accept().await.ok_or("server endpoint closed")?;
    let (client_result, server_result) = tokio::join!(client_connecting, server_incoming);
    Ok((client_result?, server_result?))
}

#[test]
fn hello_framing_is_fixed_bounded_versioned_and_role_tagged() {
    let nonce = [0x5a; clip_sync::transport::NONCE_LEN];
    let hello = Hello::from_nonce(Role::Client, nonce);
    let encoded = hello.encode();

    assert_eq!(encoded.len(), HELLO_FRAME_LEN);
    assert_eq!(hello.role(), Role::Client);
    assert_eq!(hello.nonce(), nonce);
    assert_eq!(Hello::decode(&encoded).unwrap(), hello);
    assert!(matches!(
        Hello::decode(&encoded[..encoded.len() - 1]),
        Err(AuthError::MalformedFrame)
    ));

    let mut bad_version = encoded;
    bad_version[4] ^= 1;
    assert!(matches!(
        Hello::decode(&bad_version),
        Err(AuthError::MalformedFrame)
    ));

    let mut bad_role = encoded;
    bad_role[5] = 0xff;
    assert!(matches!(
        Hello::decode(&bad_role),
        Err(AuthError::MalformedFrame)
    ));

    let server = Hello::from_nonce(Role::Server, nonce);
    assert_ne!(hello.encode(), server.encode());
}

#[test]
fn psk_is_fixed_size_and_redacted() {
    assert!(matches!(
        Psk::new(&[0; PSK_LEN - 1]),
        Err(AuthError::InvalidPskLength)
    ));
    assert!(matches!(
        Psk::new(&[0; PSK_LEN + 1]),
        Err(AuthError::InvalidPskLength)
    ));

    let psk = Psk::new(&[0xa7; PSK_LEN]).unwrap();
    assert_eq!(format!("{psk:?}"), "Psk([REDACTED])");
    assert_eq!(format!("{:?}", Psk::generate().unwrap()), "Psk([REDACTED])");
    assert_eq!(Hello::generate(Role::Server).unwrap().role(), Role::Server);
}

#[tokio::test]
async fn loopback_quinn_authenticates_before_metadata() -> Result<(), Box<dyn Error>> {
    let (client_endpoint, server_endpoint) = endpoint_pair()?;
    let (client_connection, server_connection) =
        connect_pair(&client_endpoint, &server_endpoint).await?;
    let client_psk = Psk::new(&[0x42; PSK_LEN])?;
    let server_psk = Psk::new(&[0x42; PSK_LEN])?;

    // Before this join there is intentionally no API value through which either task can process
    // application metadata: both raw connections have been moved into the authentication gate.
    let (client_auth, server_auth) = tokio::join!(
        authenticate_client(client_connection, &client_psk),
        authenticate_server(server_connection, &server_psk)
    );
    let client_auth: AuthenticatedConnection = client_auth?;
    let server_auth: AuthenticatedConnection = server_auth?;

    // Application traffic starts only after both peers hold AuthenticatedConnection values.
    let (mut send, _) = client_auth.connection().open_bi().await?;
    send.write_all(b"post-auth metadata").await?;
    send.finish()?;

    let (_, mut recv) = server_auth.connection().accept_bi().await?;
    let metadata = recv.read_to_end(64).await?;
    assert_eq!(metadata, b"post-auth metadata");

    client_auth.into_inner().close(0_u32.into(), b"done");
    client_endpoint.wait_idle().await;
    server_endpoint.wait_idle().await;
    Ok(())
}

#[tokio::test]
async fn loopback_rejects_a_different_psk() -> Result<(), Box<dyn Error>> {
    let (client_endpoint, server_endpoint) = endpoint_pair()?;
    let (client_connection, server_connection) =
        connect_pair(&client_endpoint, &server_endpoint).await?;
    let client_psk = Psk::new(&[0x11; PSK_LEN])?;
    let server_psk = Psk::new(&[0x22; PSK_LEN])?;

    let (client_auth, server_auth) = tokio::join!(
        authenticate_client(client_connection, &client_psk),
        authenticate_server(server_connection, &server_psk)
    );

    assert!(client_auth.is_err());
    assert!(matches!(server_auth, Err(AuthError::InvalidProof)));
    Ok(())
}
