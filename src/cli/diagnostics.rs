use anyhow::bail;

use crate::{
    config::AppPaths,
    ipc::protocol::{DiagnosticsRequest, StatusRequest, request, response},
};

use super::{
    commands::OutputArgs,
    support::{daemon_request, daemon_response_error, unexpected_response},
    views::{StatusOutput, print_json},
};

pub(super) async fn status(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        1,
        request::Body::Status(StatusRequest {}),
        output.json,
    )
    .await?;
    match response.body {
        Some(response::Body::Status(status)) => {
            let value = StatusOutput {
                version: &status.version,
                hostname: &status.hostname,
                uptime_seconds: status.uptime_seconds,
                config_path: &status.config_path,
                netbird_address: &status.netbird_address,
                discovered_peers: status.discovered_peers,
            };
            if output.json {
                print_json(&value)?;
            } else {
                println!("clip-sync {} on {}", value.version, value.hostname);
                println!("uptime: {}s", value.uptime_seconds);
                println!("peers discovered: {}", value.discovered_peers);
                println!(
                    "NetBird address: {}",
                    value.netbird_address.as_deref().unwrap_or("unavailable")
                );
                println!("config: {}", value.config_path);
            }
            Ok(())
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, output.json)),
        _ => Err(unexpected_response(output.json, "status")),
    }
}

pub(super) async fn doctor(paths: &AppPaths, output: OutputArgs) -> anyhow::Result<()> {
    let response = daemon_request(
        paths,
        7,
        request::Body::Diagnostics(DiagnosticsRequest {}),
        output.json,
    )
    .await?;
    match response.body {
        Some(response::Body::Diagnostics(diagnostics)) => {
            let ok = diagnostics.checks.iter().all(|check| check.ok);
            if output.json {
                let checks = diagnostics
                    .checks
                    .iter()
                    .map(|check| {
                        serde_json::json!({
                            "name": check.name,
                            "ok": check.ok,
                            "detail": check.detail,
                        })
                    })
                    .collect::<Vec<_>>();
                print_json(&serde_json::json!({ "ok": ok, "checks": checks }))?;
            } else {
                for check in diagnostics.checks {
                    println!(
                        "{}: {} ({})",
                        check.name,
                        if check.ok { "ok" } else { "failed" },
                        check.detail
                    );
                }
            }
            if ok {
                Ok(())
            } else {
                bail!("one or more daemon checks failed")
            }
        }
        Some(response::Body::Error(error)) => Err(daemon_response_error(&error, output.json)),
        _ => Err(unexpected_response(output.json, "diagnostics")),
    }
}
