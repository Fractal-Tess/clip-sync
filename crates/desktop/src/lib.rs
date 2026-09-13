//! Native ClipSync clipboard picker.

mod app;
mod control;
mod daemon;
mod theme;

use std::{path::PathBuf, time::Instant};

use anyhow::{Context, Result};
use winit::event_loop::{ControlFlow, EventLoop};

pub use daemon::Daemon;

/// Opens the desktop window and blocks until the user picks or dismisses.
///
/// Opens on the control centre when `control` is set, and on the clipboard
/// picker otherwise.
///
/// # Errors
///
/// Returns an error when the daemon cannot be reached or the windowing system
/// refuses to start an event loop.
pub fn run(config_override: Option<PathBuf>, control: bool) -> Result<()> {
    let started = Instant::now();
    let daemon = Daemon::discover(config_override)?;
    let event_loop = EventLoop::new().context("failed to start the window event loop")?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let mut picker = app::Picker::new(daemon, started, control);
    event_loop
        .run_app(&mut picker)
        .context("the window event loop failed")?;
    Ok(())
}
