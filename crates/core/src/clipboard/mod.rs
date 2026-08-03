//! Clipboard monitoring subsystem.
//!
//! This module defines backend-neutral types and traits for clipboard change
//! detection, plus a Wayland-native backend that probes for `ext-data-control-v1`
//! (preferred) or `zwlr-data-control-v1` (legacy fallback).
//!
//! # Milestone 0 status
//!
//! - **Probe**: functional — connects to Wayland, detects protocol support.
//! - **Watch/capture**: Wayland data-control event loop captures regular
//!   clipboard offers and serves daemon-owned content while the watcher lives.

pub mod backend;
pub mod types;
pub mod wayland;
