mod client;
mod dispatch;
mod error;
pub mod protocol;
mod responses;
mod security;
mod server;
mod state;

pub use client::request;
pub use error::IpcError;
pub use security::DaemonInstance;
pub use server::serve;
pub use state::{DaemonCommand, DaemonState};

#[cfg(test)]
mod tests;
