mod dispatch;
mod responses;
mod security;
mod server;
mod state;

use clip_sync_ipc::{IpcError, protocol};

pub(crate) use security::DaemonInstance;
pub(crate) use server::serve;
pub(crate) use state::{DaemonCommand, DaemonState};

#[cfg(test)]
mod tests;
