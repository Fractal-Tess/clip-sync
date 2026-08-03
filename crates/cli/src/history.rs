use clip_sync_core::config::AppPaths;
use clip_sync_ipc::protocol::{
    ActivateRequest, HistoryRequest, HistoryUpdateAction, HistoryUpdateRequest, request, response,
};

use super::{
    commands::{HistoryCommand, MutationArgs},
    support::{daemon_request, daemon_response_error, mutation_response, unexpected_response},
    views::{history_item_json, print_json},
};

pub(super) async fn history_command(
    paths: &AppPaths,
    command: HistoryCommand,
) -> anyhow::Result<()> {
    match command {
        HistoryCommand::List { query, limit, json } => {
            history_query(paths, query.unwrap_or_default(), limit, json).await
        }
        HistoryCommand::Search { query, limit, json } => {
            history_query(paths, query, limit, json).await
        }
        HistoryCommand::Activate(args) => {
            let response = daemon_request(
                paths,
                4,
                request::Body::Activate(ActivateRequest {
                    content_id: args.content_id,
                }),
                args.json,
            )
            .await?;
            mutation_response(response, args.json, "clipboard activated")
        }
        HistoryCommand::Pin(args) => history_update(paths, args, HistoryUpdateAction::Pin).await,
        HistoryCommand::Unpin(args) => {
            history_update(paths, args, HistoryUpdateAction::Unpin).await
        }
        HistoryCommand::Delete(args) => {
            history_update(paths, args, HistoryUpdateAction::Delete).await
        }
    }
}

async fn history_query(
    paths: &AppPaths,
    query: String,
    limit: u32,
    json: bool,
) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        3,
        request::Body::History(HistoryRequest {
            query,
            limit,
            offset: 0,
        }),
        json,
    )
    .await?;
    match response.body {
        Some(response::Body::History(history)) if json => {
            let items = history
                .items
                .into_iter()
                .map(|item| history_item_json(&item))
                .collect::<Vec<_>>();
            print_json(&items)
        }
        Some(response::Body::History(history)) => {
            for item in history.items {
                let short_id = item.content_id.chars().take(12).collect::<String>();
                let pin = if item.pinned { "pin" } else { "   " };
                println!(
                    "{short_id}  {pin}  {:>8} B  {}",
                    item.logical_size, item.preview
                );
            }
            Ok(())
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, json)),
        _ => Err(unexpected_response(json, "history query")),
    }
}

async fn history_update(
    paths: &AppPaths,
    args: MutationArgs,
    action: HistoryUpdateAction,
) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        5,
        request::Body::HistoryUpdate(HistoryUpdateRequest {
            content_id: args.content_id,
            action: action as i32,
        }),
        args.json,
    )
    .await?;
    mutation_response(response, args.json, "history updated")
}
