//! QUIC transport building blocks.
//!
//! Application data must only be handled through an [`AuthenticatedConnection`].

pub mod auth;

pub use auth::{
    AuthError, AuthenticatedConnection, HELLO_FRAME_LEN, Hello, NONCE_LEN, PSK_LEN, Psk, Role,
    authenticate_client, authenticate_server,
};
