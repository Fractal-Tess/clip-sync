use std::io::Cursor;

use anyhow::Context;

use clip_sync_core::storage::{HistoryStore, LocalSource};
use clip_sync_ipc::protocol::ImagePreviewResponse;

const IMAGE_PREVIEW_WIDTH: u32 = 320;
const IMAGE_PREVIEW_HEIGHT: u32 = 180;
const MAX_IMAGE_PREVIEW_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_IMAGE_PREVIEW_DIMENSION: u32 = 8192;
const MAX_IMAGE_PREVIEW_DECODE_BYTES: u64 = 128 * 1024 * 1024;

/// Thumbnails an image item whose bytes are on this device: inline items,
/// and large images this device copied. An image stored only on another
/// device is not fetched just to draw a thumbnail.
pub(super) fn image_preview(
    encoded_content_id: &str,
    history: &HistoryStore,
) -> anyhow::Result<ImagePreviewResponse> {
    let content_id = encoded_content_id
        .parse()
        .context("content ID is invalid")?;
    if !history.projection().is_visible(content_id) {
        anyhow::bail!("history item is deleted");
    }

    let payload = match history.load_payload(content_id).context("load image")? {
        Some(payload) => payload,
        None => match history.local_source(content_id).context("load image")? {
            Some(LocalSource::Data(payload)) => payload,
            _ => anyhow::bail!("image is stored on another device"),
        },
    };
    let representation = payload
        .representations()
        .iter()
        .find(|representation| image_format_for_mime(representation.mime()).is_some())
        .context("history item has no supported raster image")?;
    let source_size = u64::try_from(representation.bytes().len())
        .context("image preview source size does not fit in u64")?;
    if source_size > MAX_IMAGE_PREVIEW_SOURCE_BYTES {
        anyhow::bail!("image is too large to preview safely");
    }
    decode_image_preview(
        encoded_content_id,
        representation.mime().to_owned(),
        representation.bytes().to_vec(),
    )
}

pub(super) fn decode_image_preview(
    content_id: &str,
    mime_type: String,
    bytes: Vec<u8>,
) -> anyhow::Result<ImagePreviewResponse> {
    let format = image_format_for_mime(&mime_type).context("unsupported raster image type")?;
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_PREVIEW_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_PREVIEW_DIMENSION);
    limits.max_alloc = Some(MAX_IMAGE_PREVIEW_DECODE_BYTES);
    reader.limits(limits);
    let image = reader.decode().context("decode clipboard image preview")?;
    let thumbnail = image
        .thumbnail(IMAGE_PREVIEW_WIDTH, IMAGE_PREVIEW_HEIGHT)
        .to_rgba8();
    let (width, height) = thumbnail.dimensions();
    Ok(ImagePreviewResponse {
        content_id: content_id.to_owned(),
        mime_type,
        width,
        height,
        rgba: thumbnail.into_raw(),
    })
}

pub(super) fn image_format_for_mime(mime: &str) -> Option<image::ImageFormat> {
    match mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/png" => Some(image::ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(image::ImageFormat::Jpeg),
        "image/gif" => Some(image::ImageFormat::Gif),
        "image/webp" => Some(image::ImageFormat::WebP),
        "image/bmp" | "image/x-ms-bmp" => Some(image::ImageFormat::Bmp),
        "image/tiff" => Some(image::ImageFormat::Tiff),
        _ => None,
    }
}
