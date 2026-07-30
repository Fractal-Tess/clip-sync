mod activation;
mod capture;
mod clipboard;
mod commands;
mod config_supervision;
mod mesh_persistence;
mod preview;
mod runtime;
mod views;

pub use capture::{
    AutomaticClipboardCaptureResult, LiveClipboardShareInspection, ShareCurrentClipboardResult,
    capture_automatic_clipboard, inspect_current_clipboard, inspect_live_current_clipboard,
    share_current_clipboard, share_live_current_clipboard,
};
pub use commands::cancel_transfer;
pub use runtime::run;

#[cfg(test)]
mod tests;
