use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc,
    },
    thread::JoinHandle,
    time::Duration,
};

use eframe::egui;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use wayland_client::{
    Connection, Dispatch, QueueHandle,
    globals::{GlobalListContents, registry_queue_init},
    protocol::wl_registry,
};

use super::singleton::UiSignal;

pub(super) const GLOBAL_SHORTCUT_APP_ID: &str = "clip-sync";
pub(super) const CLOSE_QUICK_SHORTCUT_ID: &str = "close-quick";

mod protocol {
    use wayland_client;

    pub mod __interfaces {
        use wayland_client::backend as wayland_backend;

        wayland_scanner::generate_interfaces!("./protocols/hyprland-global-shortcuts-v1.xml");
    }

    use self::__interfaces::{
        HYPRLAND_GLOBAL_SHORTCUT_V1_INTERFACE, HYPRLAND_GLOBAL_SHORTCUTS_MANAGER_V1_INTERFACE,
    };
    wayland_scanner::generate_client_code!("./protocols/hyprland-global-shortcuts-v1.xml");
}

use protocol::{
    hyprland_global_shortcut_v1::{self, HyprlandGlobalShortcutV1},
    hyprland_global_shortcuts_manager_v1::{self, HyprlandGlobalShortcutsManagerV1},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ShortcutEventKind {
    Pressed,
    Released,
}

pub(super) const fn signal_for_shortcut_event(kind: ShortcutEventKind) -> Option<UiSignal> {
    match kind {
        ShortcutEventKind::Pressed => Some(UiSignal::CloseQuick),
        ShortcutEventKind::Released => None,
    }
}

pub(super) struct GlobalShortcutListener {
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl GlobalShortcutListener {
    pub(super) fn start(context: egui::Context, signal_tx: std_mpsc::Sender<UiSignal>) -> Self {
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_shutdown = Arc::clone(&shutdown);
        let thread = std::thread::Builder::new()
            .name("clip-sync-global-shortcut".to_owned())
            .spawn(move || {
                if let Err(error) = run_listener(&worker_shutdown, context, signal_tx) {
                    tracing::debug!(%error, "Hyprland global shortcut unavailable");
                }
            })
            .map_err(|error| {
                tracing::debug!(%error, "could not start Hyprland global shortcut listener");
            })
            .ok();
        Self { shutdown, thread }
    }
}

impl Drop for GlobalShortcutListener {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::debug!("Hyprland global shortcut listener panicked during shutdown");
        }
    }
}

struct ShortcutState {
    context: egui::Context,
    signal_tx: std_mpsc::Sender<UiSignal>,
}

fn run_listener(
    shutdown: &AtomicBool,
    context: egui::Context,
    signal_tx: std_mpsc::Sender<UiSignal>,
) -> Result<(), String> {
    let connection = Connection::connect_to_env()
        .map_err(|error| format!("could not connect to Wayland: {error}"))?;
    let (globals, mut event_queue) = registry_queue_init::<ShortcutState>(&connection)
        .map_err(|error| format!("could not enumerate Wayland globals: {error}"))?;
    let queue_handle = event_queue.handle();
    let manager = globals
        .bind::<HyprlandGlobalShortcutsManagerV1, _, _>(&queue_handle, 1..=1, ())
        .map_err(|error| format!("hyprland_global_shortcuts_v1 is unavailable: {error}"))?;
    let shortcut = manager.register_shortcut(
        CLOSE_QUICK_SHORTCUT_ID.to_owned(),
        GLOBAL_SHORTCUT_APP_ID.to_owned(),
        "Close ClipSync Quick History".to_owned(),
        "Compositor-defined global shortcut".to_owned(),
        &queue_handle,
        (),
    );
    connection
        .flush()
        .map_err(|error| format!("could not register global shortcut: {error}"))?;

    let mut state = ShortcutState { context, signal_tx };
    let timeout = Timespec::try_from(Duration::from_millis(100))
        .expect("100 milliseconds fits in a Wayland poll timeout");
    while !shutdown.load(Ordering::Acquire) {
        event_queue
            .dispatch_pending(&mut state)
            .map_err(|error| format!("could not dispatch global shortcut event: {error}"))?;
        event_queue
            .flush()
            .map_err(|error| format!("could not flush global shortcut connection: {error}"))?;
        let Some(read_guard) = event_queue.prepare_read() else {
            continue;
        };
        let mut poll_fds = [PollFd::from_borrowed_fd(
            read_guard.connection_fd(),
            PollFlags::IN,
        )];
        if poll(&mut poll_fds, Some(&timeout))
            .map_err(|error| format!("could not poll global shortcut connection: {error}"))?
            > 0
        {
            read_guard
                .read()
                .map_err(|error| format!("could not read global shortcut event: {error}"))?;
        }
    }

    shortcut.destroy();
    manager.destroy();
    let _ = connection.flush();
    Ok(())
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for ShortcutState {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<HyprlandGlobalShortcutsManagerV1, ()> for ShortcutState {
    fn event(
        _state: &mut Self,
        _proxy: &HyprlandGlobalShortcutsManagerV1,
        _event: hyprland_global_shortcuts_manager_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<HyprlandGlobalShortcutV1, ()> for ShortcutState {
    fn event(
        state: &mut Self,
        _proxy: &HyprlandGlobalShortcutV1,
        event: hyprland_global_shortcut_v1::Event,
        _data: &(),
        _connection: &Connection,
        _queue_handle: &QueueHandle<Self>,
    ) {
        let kind = match event {
            hyprland_global_shortcut_v1::Event::Pressed { .. } => ShortcutEventKind::Pressed,
            hyprland_global_shortcut_v1::Event::Released { .. } => ShortcutEventKind::Released,
        };
        if let Some(signal) = signal_for_shortcut_event(kind)
            && state.signal_tx.send(signal).is_ok()
        {
            state.context.request_repaint();
        }
    }
}
