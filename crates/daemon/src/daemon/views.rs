use clip_sync_core::{
    model::{ItemKind, PayloadDescriptor, Reference},
    replica::Replica,
    storage::HistoryStore,
};
use clip_sync_ipc::protocol::{DeviceItem, HistoryItem};

pub(super) fn history_items(replica: &Replica) -> Vec<HistoryItem> {
    let local = replica.node_id();
    replica
        .projection()
        .visible_items()
        .into_iter()
        .map(|view| {
            let origin = view.origin().unwrap_or_else(|| view.last_activity());
            let (preview, mime_types, logical_size) = view.item().map_or_else(
                || ("Waiting for this item to sync".to_owned(), Vec::new(), 0),
                |item| (item_preview(item), item.mime_types(), item.logical_size()),
            );
            HistoryItem {
                content_id: view.content_id().to_string(),
                preview,
                mime_types,
                logical_size,
                source_node: origin.operation_id().node().to_string(),
                pinned: view.pinned(),
                physical_millis: view.last_activity().timestamp().physical_millis(),
                source_device: String::new(),
                origin_millis: Some(origin.timestamp().physical_millis()),
                remote: matches!(view.item(), Some(ItemKind::Reference(_)))
                    && origin.operation_id().node() != local,
                pinned_millis: view
                    .pinned_at()
                    .map(|event| event.timestamp().physical_millis()),
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

fn item_preview(item: &ItemKind) -> String {
    match item {
        ItemKind::Inline {
            text_preview: Some(text),
            ..
        } => text.clone(),
        ItemKind::Inline { descriptor, .. } => binary_preview(descriptor),
        ItemKind::Reference(Reference::Files(entries)) => {
            let roots = entries
                .iter()
                .filter(|entry| !entry.path.contains('/'))
                .collect::<Vec<_>>();
            let files = entries.iter().filter(|entry| !entry.directory).count();
            match roots.as_slice() {
                [root] if !root.directory => root.path.clone(),
                [root] => format!("{}/ · {files} files", root.path),
                _ => format!(
                    "{} and {} more · {files} files",
                    roots.first().map_or("", |root| root.path.as_str()),
                    roots.len().saturating_sub(1)
                ),
            }
        }
        ItemKind::Reference(Reference::Data(representations)) => format!(
            "{} · {} bytes",
            representations
                .first()
                .map_or("unknown", |representation| representation.mime()),
            item.logical_size()
        ),
    }
}

fn binary_preview(descriptor: &PayloadDescriptor) -> String {
    let mime = descriptor
        .representations()
        .first()
        .map_or("unknown", |representation| representation.mime());
    format!("{mime} · {} bytes", descriptor.logical_size())
}
