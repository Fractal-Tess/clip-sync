use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use clip_sync_core::config::AppPaths;
use clip_sync_ipc::{
    self as ipc,
    protocol::{Request, request, response},
};

pub(crate) struct AppState {
    paths: Result<AppPaths, String>,
    request_id: AtomicU64,
}

impl AppState {
    pub(crate) fn discover(config_override: Option<PathBuf>) -> Self {
        Self {
            paths: AppPaths::discover(config_override).map_err(|error| error.to_string()),
            request_id: AtomicU64::new(0),
        }
    }

    fn next_request_id(&self) -> u64 {
        self.request_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

pub(crate) async fn daemon_request(
    state: &AppState,
    body: request::Body,
) -> Result<response::Body, String> {
    let paths = state.paths.as_ref().map_err(Clone::clone)?;
    let response = ipc::request(
        &paths.socket,
        Request {
            protocol_version: clip_sync_ipc::protocol::IPC_PROTOCOL_VERSION,
            request_id: state.next_request_id(),
            body: Some(body),
        },
    )
    .await
    .map_err(|error| {
        format!(
            "ClipSync daemon is unavailable at {}: {error}",
            paths.socket.display()
        )
    })?;

    match response.body {
        Some(response::Body::Error(error)) => Err(format!("{}: {}", error.code, error.message)),
        Some(body) => Ok(body),
        None => Err("ClipSync daemon returned an empty response".to_owned()),
    }
}
