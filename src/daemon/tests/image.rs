use std::io::Cursor;

use super::super::{
    activation::image_focused_activation_content,
    preview::{decode_image_preview, image_format_for_mime},
};
use crate::clipboard::types::{ClipboardContent, ClipboardRepresentation, MimeType};

#[test]
fn image_activation_excludes_url_and_browser_metadata() {
    let content = ClipboardContent::new(vec![
        ClipboardRepresentation::new(
            MimeType::new("chromium/x-source-url").unwrap(),
            b"file:///tmp/image.png".to_vec(),
        ),
        ClipboardRepresentation::new(
            MimeType::new("image/png").unwrap(),
            vec![0x89, b'P', b'N', b'G'],
        ),
        ClipboardRepresentation::new(
            MimeType::new("text/html").unwrap(),
            b"<img src=\"file:///tmp/image.png\">".to_vec(),
        ),
    ])
    .unwrap();

    let activated = image_focused_activation_content(content).unwrap();

    assert_eq!(activated.representations().len(), 1);
    assert_eq!(
        activated.representations()[0].mime_type().as_str(),
        "image/png"
    );
    assert_eq!(
        activated.representations()[0].bytes(),
        [0x89, b'P', b'N', b'G']
    );
}

#[test]
fn non_image_activation_preserves_the_complete_bundle() {
    let content = ClipboardContent::new(vec![
        ClipboardRepresentation::new(MimeType::new("text/plain").unwrap(), b"plain".to_vec()),
        ClipboardRepresentation::new(
            MimeType::new("text/html").unwrap(),
            b"<b>plain</b>".to_vec(),
        ),
    ])
    .unwrap();

    let activated = image_focused_activation_content(content.clone()).unwrap();

    assert_eq!(activated, content);
}

#[test]
fn raster_image_preview_is_bounded_rgba() {
    let source = image::RgbaImage::from_pixel(640, 360, image::Rgba([20, 80, 140, 255]));
    let mut encoded = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(source)
        .write_to(&mut encoded, image::ImageFormat::Png)
        .unwrap();

    let preview =
        decode_image_preview("content-id", "image/png".to_owned(), encoded.into_inner()).unwrap();

    assert_eq!(preview.content_id, "content-id");
    assert_eq!(preview.mime_type, "image/png");
    assert_eq!((preview.width, preview.height), (320, 180));
    assert_eq!(preview.rgba.len(), 320 * 180 * 4);
}

#[test]
fn vector_images_are_not_offered_as_raster_previews() {
    assert_eq!(image_format_for_mime("image/svg+xml"), None);
    assert_eq!(
        image_format_for_mime("image/jpeg; charset=binary"),
        Some(image::ImageFormat::Jpeg)
    );
}
