use crate::{
    config::AppPaths,
    ipc::protocol::{TransferCancelRequest, TransfersRequest, request, response},
};

use super::{
    commands::TransferCommand,
    support::{daemon_request, daemon_response_error, mutation_response, unexpected_response},
    views::{print_json, transfer_json},
};

pub(super) async fn transfer_command(
    paths: &AppPaths,
    command: TransferCommand,
) -> anyhow::Result<()> {
    match command {
        TransferCommand::List(output) => {
            let response = daemon_request(
                paths,
                9,
                request::Body::Transfers(TransfersRequest {}),
                output.json,
            )
            .await?;
            match response.body {
                Some(response::Body::Transfers(transfers)) if output.json => {
                    let transfers = transfers
                        .transfers
                        .into_iter()
                        .map(|transfer| transfer_json(&transfer))
                        .collect::<Vec<_>>();
                    print_json(&transfers)
                }
                Some(response::Body::Transfers(transfers)) => {
                    if transfers.transfers.is_empty() {
                        println!("no transfers");
                    }
                    for transfer in transfers.transfers {
                        let percent = transfer
                            .completed_bytes
                            .saturating_mul(100)
                            .checked_div(transfer.total_bytes)
                            .unwrap_or(0);
                        println!(
                            "{}  {}  {}/{} B ({percent}%)  {}  {}",
                            transfer.transfer_id,
                            transfer.state,
                            transfer.completed_bytes,
                            transfer.total_bytes,
                            transfer.peer,
                            transfer.content_id,
                        );
                    }
                    Ok(())
                }
                Some(response::Body::Error(error)) => {
                    Err(daemon_response_error(&error, output.json))
                }
                _ => Err(unexpected_response(output.json, "transfers")),
            }
        }
        TransferCommand::Cancel { transfer_id, json } => {
            let response = daemon_request(
                paths,
                10,
                request::Body::TransferCancel(TransferCancelRequest { transfer_id }),
                json,
            )
            .await?;
            mutation_response(response, json, "transfer cancelled")
        }
    }
}
