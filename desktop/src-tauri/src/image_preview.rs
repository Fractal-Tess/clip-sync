use clip_sync_ipc::protocol::ImagePreviewResponse;

use crate::views::ImagePreviewView;

pub(crate) fn image_preview_view(
    requested_content_id: &str,
    preview: ImagePreviewResponse,
) -> Result<ImagePreviewView, String> {
    const MAX_WIDTH: u32 = 320;
    const MAX_HEIGHT: u32 = 180;

    if preview.content_id != requested_content_id {
        return Err("image preview content ID did not match its request".to_owned());
    }
    if preview.width == 0
        || preview.height == 0
        || preview.width > MAX_WIDTH
        || preview.height > MAX_HEIGHT
    {
        return Err("image preview dimensions are invalid".to_owned());
    }
    let expected_length = usize::try_from(preview.width)
        .ok()
        .and_then(|width| {
            usize::try_from(preview.height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "image preview dimensions overflow".to_owned())?;
    if preview.rgba.len() != expected_length {
        return Err("image preview pixel data has the wrong length".to_owned());
    }

    Ok(ImagePreviewView {
        content_id: preview.content_id,
        mime_type: preview.mime_type,
        width: preview.width,
        height: preview.height,
        rgba: preview.rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn preview(width: u32, height: u32, rgba: Vec<u8>) -> ImagePreviewResponse {
        ImagePreviewResponse {
            content_id: "content-id".to_owned(),
            mime_type: "image/png".to_owned(),
            width,
            height,
            rgba,
        }
    }

    #[test]
    fn accepts_bounded_rgba_preview() {
        let view = image_preview_view("content-id", preview(2, 1, vec![255; 8]))
            .expect("valid image preview");

        assert_eq!(view.content_id, "content-id");
        assert_eq!((view.width, view.height), (2, 1));
        assert_eq!(view.rgba, vec![255; 8]);
    }

    #[test]
    fn rejects_mismatched_content_id() {
        let error = image_preview_view("different-id", preview(2, 1, vec![255; 8]))
            .expect_err("mismatched preview must fail");

        assert!(error.contains("did not match"));
    }

    #[test]
    fn rejects_invalid_dimensions_and_pixel_length() {
        assert!(image_preview_view("content-id", preview(0, 1, Vec::new())).is_err());
        assert!(image_preview_view("content-id", preview(2, 1, vec![255; 7])).is_err());
        assert!(image_preview_view("content-id", preview(321, 1, vec![255; 321 * 4])).is_err());
    }
}
