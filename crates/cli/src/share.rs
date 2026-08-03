use anyhow::bail;

use clip_sync_core::config::AppPaths;
use clip_sync_ipc::protocol::{ShareClipboardRequest, request, response};

use super::{
    commands::ShareArgs,
    support::{daemon_request, daemon_response_error, unexpected_response},
    views::{print_json, share_json},
};

pub(super) async fn share_clipboard(paths: &AppPaths, args: ShareArgs) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        8,
        request::Body::ShareClipboard(ShareClipboardRequest {
            confirmed: args.confirm,
        }),
        args.json,
    )
    .await?;
    match response.body {
        Some(response::Body::ShareClipboard(result)) => {
            if args.json {
                print_json(&share_json(&result))?;
            } else if result.shared {
                println!("{}", result.message);
                println!(
                    "transfer: {}",
                    result.transfer_id.as_deref().unwrap_or("unavailable")
                );
                println!(
                    "content: {}",
                    result.content_id.as_deref().unwrap_or("unavailable")
                );
            } else {
                println!(
                    "{} ({} bytes; MIME: {})",
                    result.message,
                    result.logical_size,
                    result.mime_types.join(", ")
                );
            }
            if result.shared {
                Ok(())
            } else {
                bail!("{}", result.message)
            }
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, args.json)),
        _ => Err(unexpected_response(args.json, "clipboard share")),
    }
}
