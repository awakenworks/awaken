use std::sync::Arc;

pub(crate) struct LiveRuntimeCapabilities {
    pub(crate) initial: Vec<awaken_acp_application::AcpHostObservation>,
    pub(crate) workers: awaken_server::WorkerDirectoryHandle,
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
        super::runtime_capabilities(&self.initial)
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
