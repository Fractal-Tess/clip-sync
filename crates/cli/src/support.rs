use clip_sync_core::config::AppPaths;
use clip_sync_ipc::{
    self as ipc,
    protocol::{IPC_PROTOCOL_VERSION, Request, request, response},
};

use super::views::{error_json, print_json};

pub(super) async fn daemon_request(
    paths: &AppPaths,
    request_id: u64,
    body: request::Body,
    json: bool,
) -> anyhow::Result<clip_sync_ipc::protocol::Response> {
    let result = ipc::request(
        &paths.socket,
        Request {
            protocol_version: IPC_PROTOCOL_VERSION,
            request_id,
            body: Some(body),
        },
    )
    .await;
    match result {
        Ok(response) => Ok(response),
        Err(error) => {
            let message = format!(
                "clip-sync daemon is unavailable at {}; start clip-sync.service or run `clip-sync daemon`",
                paths.socket.display()
            );
            if json {
                print_json(&error_json("daemon_unavailable", &message))?;
            }
            Err(anyhow::Error::new(error).context(message))
        }
    }
}

pub(super) fn mutation_response(
    response: clip_sync_ipc::protocol::Response,
    json: bool,
    fallback_message: &str,
) -> anyhow::Result<()> {
    match response.body {
        Some(response::Body::Mutation(result)) if result.ok => {
            if json {
                print_json(&serde_json::json!({
                    "ok": true,
                    "message": result.message,
                    "resource_id": result.resource_id,
                }))?;
            } else if result.message.is_empty() {
                println!("{fallback_message}");
            } else {
                println!("{}", result.message);
            }
            Ok(())
        }
        Some(response::Body::Mutation(_)) => Err(operation_error(
            json,
            "mutation_failed",
            "daemon reported an unsuccessful mutation",
        )),
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, json)),
        _ => Err(unexpected_response(json, "mutation")),
    }
}

pub(super) fn daemon_response_error(
    error: &clip_sync_ipc::protocol::ErrorResponse,
    json: bool,
) -> anyhow::Error {
    operation_error(json, &error.code, &error.message)
}

pub(super) fn unexpected_response(json: bool, operation: &str) -> anyhow::Error {
    operation_error(
        json,
        "protocol_error",
        &format!("daemon returned an unexpected response to {operation}"),
    )
}

pub(super) fn operation_error(json: bool, code: &str, message: &str) -> anyhow::Error {
    if json {
        let value = error_json(code, message);
        if let Err(serialization_error) = print_json(&value) {
            return serialization_error;
        }
    }
    anyhow::anyhow!("{code}: {message}")
}
