#![allow(
    clippy::wildcard_imports,
    reason = "UI tests share fixtures and private helpers"
)]

use std::{
    collections::{BTreeSet, HashSet},
    sync::{Arc, mpsc as std_mpsc},
    time::{Duration, Instant},
};

use super::{
    Presentation, global_shortcut::*, history::*, ipc_types::*, ipc_worker::*,
    signal_closes_presentation, singleton::*, style::*, window::*,
};
use crate::{
    config::AppPaths,
    ipc::protocol::{
        HistoryItem, HistoryRequest, HistoryResponse, IPC_PROTOCOL_VERSION, MutationResponse,
        Request, Response, ShareClipboardRequest, ShareClipboardResponse, StatusResponse, request,
        response,
    },
};
use eframe::egui::{self, Frame, Key, Margin, Vec2};

fn assert_approx_eq(left: f32, right: f32) {
    assert!((left - right).abs() < f32::EPSILON, "{left} != {right}");
}

fn history_item(content_id: &str) -> HistoryItem {
    HistoryItem {
        content_id: content_id.to_owned(),
        preview: format!("preview {content_id}"),
        mime_types: vec!["text/plain".to_owned()],
        logical_size: 42,
        source_node: "node".to_owned(),
        pinned: false,
        source_device: "vd".to_owned(),
        physical_millis: 0,
    }
}

fn egui_input(size: Vec2, key: Option<Key>) -> egui::RawInput {
    let mut input = egui::RawInput {
        screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
        ..Default::default()
    };
    if let Some(key) = key {
        input.events.push(egui::Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::NONE,
        });
    }
    input
}

#[derive(Clone, Copy)]
enum FocusedHistoryControl {
    Share,
    Activate,
    Pin,
    Delete,
}

mod global_shortcut;
mod history_model;
mod history_widgets;
mod ipc;
mod presentation;
mod singleton;
mod window;
