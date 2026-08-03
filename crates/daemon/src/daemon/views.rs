use clip_sync_core::{model::Payload, replica::Replica, storage::HistoryStore};
use clip_sync_ipc::protocol::{DeviceItem, HistoryItem};

pub(super) fn history_items(replica: &Replica) -> Vec<HistoryItem> {
    replica
        .projection()
        .visible_items()
        .into_iter()
        .map(|view| {
            let payload = view.payload();
            let mime_types = payload.map_or_else(Vec::new, |payload| {
                payload
                    .representations()
                    .iter()
                    .map(|representation| representation.mime().to_owned())
                    .collect()
            });
            let transfer = replica.projection().manifest_for_content(view.content_id());
            let mime_types = if mime_types.is_empty() {
                transfer.map_or_else(Vec::new, |(_, _, manifest)| match manifest {
                    clip_sync_core::payload::StoredManifest::MimeBundle(bundle) => bundle
                        .representations()
                        .iter()
                        .map(|representation| representation.mime().to_owned())
                        .collect(),
                    clip_sync_core::payload::StoredManifest::Files(_) => {
                        vec!["text/uri-list".to_owned()]
                    }
                    clip_sync_core::payload::StoredManifest::Blob(_) => {
                        vec!["application/octet-stream".to_owned()]
                    }
                })
            } else {
                mime_types
            };
            let logical_size = payload.map_or_else(
                || transfer.map_or(0, |(_, _, manifest)| manifest.logical_size()),
                |payload| payload.descriptor().logical_size(),
            );
            let origin = replica
                .projection()
                .origin_event_for_content(view.content_id())
                .unwrap_or_else(|| view.last_activity());
            HistoryItem {
                content_id: view.content_id().to_string(),
                preview: payload.map_or_else(
                    || {
                        transfer.map_or_else(
                            || "Unavailable payload".to_owned(),
                            |(transfer_id, _, _)| {
                                let phase = replica.projection().transfer(transfer_id).map_or(
                                    clip_sync_core::transfer::TransferPhase::Pending,
                                    clip_sync_core::model::TransferView::phase,
                                );
                                format!("Transferred payload · {phase:?}")
                            },
                        )
                    },
                    history_preview,
                ),
                mime_types,
                logical_size,
                source_node: origin.operation_id().node().to_string(),
                pinned: view.pinned(),
                physical_millis: view.last_activity().timestamp().physical_millis(),
                source_device: String::new(),
                origin_millis: Some(origin.timestamp().physical_millis()),
            }
        })
        .collect()
}

pub(super) fn device_items(history: &HistoryStore) -> Vec<DeviceItem> {
    let local = history.replica().node_id();
    let mut members = history
        .projection()
        .known_members()
        .chain(history.projection().forgotten_devices())
        .collect::<std::collections::BTreeSet<_>>();
    if let Ok(acknowledgements) = history.acknowledgements() {
        members.extend(acknowledgements.known_members());
    }
    members.insert(local);
    members
        .into_iter()
        .map(|node_id| DeviceItem {
            device_id: node_id.to_string(),
            local: node_id == local,
            forgotten: history.projection().is_device_forgotten(node_id),
        })
        .collect()
}

fn history_preview(payload: &Payload) -> String {
    if let Some(text) = payload
        .representations()
        .iter()
        .find(|representation| representation.mime().starts_with("text/plain"))
    {
        let decoded = String::from_utf8_lossy(text.bytes());
        let mut preview = decoded
            .chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(160)
            .collect::<String>();
        if decoded.chars().count() > 160 {
            preview.push('…');
        }
        return preview;
    }

    let mime = payload
        .representations()
        .first()
        .map_or("unknown", |representation| representation.mime());
    format!("{mime} · {} bytes", payload.descriptor().logical_size())
}
