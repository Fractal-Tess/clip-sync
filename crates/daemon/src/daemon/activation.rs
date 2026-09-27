use std::path::{Path, PathBuf};

use anyhow::Context;
use tokio::sync::mpsc;

use clip_sync_core::{
    clipboard::{
        backend::ClipboardBackend,
        types::{ClipboardContent, ClipboardRepresentation, MimeType},
        wayland::WaylandBackend,
    },
    files,
    model::{ContentId, FileEntry, ItemKind, OpId, Payload, Reference},
    storage::{HistoryStore, LocalSource},
};

use crate::{
    ipc::DaemonState,
    mesh::{Fetched, MeshHandle},
};

use super::{runtime::unix_time_millis, views::history_items};

/// A fetch from another device that finished, successfully or not.
pub(super) struct FetchFinished {
    pub(super) content_id: ContentId,
    pub(super) operation: OpId,
    pub(super) result: Result<Fetched, String>,
}

pub(super) struct Activation<'a> {
    pub(super) clipboard: &'a WaylandBackend,
    pub(super) history: &'a mut HistoryStore,
    pub(super) state: &'a DaemonState,
    pub(super) mesh: &'a MeshHandle,
    pub(super) cache_dir: &'a Path,
    pub(super) fetches: &'a mpsc::UnboundedSender<FetchFinished>,
}

impl Activation<'_> {
    /// Puts a history item on the clipboard, or starts fetching it from the
    /// device that holds its bytes. Returns what happened, for the caller.
    pub(super) async fn activate(&mut self, encoded_content_id: &str) -> anyhow::Result<String> {
        let content_id: ContentId = encoded_content_id
            .parse()
            .context("content ID is invalid")?;
        let projection = self.history.projection();
        if !projection.is_visible(content_id) {
            anyhow::bail!("history item is deleted");
        }
        let item = projection
            .item(content_id)
            .cloned()
            .context("history item has not finished syncing yet")?;
        let operation = projection
            .item_operation(content_id)
            .context("history item has not finished syncing yet")?;

        let content = match item {
            ItemKind::Inline { .. } => {
                let payload = self
                    .history
                    .load_payload(content_id)
                    .context("load history item")?
                    .context("history item has not finished syncing yet")?;
                payload_content(&payload)?
            }
            ItemKind::Reference(reference) => {
                if operation.node() == self.history.replica().node_id() {
                    let source = self
                        .history
                        .local_source(content_id)
                        .context("find the copied item on this device")?
                        .context("this device no longer has the copied item")?;
                    local_source_content(&reference, &source)?
                } else {
                    let cached = fetch_directory(self.cache_dir, content_id, operation);
                    match (&reference, cached.is_dir()) {
                        (Reference::Files(entries), true) => files_content(&cached, entries)?,
                        _ => return self.start_fetch(content_id, operation, reference).await,
                    }
                }
            }
        };
        self.set_clipboard(content_id, content).await?;
        Ok("clipboard activated".to_owned())
    }

    async fn start_fetch(
        &self,
        content_id: ContentId,
        operation: OpId,
        reference: Reference,
    ) -> anyhow::Result<String> {
        let origin = operation.node();
        if !self.mesh.is_connected(origin).await {
            anyhow::bail!(
                "{} is offline; this item's bytes are only on that device",
                device_name(self.mesh, origin).await
            );
        }
        let destination = fetch_directory(self.cache_dir, content_id, operation);
        let mesh = self.mesh.clone();
        let fetches = self.fetches.clone();
        tokio::spawn(async move {
            let result = mesh
                .fetch(content_id, operation, &reference, &destination)
                .await
                .map_err(|error| error.to_string());
            let _ = fetches.send(FetchFinished {
                content_id,
                operation,
                result,
            });
        });
        Ok(format!(
            "fetching from {}; it goes on the clipboard when ready",
            device_name(self.mesh, origin).await
        ))
    }

    /// Finishes an activation whose bytes arrived from another device.
    pub(super) async fn finish_fetch(&mut self, finished: FetchFinished) {
        let FetchFinished {
            content_id,
            operation,
            result,
        } = finished;
        let from = device_name(self.mesh, operation.node()).await;
        let content = result.map_err(anyhow::Error::msg).and_then(|fetched| {
            match (&fetched, self.history.projection().item(content_id)) {
                (
                    Fetched::Files(directory),
                    Some(ItemKind::Reference(Reference::Files(entries))),
                ) => files_content(directory, entries),
                (Fetched::Data(payload), _) => payload_content(payload),
                _ => anyhow::bail!("the item changed while it was being fetched"),
            }
        });
        match content {
            Ok(content) => match self.set_clipboard(content_id, content).await {
                Ok(()) => {
                    notify("Ready to paste", &format!("Fetched from {from}"));
                    let cache = self.cache_dir.join("fetched");
                    let limit = self.state.config().await.local.fetch_cache_bytes;
                    tokio::task::spawn_blocking(move || evict_fetch_cache(&cache, limit));
                }
                Err(error) => {
                    tracing::warn!(%error, "fetched item could not be put on the clipboard");
                    notify("Could not paste", &format!("{error:#}"));
                }
            },
            Err(error) => {
                tracing::warn!(%error, "fetch did not complete");
                notify(
                    &format!("Could not fetch from {from}"),
                    &format!("{error:#}"),
                );
            }
        }
    }

    async fn set_clipboard(
        &mut self,
        content_id: ContentId,
        content: ClipboardContent,
    ) -> anyhow::Result<()> {
        self.clipboard
            .set_clipboard_content(content)
            .await
            .context("set active Wayland clipboard")?;
        let operation = self
            .history
            .activate(content_id, unix_time_millis()?)
            .context("persist clipboard activation")?;
        self.mesh
            .record_local(&operation)
            .await
            .context("publish clipboard activation to mesh")?;
        self.state
            .set_history(history_items(self.history.replica()))
            .await;
        Ok(())
    }
}

/// Fetched copies are keyed by the operation too, so a re-published item
/// (a file edited in place) is fetched afresh rather than served stale.
fn fetch_directory(cache_dir: &Path, content_id: ContentId, operation: OpId) -> PathBuf {
    cache_dir
        .join("fetched")
        .join(format!("{content_id}-{}", operation.counter()))
}

fn payload_content(payload: &Payload) -> anyhow::Result<ClipboardContent> {
    let representations = payload
        .representations()
        .iter()
        .map(|representation| {
            let mime = MimeType::new(representation.mime())
                .context("stored MIME type cannot be served")?;
            Ok(ClipboardRepresentation::new(mime, representation.bytes()))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let content = ClipboardContent::new_with_max(representations, u64::MAX)
        .context("stored history item cannot be served")?;
    image_focused_activation_content(content).context("prepare history item for activation")
}

fn local_source_content(
    reference: &Reference,
    source: &LocalSource,
) -> anyhow::Result<ClipboardContent> {
    match (reference, source) {
        (Reference::Files(entries), LocalSource::Files(sources)) => {
            let roots = entries
                .iter()
                .zip(sources)
                .filter(|(entry, _)| !entry.path.contains('/'))
                .map(|(entry, source)| (source.path.as_path(), entry.directory))
                .collect::<Vec<_>>();
            for (path, _) in &roots {
                anyhow::ensure!(path.exists(), "{} no longer exists", path.display());
            }
            file_list_content(&files::uri_list_for_paths(roots)?)
        }
        (Reference::Data(_), LocalSource::Data(payload)) => payload_content(payload),
        _ => anyhow::bail!("the copied item on this device does not match its description"),
    }
}

fn files_content(root: &Path, entries: &[FileEntry]) -> anyhow::Result<ClipboardContent> {
    file_list_content(&files::uri_list(root, entries)?)
}

/// Offers files the way file managers expect: `text/uri-list` for most, and
/// GNOME's copied-files list for Nautilus and its relatives.
fn file_list_content(uri_list: &[u8]) -> anyhow::Result<ClipboardContent> {
    let mut gnome = b"copy\n".to_vec();
    gnome.extend(
        String::from_utf8_lossy(uri_list)
            .lines()
            .collect::<Vec<_>>()
            .join("\n")
            .as_bytes(),
    );
    ClipboardContent::new_with_max(
        vec![
            ClipboardRepresentation::new(MimeType::new("text/uri-list")?, uri_list.to_vec()),
            ClipboardRepresentation::new(MimeType::new("x-special/gnome-copied-files")?, gnome),
        ],
        u64::MAX,
    )
    .context("build file clipboard offer")
}

pub(super) fn image_focused_activation_content(
    content: ClipboardContent,
) -> Result<ClipboardContent, clip_sync_core::clipboard::types::ClipboardContentError> {
    if !content
        .representations()
        .iter()
        .any(|representation| clipboard_mime_is_image(representation.mime_type().as_str()))
    {
        return Ok(content);
    }

    let representations = content
        .representations()
        .iter()
        .filter(|representation| clipboard_mime_is_image(representation.mime_type().as_str()))
        .cloned()
        .collect();
    ClipboardContent::new_with_max(representations, u64::MAX)
}

fn clipboard_mime_is_image(mime: &str) -> bool {
    let essence = mime.split(';').next().unwrap_or(mime).trim();
    essence
        .get(..6)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("image/"))
}

async fn device_name(mesh: &MeshHandle, node: clip_sync_core::model::NodeId) -> String {
    mesh.device_hostnames()
        .await
        .get(&node)
        .cloned()
        .unwrap_or_else(|| "the device that copied it".to_owned())
}

/// Best effort: a desktop without a notification daemon just misses it.
fn notify(summary: &str, body: &str) {
    let spawned = std::process::Command::new("notify-send")
        .args(["--app-name=ClipSync", summary, body])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || child.wait());
    }
}

/// Removes the least recently fetched items until the cache fits `limit`.
/// The originals remain on their devices, so nothing is lost.
fn evict_fetch_cache(cache: &Path, limit: u64) {
    let Ok(entries) = std::fs::read_dir(cache) else {
        return;
    };
    let mut items = entries
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, directory_size(&entry.path()), entry.path()))
        })
        .collect::<Vec<_>>();
    items.sort_unstable_by_key(|(modified, _, _)| *modified);
    let mut total = items.iter().map(|(_, size, _)| *size).sum::<u64>();
    for (_, size, path) in items {
        if total <= limit {
            break;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            total = total.saturating_sub(size);
        }
    }
}

fn directory_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !metadata.is_dir() {
        return metadata.len();
    }
    std::fs::read_dir(path).map_or(0, |entries| {
        entries
            .filter_map(Result::ok)
            .map(|entry| directory_size(&entry.path()))
            .sum()
    })
}
