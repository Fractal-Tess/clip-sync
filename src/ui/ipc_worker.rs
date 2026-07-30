use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc as std_mpsc},
    thread::JoinHandle,
    time::Duration,
};

use eframe::egui;
use tokio_util::sync::CancellationToken;

use crate::{
    ipc::{
        self,
        protocol::{IPC_PROTOCOL_VERSION, Request, StatusRequest, request},
    },
    ui::{
        ipc_types::{IpcDispatchClass, UiCommand, UiEvent, request_with_shared_daemon_start},
        style::{MAX_UI_IPC_CONCURRENCY, UI_IPC_QUEUE_CAPACITY},
    },
};

enum IpcDispatchGuard {
    ReadOnly,
    Mutation {
        _guard: tokio::sync::OwnedRwLockReadGuard<()>,
    },
    Share {
        _guard: tokio::sync::OwnedRwLockWriteGuard<()>,
    },
}

async fn acquire_dispatch_guard(
    dispatch_class: IpcDispatchClass,
    gate: Arc<tokio::sync::RwLock<()>>,
) -> IpcDispatchGuard {
    match dispatch_class {
        IpcDispatchClass::ReadOnly => IpcDispatchGuard::ReadOnly,
        IpcDispatchClass::Mutation => IpcDispatchGuard::Mutation {
            _guard: gate.read_owned().await,
        },
        IpcDispatchClass::Share => IpcDispatchGuard::Share {
            _guard: gate.write_owned().await,
        },
    }
}

pub(super) struct IpcWorker {
    command_tx: Option<tokio::sync::mpsc::Sender<UiCommand>>,
    shutdown: CancellationToken,
    thread: Option<JoinHandle<()>>,
}

impl IpcWorker {
    pub(super) fn send(&self, command: UiCommand) -> Result<(), String> {
        self.command_tx
            .as_ref()
            .expect("IPC sender exists until worker drop")
            .try_send(command)
            .map_err(|error| format!("the bounded local IPC queue is unavailable: {error}"))
    }
}

impl Drop for IpcWorker {
    fn drop(&mut self) {
        self.shutdown.cancel();
        self.command_tx.take();
        if let Some(thread) = self.thread.take()
            && thread.join().is_err()
        {
            tracing::debug!("local UI IPC worker panicked during shutdown");
        }
    }
}

pub(super) fn spawn_ipc_worker(
    socket: PathBuf,
    event_tx: std_mpsc::Sender<UiEvent>,
    context: egui::Context,
) -> IpcWorker {
    let (command_tx, mut command_rx) =
        tokio::sync::mpsc::channel::<UiCommand>(UI_IPC_QUEUE_CAPACITY);
    let shutdown = CancellationToken::new();
    let worker_shutdown = shutdown.clone();
    let thread = std::thread::Builder::new()
        .name("clip-sync-ui-ipc".to_owned())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("clip-sync-ui-ipc-task")
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = event_tx.send(UiEvent::Status(Err(format!(
                        "could not start the local IPC runtime: {error}"
                    ))));
                    context.request_repaint();
                    return;
                }
            };
            runtime.block_on(async move {
                let limiter = Arc::new(tokio::sync::Semaphore::new(MAX_UI_IPC_CONCURRENCY));
                let mutation_gate = Arc::new(tokio::sync::RwLock::new(()));
                let daemon_start_gate = Arc::new(tokio::sync::Mutex::new(()));
                let mut tasks = tokio::task::JoinSet::new();
                let mut request_id = 0_u64;
                loop {
                    tokio::select! {
                        biased;
                        () = worker_shutdown.cancelled() => break,
                        joined = tasks.join_next(), if !tasks.is_empty() => {
                            if let Some(Err(error)) = joined
                                && !error.is_cancelled()
                            {
                                tracing::debug!(%error, "UI IPC request task failed");
                            }
                        }
                        command = command_rx.recv() => {
                            let Some(command) = command else {
                                break;
                            };
                            request_id = request_id.saturating_add(1);
                            let request_id = request_id;
                            let socket = socket.clone();
                            let event_tx = event_tx.clone();
                            let context = context.clone();
                            let daemon_start_gate = Arc::clone(&daemon_start_gate);
                            let mutation_gate = Arc::clone(&mutation_gate);
                            let limiter = Arc::clone(&limiter);
                            let shutdown = worker_shutdown.clone();
                            let dispatch_class = command.dispatch_class();
                            let (body, target) = command.request_body();
                            tasks.spawn(async move {
                                let _mutation_guard =
                                    acquire_dispatch_guard(dispatch_class, mutation_gate).await;
                                let Ok(_permit) = limiter.acquire_owned().await else {
                                    return;
                                };
                                let request = Request {
                                    protocol_version: IPC_PROTOCOL_VERSION,
                                    request_id,
                                    body: Some(body),
                                };
                                let response = tokio::select! {
                                    biased;
                                    () = shutdown.cancelled() => return,
                                    response = request_with_shared_daemon_start(
                                        &socket,
                                        request,
                                        &daemon_start_gate,
                                    ) => response,
                                };
                                if event_tx.send(target.into_event(response)).is_ok() {
                                    context.request_repaint();
                                }
                            });
                        }
                    }
                }
                tasks.abort_all();
                while tasks.join_next().await.is_some() {}
            });
        })
        .ok();
    IpcWorker {
        command_tx: Some(command_tx),
        shutdown,
        thread,
    }
}
pub(super) fn daemon_readiness_probe() -> Request {
    Request {
        protocol_version: IPC_PROTOCOL_VERSION,
        request_id: 0,
        body: Some(request::Body::Status(StatusRequest {})),
    }
}

pub(super) async fn wait_for_daemon_ready(
    socket: &Path,
    probe: Request,
    attempts: usize,
    delay: Duration,
    start_detail: &str,
) -> Result<(), String> {
    for _ in 0..attempts {
        tokio::time::sleep(delay).await;
        match ipc::request(socket, probe.clone()).await {
            Ok(_) => return Ok(()),
            Err(error) if is_daemon_absent(&error) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Err(format!(
        "daemon did not become ready at {} ({start_detail}); start clip-sync.service or run `clip-sync daemon`",
        socket.display()
    ))
}

pub(super) fn is_daemon_absent(error: &ipc::IpcError) -> bool {
    matches!(
        error,
        ipc::IpcError::Io(io_error)
            if matches!(
                io_error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            )
    )
}

pub(super) async fn start_user_service() -> String {
    let mut command = tokio::process::Command::new("systemctl");
    command.args(["--user", "start", "clip-sync.service"]);
    command.kill_on_drop(true);
    match tokio::time::timeout(Duration::from_secs(3), command.output()).await {
        Ok(Ok(output)) if output.status.success() => {
            "requested systemd user service start".to_owned()
        }
        Ok(Ok(output)) => {
            let detail = String::from_utf8_lossy(&output.stderr);
            let detail = detail.trim();
            if detail.is_empty() {
                format!("systemctl exited with {}", output.status)
            } else {
                format!("systemctl: {detail}")
            }
        }
        Ok(Err(error)) => format!("could not run systemctl: {error}"),
        Err(_) => "systemctl start timed out".to_owned(),
    }
}
