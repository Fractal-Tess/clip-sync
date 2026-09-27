use anyhow::Context;

use clip_sync_core::{model::NodeId, storage::HistoryStore};

use crate::{
    ipc::{DaemonCommand, DaemonState},
    mesh::MeshHandle,
};

use super::{
    activation::Activation,
    preview::image_preview,
    runtime::{CLIPBOARD_DISABLED_DETAIL, unix_time_millis},
    views::{device_items, history_items},
};

pub(super) async fn handle_daemon_command(
    command: DaemonCommand,
    activation: &mut Activation<'_>,
    clipboard_enabled: bool,
) {
    let history = &mut *activation.history;
    let state = activation.state;
    let mesh = activation.mesh;
    match command {
        DaemonCommand::Activate { content_id, reply } if !clipboard_enabled => {
            let _ = reply.send(Err(format!(
                "cannot activate {content_id}: clipboard {CLIPBOARD_DISABLED_DETAIL}"
            )));
        }
        DaemonCommand::Activate { content_id, reply } => {
            let result = activation
                .activate(&content_id)
                .await
                .map_err(|error| format!("{error:#}"));
            let _ = reply.send(result);
        }
        DaemonCommand::SetPinned {
            content_id,
            pinned,
            reply,
        } => {
            let result = update_history_item(
                &content_id,
                HistoryMutation::SetPinned(pinned),
                history,
                state,
                mesh,
            )
            .await
            .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        DaemonCommand::Delete { content_id, reply } => {
            let result =
                update_history_item(&content_id, HistoryMutation::Delete, history, state, mesh)
                    .await
                    .map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
        DaemonCommand::ForgetDevice { device_id, reply } => {
            let result = forget_device(&device_id, history, mesh)
                .await
                .map_err(|error| error.to_string());
            if result.is_ok() {
                state.set_devices(device_items(history)).await;
            }
            let _ = reply.send(result);
        }
        DaemonCommand::ImagePreview { content_id, reply } => {
            let result = image_preview(&content_id, history).map_err(|error| error.to_string());
            let _ = reply.send(result);
        }
    }
}

pub(super) async fn forget_device(
    encoded_node_id: &str,
    history: &mut HistoryStore,
    mesh: &MeshHandle,
) -> anyhow::Result<()> {
    let node_id: NodeId = encoded_node_id.parse().context("device ID is invalid")?;
    let acknowledgements = history
        .acknowledgements()
        .context("load durable mesh membership")?;
    let known = history
        .projection()
        .known_members()
        .chain(acknowledgements.known_members())
        .any(|member| member == node_id);
    anyhow::ensure!(known, "device is not a known mesh member");
    anyhow::ensure!(
        !history.projection().is_device_forgotten(node_id),
        "device is already forgotten"
    );
    let operation = history
        .forget_device(node_id, unix_time_millis()?)
        .context("persist device-forget operation")?;
    mesh.record_local(&operation)
        .await
        .context("publish device-forget operation")?;
    Ok(())
}

#[derive(Clone, Copy)]
enum HistoryMutation {
    SetPinned(bool),
    Delete,
}

async fn update_history_item(
    encoded_content_id: &str,
    mutation: HistoryMutation,
    history: &mut HistoryStore,
    state: &DaemonState,
    mesh: &MeshHandle,
) -> anyhow::Result<()> {
    let now_millis = unix_time_millis()?;
    let operation = match mutation {
        HistoryMutation::SetPinned(true) => history
            .pin_by_id(encoded_content_id, now_millis)
            .context("persist clipboard history pin")?,
        HistoryMutation::SetPinned(false) => history
            .unpin_by_id(encoded_content_id, now_millis)
            .context("persist clipboard history unpin")?,
        HistoryMutation::Delete => history
            .delete_by_id(encoded_content_id, now_millis)
            .context("persist clipboard history deletion")?,
    };
    mesh.record_local(&operation)
        .await
        .context("publish clipboard history update to mesh")?;
    state.set_history(history_items(history.replica())).await;
    Ok(())
}
