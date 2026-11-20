//! Read-only discovery and prompt-free inspection of already prepared ACP agents.
use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use awaken_acp_catalog::acp_cli;
use awaken_acp_contract::AcpHostObservation;
use awaken_run_executor_acp::host::{
    AcpHostDiscovery, HostAcpAdapters, diagnostic_summary, discover_selected, prepared_host_command,
};

use crate::acp_local_credentials::credential_state_name;

/// Discover local programs without installing adapters or sending prompts.
pub async fn local_acp_diagnostics(json: bool) -> String {
    let discovery = AcpHostDiscovery::local(Duration::from_secs(10));
    render_diagnostics(&discover_selected(&discovery, None).await, json)
}

/// `--probe` uses an existing adapter and an owned temporary cwd. The configured
/// runtime directory is read only; no login or inference request is performed.
pub struct AcpDiagnosticReport {
    pub output: String,
    pub ready: bool,
}

pub async fn local_acp_diagnostics_with_options(
    json: bool,
    probe: bool,
    wrapper_root: &Path,
    selected: Option<&[String]>,
) -> Result<AcpDiagnosticReport, String> {
    let selected = selected.map(|ids| ids.iter().cloned().collect::<BTreeSet<_>>());
    if let Some(ids) = &selected
        && let Some(id) = ids.iter().find(|id| acp_cli(id).is_none())
    {
        return Err(format!("unknown ACP CLI {id}"));
    }
    let adapters = HostAcpAdapters::production(Arc::new(
        awaken_protocol_acp::ProtocolAcpCapabilityHandshake,
    ));
    let observations = discover_selected(adapters.discovery.as_ref(), selected.as_ref()).await;
    let mut rows = diagnostic_rows(&observations);
    if probe {
        let directory = tempfile::Builder::new()
            .prefix("acp-probe-")
            .tempdir()
            .map_err(|error| format!("create ACP diagnostic cwd: {error}"))?;
        for (observation, row) in observations.iter().zip(&mut rows) {
            let cli = acp_cli(&observation.cli_id).expect("discovery uses the canonical catalog");
            if !observation.detected() {
                row["probe"] = serde_json::json!({"state":"not_run", "reason":"cli_unavailable"});
                continue;
            }
            let started = awaken_time::mono::Stopwatch::start();
            let result = match prepared_host_command(cli, wrapper_root) {
                Ok(Some(mut command)) => {
                    match awaken_run_executor_acp::host::inspect_host_runtime(cli, &command).await {
                        Ok(runtime) => {
                            row["runtime"] = serde_json::json!(runtime);
                            command.runtime = Some(runtime);
                        }
                        Err(error) => {
                            row["probe"] = serde_json::json!({"state":"failed", "error":diagnostic_summary(&error)});
                            continue;
                        }
                    }
                    row["execution_source"] = serde_json::json!(if command.env.is_empty()
                        && matches!(
                            cli.acquisition,
                            awaken_acp_catalog::AcpAcquisition::PinnedNpmWrapper { .. }
                        ) {
                        "adapter_managed"
                    } else {
                        "host"
                    });
                    adapters
                        .negotiator
                        .negotiate_command(&command, directory.path(), cli.auth_method_id)
                        .await
                }
                Ok(None) => {
                    row["probe"] = serde_json::json!({"state":"not_prepared", "reason":"adapter_not_installed"});
                    continue;
                }
                Err(error) => Err(error),
            };
            row["probe"] = match result {
                Ok(negotiated) => {
                    serde_json::json!({"state":"verified", "elapsed_ms":started.elapsed().as_millis(), "capabilities":negotiated})
                }
                Err(error) => {
                    serde_json::json!({"state":"failed", "elapsed_ms":started.elapsed().as_millis(), "error":diagnostic_summary(&error)})
                }
            };
        }
    }
    let ready = !probe || rows.iter().all(|row| row["probe"]["state"] == "verified");
    let output = if json && probe {
        serde_json::to_string_pretty(&serde_json::json!({
            "acp":rows, "probe_ready":ready, "inference_checked":false
        }))
        .expect("ACP diagnostics serialize")
    } else {
        render_rows(&rows, json)
    };
    Ok(AcpDiagnosticReport { output, ready })
}

fn diagnostic_rows(observations: &[AcpHostObservation]) -> Vec<serde_json::Value> {
    observations
        .iter()
        .filter_map(|observation| {
            let cli = acp_cli(&observation.cli_id)?;
            Some(serde_json::json!({
                "id":observation.cli_id, "name":observation.display_name,
                "supported":true, "detected":observation.detected(),
                "version":observation.version,
                "login_state":observation.credential_state.map(credential_state_name),
                "reason_code":observation.reason_code,
                "remediation":cli.remediation(observation.reason_code.as_deref()),
            }))
        })
        .collect()
}

pub(crate) fn render_diagnostics(observations: &[AcpHostObservation], json: bool) -> String {
    render_rows(&diagnostic_rows(observations), json)
}

fn render_rows(rows: &[serde_json::Value], json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(
            &serde_json::json!({"acp":rows, "inference_checked":false}),
        )
        .expect("ACP diagnostics serialize");
    }
    let mut output = String::from("Awaken ACP diagnostics\n\n");
    for row in rows {
        let status = if row["detected"] == true {
            row["login_state"].as_str().unwrap_or("probe_failed")
        } else {
            "not_detected"
        };
        output.push_str(&format!(
            "  {:<12} {:<16} {}\n",
            row["id"].as_str().unwrap_or("unknown"),
            status,
            row["version"].as_str().unwrap_or("-")
        ));
        if let Some(probe) = row.get("probe") {
            output.push_str(&format!(
                "    protocol: {}\n",
                probe["state"].as_str().unwrap_or("not_run")
            ));
            if let Some(error) = probe["error"].as_str() {
                output.push_str(&format!("    {error}\n"));
            }
        }
        if let Some(remediation) = row["remediation"].as_str() {
            output.push_str(&format!("    {remediation}\n"));
        }
    }
    output.push_str("\nInference has not been checked.\n");
    output
}
