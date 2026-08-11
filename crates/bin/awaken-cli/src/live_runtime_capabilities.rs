use std::sync::Arc;

/// Project the one executable ACP catalog into the management read model. This
/// startup edge is intentionally the only place that knows both contexts;
/// neither Control nor the executor keeps a synchronized adapter list.
fn runtime_capabilities(
    observations: &[awaken_acp_application::AcpHostObservation],
) -> Vec<awaken_control::RuntimeCapability> {
    std::iter::once(awaken_control::RuntimeCapability::native())
        .chain(awaken_run_executor_acp::known_acp_clis().iter().map(|cli| {
            let capability =
                awaken_control::RuntimeCapability::acp(cli.id, cli.display_name, cli.description);
            let Some(observation) = observations.iter().find(|row| row.cli_id == cli.id) else {
                return capability;
            };
            capability.with_local(awaken_control::LocalRuntimeCapability {
                detected: observation.detected(),
                version: observation.version.clone(),
                login_state: observation.credential_state.map(credential_state_name),
                reason_code: observation.reason_code.clone(),
                remediation: cli
                    .remediation(observation.reason_code.as_deref())
                    .map(str::to_string),
                negotiated: None,
            })
        }))
        .collect()
}

fn credential_state_name(state: awaken_runtime_contract::CredentialObservationState) -> String {
    serde_json::to_value(state)
        .expect("credential observation state serializes")
        .as_str()
        .expect("credential observation state serializes as a string")
        .to_string()
}

pub(crate) struct LiveRuntimeCapabilities {
    pub(crate) initial: Vec<awaken_acp_application::AcpHostObservation>,
    pub(crate) workers: Arc<dyn awaken_coordinator::WorkerObservationSource>,
    pub(crate) credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    pub(crate) workspace: String,
}

#[async_trait::async_trait]
impl awaken_control::RuntimeCapabilitySource for LiveRuntimeCapabilities {
    async fn current(&self) -> Vec<awaken_control::RuntimeCapability> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let workers = self.workers.list().await.unwrap_or_default();
        let credentials = self
            .credentials
            .list(&self.workspace)
            .await
            .unwrap_or_default();
        runtime_capabilities(&self.initial)
            .into_iter()
            .map(|mut capability| {
                let Some(cli_id) = capability.cli.as_deref() else {
                    return capability;
                };
                let cli = awaken_run_executor_acp::acp_cli(cli_id)
                    .expect("runtime capability came from the ACP catalog");
                let backend_ref = capability.id.clone();
                let source = credentials.iter().find(|source| {
                    source
                        .worker_local_binding
                        .as_ref()
                        .is_some_and(|binding| binding.driver_id == backend_ref)
                });
                let live_worker = workers.iter().find(|worker| {
                    worker.snapshot.expires_at_ms > now
                        && worker.snapshot.manifest.capabilities.contains(&backend_ref)
                });
                let live_capability = live_worker.and_then(|worker| {
                    worker
                        .snapshot
                        .acp_capability_observations
                        .iter()
                        .find(|row| {
                            row.observation.backend_ref == backend_ref
                                && row.observation.observed_at_ms <= now
                                && now < row.valid_until_ms
                        })
                });
                let live_credential = source.and_then(|source| {
                    live_worker.and_then(|worker| {
                        worker.snapshot.credential_observations.iter().find(|row| {
                            row.credential.id == source.id.0
                                && i64::try_from(row.credential.revision).ok()
                                    == Some(source.version)
                                && row.observed_at_ms <= now
                                && now < row.valid_until_ms
                        })
                    })
                });
                if live_worker.is_some() {
                    let initial_version = capability
                        .local
                        .as_ref()
                        .and_then(|local| local.version.clone());
                    capability = capability.with_local(awaken_control::LocalRuntimeCapability {
                        detected: true,
                        version: live_capability
                            .map(|row| row.observation.adapter_version.clone())
                            .or(initial_version),
                        login_state: live_credential
                            .and_then(|row| serde_json::to_value(row.state).ok())
                            .and_then(|value| value.as_str().map(str::to_string)),
                        reason_code: live_credential
                            .and_then(|row| row.reason_code.clone())
                            .or_else(|| {
                                live_capability.and_then(|row| row.observation.reason_code.clone())
                            }),
                        remediation: live_credential
                            .and_then(|row| row.reason_code.as_deref())
                            .and_then(|reason| cli.remediation(Some(reason)))
                            .map(str::to_string),
                        negotiated: live_capability
                            .and_then(|row| row.observation.negotiated.as_ref())
                            .and_then(|negotiated| serde_json::to_value(negotiated).ok()),
                    });
                }
                capability
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn management_runtime_projection_is_exactly_the_executable_catalog() {
        // Cause graph:
        // C1 native runtime is intrinsic -> E1 exactly one `awaken` row.
        // C2 an AcpCli catalog row exists -> E2 exactly one matching `acp:<id>` row.
        // C3 no AcpCli row exists -> E3 no management capability can advertise it.
        //
        // Decision table:
        // | Rule | Native | catalog row | capability |
        // | R1 | T | - | awaken exactly once |
        // | R2 | - | T | matching acp:<id> exactly once |
        // | R3 | - | F | absent |
        let projected = runtime_capabilities(&[]);
        assert_eq!(
            projected.iter().filter(|row| row.id == "awaken").count(),
            1,
            "R1"
        );

        let catalog = awaken_run_executor_acp::known_acp_clis();
        let projected_acp: Vec<_> = projected.iter().filter(|row| row.kind == "acp").collect();
        assert_eq!(projected_acp.len(), catalog.len(), "R2/R3 cardinality");
        for cli in catalog {
            let row = projected_acp
                .iter()
                .find(|row| row.cli.as_deref() == Some(cli.id))
                .unwrap_or_else(|| panic!("R2 missing catalog projection for {}", cli.id));
            assert_eq!(row.id, format!("acp:{}", cli.id), "R2");
            assert_eq!(row.label, cli.display_name, "R2 metadata");
            assert_eq!(row.description, cli.description, "R2 metadata");
        }
    }

    #[test]
    fn management_runtime_projection_joins_one_secret_free_local_observation() {
        // Cause graph: catalog row + same-id startup observation -> enriched
        // read model. Rows without an observation stay supported with unknown
        // local status; no joined inventory is persisted.
        //
        // Decision table:
        // L1 same id + detected/login-required -> detected status + remediation
        // L2 no observation                    -> local status absent
        let projected = runtime_capabilities(&[awaken_acp_application::AcpHostObservation {
            cli_id: "codex".into(),
            display_name: "Codex".into(),
            detection: awaken_acp_application::AcpDetectionState::Detected,
            version: Some("codex 1".into()),
            credential_state: Some(
                awaken_runtime_contract::CredentialObservationState::LoginRequired,
            ),
            reason_code: Some("acp_login_required".into()),
            capability_state: None,
            capability_fingerprint: None,
            capability_reason_code: None,
        }]);
        let codex = projected.iter().find(|row| row.id == "acp:codex").unwrap();
        let local = codex.local.as_ref().expect("L1");
        assert!(local.detected, "L1");
        assert_eq!(local.login_state.as_deref(), Some("login_required"), "L1");
        assert!(
            local
                .remediation
                .as_deref()
                .unwrap()
                .contains("codex login")
        );
        assert!(
            projected
                .iter()
                .find(|row| row.id == "acp:gemini")
                .unwrap()
                .local
                .is_none(),
            "L2"
        );
    }
}
