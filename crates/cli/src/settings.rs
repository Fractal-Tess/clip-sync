use anyhow::{Context, bail};

use clip_sync_core::config::{AppPaths, Config};
use clip_sync_ipc::protocol::{
    ConfigRequest, PeerInterfacesUpdateRequest, SharedSettingKind, SharedSettingUpdateRequest,
    request, response,
};

use super::{
    commands::{ConfigCommand, ConfigSetting},
    support::{daemon_request, daemon_response_error, mutation_response, unexpected_response},
    views::{SafeConfig, print_json},
};

pub(super) async fn config_command(paths: &AppPaths, command: ConfigCommand) -> anyhow::Result<()> {
    match command {
        ConfigCommand::Show(output) => {
            let response = daemon_request(
                paths,
                6,
                request::Body::Config(ConfigRequest {}),
                output.json,
            )
            .await?;
            match response.body {
                Some(response::Body::Config(config)) => {
                    let config: SafeConfig = serde_json::from_slice(&config.redacted_json)
                        .context("decode daemon config response")?;
                    if output.json {
                        print_json(&config)?;
                    } else {
                        println!("{}", toml::to_string_pretty(&config)?);
                    }
                    Ok(())
                }
                Some(response::Body::Error(error)) => {
                    Err(daemon_response_error(&error, output.json))
                }
                _ => Err(unexpected_response(output.json, "config show")),
            }
        }
        ConfigCommand::Init { force } => {
            if paths.config.exists() && !force {
                bail!(
                    "{} already exists; pass --force to replace it",
                    paths.config.display()
                );
            }
            Config::default().save(&paths.config)?;
            println!("wrote {}", paths.config.display());
            Ok(())
        }
        ConfigCommand::Set {
            setting,
            value,
            json,
        } => {
            let response = daemon_request(
                paths,
                12,
                request::Body::SharedSettingUpdate(SharedSettingUpdateRequest {
                    setting: match setting {
                        ConfigSetting::MeshQuota => SharedSettingKind::MeshQuotaBytes,
                        ConfigSetting::CaptureThreshold => SharedSettingKind::CaptureThresholdBytes,
                    } as i32,
                    value,
                }),
                json,
            )
            .await?;
            mutation_response(response, json, "shared setting updated")
        }
        ConfigCommand::SetPeerInterfaces { interfaces, json } => {
            let response = daemon_request(
                paths,
                13,
                request::Body::PeerInterfacesUpdate(PeerInterfacesUpdateRequest { interfaces }),
                json,
            )
            .await?;
            mutation_response(response, json, "peer interfaces updated")
        }
    }
}
