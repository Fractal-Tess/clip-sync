use eframe::egui;

use crate::{
    ipc::protocol::{
        ConfigResponse, DiagnosticsResponse, HistoryResponse, ImagePreviewResponse,
        MutationResponse, PeersResponse, Response, ShareClipboardResponse, StatusResponse,
        TransfersResponse, response,
    },
    ui::style::{MAX_IMAGE_PREVIEW_HEIGHT, MAX_IMAGE_PREVIEW_WIDTH},
};

fn response_error(response: Response) -> Result<response::Body, String> {
    match response.body {
        Some(response::Body::Error(error)) => Err(format!("{}: {}", error.code, error.message)),
        Some(body) => Ok(body),
        None => Err("daemon returned an empty response".to_owned()),
    }
}

pub(super) fn expect_status(result: Result<Response, String>) -> Result<StatusResponse, String> {
    match response_error(result?)? {
        response::Body::Status(value) => Ok(value),
        _ => Err("daemon returned an unexpected status response".to_owned()),
    }
}

pub(super) fn expect_history(result: Result<Response, String>) -> Result<HistoryResponse, String> {
    match response_error(result?)? {
        response::Body::History(value) => Ok(value),
        _ => Err("daemon returned an unexpected history response".to_owned()),
    }
}

pub(super) fn expect_image_preview(
    result: Result<Response, String>,
) -> Result<ImagePreviewResponse, String> {
    match response_error(result?)? {
        response::Body::ImagePreview(value) => Ok(value),
        _ => Err("daemon returned an unexpected image-preview response".to_owned()),
    }
}

pub(super) fn expect_peers(result: Result<Response, String>) -> Result<PeersResponse, String> {
    match response_error(result?)? {
        response::Body::Peers(value) => Ok(value),
        _ => Err("daemon returned an unexpected peers response".to_owned()),
    }
}

pub(super) fn expect_config(result: Result<Response, String>) -> Result<ConfigResponse, String> {
    match response_error(result?)? {
        response::Body::Config(value) => Ok(value),
        _ => Err("daemon returned an unexpected config response".to_owned()),
    }
}

pub(super) fn expect_diagnostics(
    result: Result<Response, String>,
) -> Result<DiagnosticsResponse, String> {
    match response_error(result?)? {
        response::Body::Diagnostics(value) => Ok(value),
        _ => Err("daemon returned an unexpected diagnostics response".to_owned()),
    }
}

pub(super) fn expect_transfers(
    result: Result<Response, String>,
) -> Result<TransfersResponse, String> {
    match response_error(result?)? {
        response::Body::Transfers(value) => Ok(value),
        _ => Err("daemon returned an unexpected transfers response".to_owned()),
    }
}

pub(super) fn expect_share(
    result: Result<Response, String>,
) -> Result<ShareClipboardResponse, String> {
    match response_error(result?)? {
        response::Body::ShareClipboard(value) => Ok(value),
        _ => Err("daemon returned an unexpected clipboard-share response".to_owned()),
    }
}

pub(super) fn expect_mutation(
    result: Result<Response, String>,
) -> Result<MutationResponse, String> {
    match response_error(result?)? {
        response::Body::Mutation(value) => Ok(value),
        _ => Err("daemon returned an unexpected mutation response".to_owned()),
    }
}

pub(in crate::ui) fn preview_texture(
    context: &egui::Context,
    requested_content_id: &str,
    preview: &ImagePreviewResponse,
) -> Result<egui::TextureHandle, String> {
    if preview.content_id != requested_content_id {
        return Err("image preview content ID did not match its request".to_owned());
    }
    if preview.width == 0
        || preview.height == 0
        || preview.width > MAX_IMAGE_PREVIEW_WIDTH
        || preview.height > MAX_IMAGE_PREVIEW_HEIGHT
    {
        return Err("image preview dimensions are invalid".to_owned());
    }
    let width = usize::try_from(preview.width)
        .map_err(|_| "image preview width does not fit in memory".to_owned())?;
    let height = usize::try_from(preview.height)
        .map_err(|_| "image preview height does not fit in memory".to_owned())?;
    let expected = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "image preview dimensions overflow".to_owned())?;
    if preview.rgba.len() != expected {
        return Err("image preview pixel data has the wrong length".to_owned());
    }
    let image = egui::ColorImage::from_rgba_unmultiplied([width, height], &preview.rgba);
    Ok(context.load_texture(
        format!("clip-sync-preview-{requested_content_id}"),
        image,
        egui::TextureOptions::LINEAR,
    ))
}
