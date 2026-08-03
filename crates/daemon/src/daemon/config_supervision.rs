use std::time::Duration;

use anyhow::Context;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use clip_sync_core::{
    clipboard::wayland::WaylandBackend,
    config::{Config, SharedConfig},
    model::SharedSetting,
    payload::ExplicitSharePolicy,
    storage::HistoryStore,
    transfer::TransferCoordinator,
};
use clip_sync_ipc::protocol::SharedSettingKind;

use crate::{ipc::DaemonState, mesh::MeshHandle};

use super::{runtime::unix_time_millis, views::history_items};

#[allow(clippy::too_many_arguments)]
pub(super) async fn update_shared_setting(
    setting: SharedSettingKind,
    value: u64,
    history: &mut HistoryStore,
    clipboard: &WaylandBackend,
    transfers: &mut TransferCoordinator,
    mesh: &MeshHandle,
    config_path: &std::path::Path,
    config: &mut Config,
) -> anyhow::Result<()> {
    anyhow::ensure!(value > 0, "shared setting value must be greater than zero");
    if setting == SharedSettingKind::CaptureThresholdBytes {
        anyhow::ensure!(
            value <= config.local.maximum_explicit_share_bytes,
            "capture threshold exceeds the local explicit-share hard limit"
        );
    }

    let now = unix_time_millis()?;
    let operations = match setting {
        SharedSettingKind::MeshQuotaBytes => history
            .set_mesh_quota_and_enforce(value, now)
            .context("persist mesh quota and deterministic evictions")?,
        SharedSettingKind::CaptureThresholdBytes => vec![
            history
                .set_shared_setting(SharedSetting::CaptureThresholdBytes, value, now)
                .context("persist shared capture threshold")?,
        ],
        SharedSettingKind::Unspecified => anyhow::bail!("shared setting is missing"),
    };
    for operation in &operations {
        mesh.record_local(operation)
            .await
            .context("publish shared setting operation")?;
    }

    let effective = history.projection().effective_shared_settings();
    clipboard
        .set_capture_threshold(effective.capture_threshold_bytes)
        .context("apply shared capture threshold")?;
    transfers
        .update_policy(ExplicitSharePolicy {
            automatic_capture_threshold_bytes: effective.capture_threshold_bytes,
            mesh_quota_bytes: effective.mesh_quota_bytes,
            maximum_explicit_share_bytes: config.local.maximum_explicit_share_bytes,
            free_space_reserve_bytes: config.local.transfer_free_space_reserve_bytes,
        })
        .context("apply shared transfer policy")?;
    let rewritten = Config::rewrite_shared(
        config_path,
        effective,
        history.projection().shared_settings_revision(),
    )
    .context("atomically mirror shared settings to config")?;
    config.shared = rewritten.shared;
    Ok(())
}

pub(super) fn initialize_shared_settings(
    history: &mut HistoryStore,
    config_path: &std::path::Path,
    config: &mut Config,
) -> anyhow::Result<()> {
    let projection = history.projection();
    let effective = projection.effective_shared_settings();
    let revision = projection.shared_settings_revision();
    let has_replicated_settings = projection
        .setting_event(SharedSetting::MeshQuotaBytes.key())
        .is_some()
        || projection
            .setting_event(SharedSetting::CaptureThresholdBytes.key())
            .is_some();
    let external_edit = if has_replicated_settings {
        !config.shared.revision.is_empty() && !config.shared.matches(effective, &revision)
    } else {
        config.shared.mesh_quota_bytes != effective.mesh_quota_bytes
            || config.shared.capture_threshold_bytes != effective.capture_threshold_bytes
    };

    if external_edit {
        let now = unix_time_millis()?;
        if config.shared.mesh_quota_bytes != effective.mesh_quota_bytes {
            history
                .set_mesh_quota_and_enforce(config.shared.mesh_quota_bytes, now)
                .context("apply configured shared mesh quota")?;
        }
        let current = history.projection().effective_shared_settings();
        if config.shared.capture_threshold_bytes != current.capture_threshold_bytes {
            history
                .set_shared_setting(
                    SharedSetting::CaptureThresholdBytes,
                    config.shared.capture_threshold_bytes,
                    now,
                )
                .context("apply configured shared capture threshold")?;
        }
    }

    let effective = history.projection().effective_shared_settings();
    let revision = history.projection().shared_settings_revision();
    if !config.shared.matches(effective, &revision) {
        config.shared = SharedConfig {
            mesh_quota_bytes: effective.mesh_quota_bytes,
            capture_threshold_bytes: effective.capture_threshold_bytes,
            revision,
        };
        config
            .save(config_path)
            .context("atomically save effective shared settings")?;
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "config reload updates every daemon-owned policy surface atomically"
)]
pub(super) async fn apply_config_reload(
    mut changed: Config,
    config_path: &std::path::Path,
    current: &mut Config,
    history: &mut HistoryStore,
    clipboard: &WaylandBackend,
    mesh: &MeshHandle,
    state: &DaemonState,
    transfers: &mut TransferCoordinator,
) -> anyhow::Result<()> {
    anyhow::ensure!(
        !restart_required_local_change(&current.local, &changed.local),
        "changed local bootstrap settings require a daemon restart"
    );
    let before = history.projection().effective_shared_settings();
    let current_revision = history.projection().shared_settings_revision();
    if changed.shared.matches(before, &current_revision) {
        transfers
            .update_policy(ExplicitSharePolicy {
                automatic_capture_threshold_bytes: before.capture_threshold_bytes,
                mesh_quota_bytes: before.mesh_quota_bytes,
                maximum_explicit_share_bytes: changed.local.maximum_explicit_share_bytes,
                free_space_reserve_bytes: changed.local.transfer_free_space_reserve_bytes,
            })
            .context("apply reloaded explicit-share policy")?;
        *current = changed;
        state.set_config(current.clone()).await;
        return Ok(());
    }

    let now = unix_time_millis()?;
    let mut authored = Vec::new();
    if changed.shared.mesh_quota_bytes != before.mesh_quota_bytes {
        authored.extend(
            history
                .set_mesh_quota_and_enforce(changed.shared.mesh_quota_bytes, now)
                .context("apply reloaded shared mesh quota")?,
        );
    }
    let effective = history.projection().effective_shared_settings();
    if changed.shared.capture_threshold_bytes != effective.capture_threshold_bytes {
        authored.push(
            history
                .set_shared_setting(
                    SharedSetting::CaptureThresholdBytes,
                    changed.shared.capture_threshold_bytes,
                    now,
                )
                .context("apply reloaded shared capture threshold")?,
        );
    }
    for operation in &authored {
        mesh.record_local(operation)
            .await
            .context("publish config-authored shared setting")?;
    }

    let effective = history.projection().effective_shared_settings();
    clipboard
        .set_capture_threshold(effective.capture_threshold_bytes)
        .context("apply config-authored capture threshold")?;
    transfers
        .update_policy(ExplicitSharePolicy {
            automatic_capture_threshold_bytes: effective.capture_threshold_bytes,
            mesh_quota_bytes: effective.mesh_quota_bytes,
            maximum_explicit_share_bytes: changed.local.maximum_explicit_share_bytes,
            free_space_reserve_bytes: changed.local.transfer_free_space_reserve_bytes,
        })
        .context("apply config-authored explicit-share policy")?;
    changed.shared = SharedConfig {
        mesh_quota_bytes: effective.mesh_quota_bytes,
        capture_threshold_bytes: effective.capture_threshold_bytes,
        revision: history.projection().shared_settings_revision(),
    };
    changed
        .save(config_path)
        .context("atomically save config-authored shared settings")?;
    *current = changed;
    state.set_config(current.clone()).await;
    state.set_history(history_items(history.replica())).await;
    Ok(())
}

pub(super) fn restart_required_local_change(
    current: &clip_sync_core::config::LocalConfig,
    changed: &clip_sync_core::config::LocalConfig,
) -> bool {
    current.mesh_key_file != changed.mesh_key_file
        || current.listen_port != changed.listen_port
        || current.discovery_interval_seconds != changed.discovery_interval_seconds
        || current.reconcile_interval_seconds != changed.reconcile_interval_seconds
        || current.reconnect_min_seconds != changed.reconnect_min_seconds
        || current.reconnect_max_seconds != changed.reconnect_max_seconds
        || current.materialization_free_space_reserve_bytes
            != changed.materialization_free_space_reserve_bytes
        || current.max_concurrent_chunk_streams != changed.max_concurrent_chunk_streams
}

pub(super) fn spawn_config_watch(
    path: std::path::PathBuf,
    initial: Config,
    updates: tokio::sync::mpsc::UnboundedSender<Config>,
    shutdown: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut observed = initial;
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
            match Config::load(&path) {
                Ok(config) if config != observed => {
                    let reload = config_change_requires_reload(&observed, &config);
                    observed = config.clone();
                    if reload && updates.send(config).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::debug!(%error, "waiting for config to become valid");
                }
            }
        }
    })
}

pub(super) fn config_change_requires_reload(observed: &Config, changed: &Config) -> bool {
    if observed.local != changed.local {
        return true;
    }
    if observed.shared == changed.shared {
        return false;
    }

    // Daemon-authored shared-setting mirrors always advance the replicated
    // register fingerprint. A human edit changes values while retaining (or
    // clearing) the last fingerprint, so it still becomes a mesh operation.
    changed.shared.revision.is_empty() || changed.shared.revision == observed.shared.revision
}
