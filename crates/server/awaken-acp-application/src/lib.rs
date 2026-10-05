// @formal entrypoint worker_credential.host_observation_projection_is_backend_scoped
// @formal entrypoint worker_credential.local_binding_batch_is_atomic
//! Coordinator ACP registration. Host mechanisms live in the Worker ACP adapter.
use awaken_credential_vault::repo::{CredentialRepo, ensure_worker_local};
use awaken_credential_vault::{
    CredentialKind, CredentialSource, CredentialStatus, WorkerLocalBinding,
};
use awaken_run_executor_acp::acp_cli;
pub use awaken_run_executor_acp::host::*;
use awaken_runtime_contract::CredentialRef;
use awaken_runtime_contract::resolved::Backend;
use awaken_time::WallClock;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

/// Coordinator-owned registration inputs; the Worker preparation has no repository.
pub struct LocalAcpPreparation {
    pub probe_cwd: PathBuf,
    pub initial_workspace: Option<String>,
    pub wrapper_root: PathBuf,
    pub selected_cli_ids: Option<BTreeSet<String>>,
    pub credentials: Arc<dyn CredentialRepo>,
    pub clock: Arc<dyn WallClock>,
}

/// Registration facade over one Worker-owned preparation result, without copied state.
pub struct PreparedAcpCapabilities {
    pub host: PreparedHostAcpCapabilities,
}
impl std::ops::Deref for PreparedAcpCapabilities {
    type Target = PreparedHostAcpCapabilities;
    fn deref(&self) -> &Self::Target {
        &self.host
    }
}
impl PreparedAcpCapabilities {
    pub async fn bind_workspace(
        &self,
        credentials: &dyn CredentialRepo,
        workspace: &str,
    ) -> Result<(), String> {
        ensure_workspace_bindings(credentials, workspace, self.selected_cli_ids()).await?;
        let sources = credentials
            .list(workspace)
            .await
            .map_err(|error| format!("list local ACP bindings: {error}"))?;
        add_sources(&self.resolver, sources)
    }
}

pub async fn prepare_host_acp_with(
    input: LocalAcpPreparation,
    discovery: Arc<dyn AcpDiscovery>,
    installer: Arc<dyn AcpWrapperInstaller>,
    negotiator: Arc<dyn AcpCapabilityNegotiator>,
) -> Result<PreparedAcpCapabilities, String> {
    let host = prepare_host_acp_capabilities_with(
        HostAcpPreparation {
            probe_cwd: input.probe_cwd,
            wrapper_root: input.wrapper_root,
            selected_cli_ids: input.selected_cli_ids,
            clock: input.clock,
        },
        discovery,
        installer,
        negotiator,
    )
    .await?;
    let prepared = PreparedAcpCapabilities { host };
    if let Some(workspace) = input.initial_workspace {
        prepared
            .bind_workspace(input.credentials.as_ref(), &workspace)
            .await?;
    }
    Ok(prepared)
}

/// Idempotently create the non-secret WorkerLocal identities requested by one
/// execution Workspace. Discovery is deliberately not required here: the
/// control plane records intent, while Worker observations independently prove
/// whether a matching host identity is currently usable.
pub async fn ensure_workspace_bindings<'a>(
    credentials: &dyn CredentialRepo,
    workspace: &str,
    cli_ids: impl IntoIterator<Item = &'a str>,
) -> Result<(), String> {
    for cli_id in cli_ids {
        let cli = acp_cli(cli_id)
            .ok_or_else(|| format!("local ACP binding names unknown CLI `{cli_id}`"))?;
        ensure_worker_local(
            credentials,
            workspace,
            WorkerLocalBinding::new(format!("acp:{}", cli.id), "default"),
            None,
        )
        .await
        .map_err(|error| format!("register local ACP binding for {}: {error}", cli.id))?;
    }
    Ok(())
}

/// Project Control source metadata once; host adapters only accept these exact bindings.
pub fn project_local_credential_sources(
    sources: impl IntoIterator<Item = CredentialSource>,
) -> Result<Vec<AcpLocalCredentialBinding>, String> {
    let mut projected = Vec::new();
    for source in sources {
        if source.kind != CredentialKind::WorkerLocal {
            continue;
        }
        let binding = source
            .worker_local_binding
            .as_ref()
            .ok_or("WorkerLocal credential requires a binding")?;
        let cli_id = match Backend::from_ref(&binding.driver_id) {
            Backend::Acp(backend) => backend.cli().to_string(),
            Backend::Invalid(_) => return Err("invalid WorkerLocal driver".into()),
            _ => continue,
        };
        if binding.subject_id != "default" {
            return Err("host ACP supports only the default local subject".into());
        }
        if source.material_ref.is_some()
            || !source.auxiliary_material_refs.is_empty()
            || source.oauth_command.is_some()
            || source.env_key.is_some()
        {
            return Err("WorkerLocal ACP binding must not contain material locators".into());
        }
        let projection = AcpLocalCredentialBinding {
            credential: CredentialRef {
                id: source.id.0,
                revision: u64::try_from(source.version)
                    .map_err(|_| "WorkerLocal revision must be positive")?,
            },
            cli_id,
            enabled: source.status == CredentialStatus::Active,
        };
        projection.validate()?;
        projected.push(projection);
    }
    Ok(projected)
}

/// Adapt stored metadata at the Coordinator boundary, never in a Worker.
pub fn resolver_from_sources(
    discovery: Arc<dyn AcpDiscovery>,
    sources: impl IntoIterator<Item = CredentialSource>,
    clock: Arc<dyn WallClock>,
) -> Result<AcpLocalCredentialResolver, String> {
    AcpLocalCredentialResolver::from_bindings(
        discovery,
        project_local_credential_sources(sources)?,
        clock,
    )
}
pub fn add_sources(
    resolver: &AcpLocalCredentialResolver,
    sources: impl IntoIterator<Item = CredentialSource>,
) -> Result<(), String> {
    resolver.add_bindings(project_local_credential_sources(sources)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use awaken_acp_contract::AcpCapabilityObservationSource;
    use awaken_credential_vault::repo::InMemoryCredentialRepo;
    use awaken_run_executor_acp::AcpCli;
    use awaken_runtime_contract::{
        CredentialObservationSource, CredentialObservationState, WorkerLocalReferenceRevalidator,
    };
    use std::collections::BTreeMap;
    use std::path::Path;

    #[tokio::test]
    async fn control_projection_rejects_unsupported_subjects_and_material_locators() {
        // P1 active/disabled/archived default-account metadata maps to exactly
        // one enabled bit and no material fields. P2 non-default subject,
        // material/environment/helper locators or malformed driver/revision
        // reject the complete projection; no host login can stand in for a
        // different authored account. P3 unrelated non-ACP sources are ignored.
        let repo = InMemoryCredentialRepo::new();
        let source = ensure_worker_local(
            &repo,
            "workspace",
            WorkerLocalBinding::new("acp:codex", "default"),
            None,
        )
        .await
        .unwrap();
        for status in [
            CredentialStatus::Active,
            CredentialStatus::Disabled,
            CredentialStatus::Archived,
        ] {
            let mut row = source.clone();
            row.status = status;
            let result = project_local_credential_sources([row]).unwrap();
            assert_eq!(result.len(), 1);
            assert_eq!(result[0].enabled, status == CredentialStatus::Active);
            assert_eq!(result[0].cli_id, "codex");
            assert_eq!(result[0].credential.id, source.id.0);
        }
        for fault in [
            "subject",
            "env",
            "helper",
            "driver",
            "material",
            "auxiliary",
        ] {
            let mut row = source.clone();
            match fault {
                "subject" => {
                    row.worker_local_binding.as_mut().unwrap().subject_id = "secondary".into()
                }
                "env" => row.env_key = Some("FIXTURE_KEY".into()),
                "helper" => row.oauth_command = Some(vec!["must-not-run".into()]),
                "driver" => row.worker_local_binding.as_mut().unwrap().driver_id = "acp:".into(),
                "material" => {
                    row.material_ref = Some(awaken_credential_vault::SecretRef("fixture".into()))
                }
                "auxiliary" => {
                    row.auxiliary_material_refs.insert(
                        "fixture".into(),
                        awaken_credential_vault::SecretRef("fixture".into()),
                    );
                }
                _ => unreachable!(),
            }
            assert!(
                project_local_credential_sources([source.clone(), row]).is_err(),
                "P2 {fault}"
            );
        }
        let mut unrelated = source;
        unrelated.worker_local_binding = Some(WorkerLocalBinding::new("extension", "other"));
        assert!(
            project_local_credential_sources([unrelated])
                .unwrap()
                .is_empty(),
            "P3"
        );
    }

    struct AvailableDiscovery;

    fn observation(
        id: &str,
        detection: AcpDetectionState,
        credential: Option<CredentialObservationState>,
        capability: Option<AcpCapabilityState>,
    ) -> AcpHostObservation {
        AcpHostObservation {
            cli_id: id.into(),
            display_name: id.into(),
            detection,
            version: None,
            credential_state: credential,
            reason_code: None,
            capability_state: capability,
            capability_fingerprint: None,
            capability_reason_code: None,
        }
    }

    #[async_trait]
    impl AcpDiscovery for AvailableDiscovery {
        async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
            AcpHostObservation {
                cli_id: cli.id.into(),
                display_name: cli.display_name.into(),
                detection: AcpDetectionState::Detected,
                version: Some("test".into()),
                credential_state: Some(CredentialObservationState::Available),
                reason_code: None,
                capability_state: None,
                capability_fingerprint: None,
                capability_reason_code: None,
            }
        }
    }

    struct CapabilityDiscovery {
        state: CredentialObservationState,
        detected: std::sync::atomic::AtomicBool,
    }

    #[async_trait]
    impl AcpDiscovery for CapabilityDiscovery {
        async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
            observation(
                cli.id,
                if cli.id == "claude" && self.detected.load(std::sync::atomic::Ordering::SeqCst) {
                    AcpDetectionState::Detected
                } else {
                    AcpDetectionState::Missing
                },
                Some(self.state),
                None,
            )
        }
    }

    struct CapabilityFixture {
        success: bool,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait]
    impl AcpWrapperInstaller for CapabilityFixture {
        async fn resolved_argv(&self, _: &AcpCli, _: &Path) -> Result<Option<Vec<String>>, String> {
            Ok(Some(vec!["fixture-acp".into()]))
        }
    }

    #[async_trait]
    impl AcpCapabilityNegotiator for CapabilityFixture {
        async fn negotiate(
            &self,
            _: &[String],
            _: &Path,
            _: Option<&str>,
        ) -> Result<awaken_acp_contract::NegotiatedAcpCapabilities, String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if !self.success {
                return Err("fixture negotiation failed".into());
            }
            Ok(awaken_acp_contract::NegotiatedAcpCapabilities {
                protocol_version: "1".into(),
                load_session: true,
                prompt_image: false,
                prompt_audio: false,
                prompt_embedded_context: true,
                mcp_http: true,
                mcp_sse: false,
                session_list: false,
                modes: vec![],
                config_options: vec![],
            })
        }
    }

    #[tokio::test]
    async fn an_unknown_capability_route_fails_before_negotiation() {
        // R1 a custom composition supplies a route outside the canonical CLI
        // catalog => typed source failure, no protocol call and no advertisement.
        // Normal profiles are validated upstream, but this public port must not
        // panic or silently accept an unvalidated adapter identity.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let fixture = Arc::new(CapabilityFixture {
            success: true,
            calls: AtomicUsize::new(0),
        });
        let resolver = resolver_from_sources(
            Arc::new(AvailableDiscovery),
            [],
            awaken_time::system_clock(),
        )
        .unwrap()
        .with_capability_observations(
            fixture.clone(),
            [("unknown".into(), vec!["must-not-launch".into()])]
                .into_iter()
                .collect(),
            PathBuf::from("."),
        );
        assert!(
            resolver
                .capability_observations()
                .await
                .unwrap_err()
                .contains("unknown CLI")
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn disabling_the_last_binding_cannot_reuse_stale_installation_evidence() {
        // R1 active + installed -> Available credential and Verified protocol;
        // R2 disable the last binding AND remove the CLI before refresh ->
        // Disabled credential + Unavailable protocol, with no new negotiation;
        // R3 reinstall without re-enabling -> still Disabled but Verified protocol.
        // This is the production credential-then-capability sequence, not manual
        // cache clearing. It detects stale probes surviving a status transition.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let repo = InMemoryCredentialRepo::new();
        let mut source = ensure_worker_local(
            &repo,
            "workspace",
            WorkerLocalBinding::new("acp:claude", "default"),
            None,
        )
        .await
        .unwrap();
        let discovery = Arc::new(CapabilityDiscovery {
            state: CredentialObservationState::Available,
            detected: AtomicBool::new(true),
        });
        let fixture = Arc::new(CapabilityFixture {
            success: true,
            calls: AtomicUsize::new(0),
        });
        let resolver = resolver_from_sources(
            discovery.clone(),
            [source.clone()],
            awaken_time::system_clock(),
        )
        .unwrap()
        .with_capability_observations(
            fixture.clone(),
            [("claude".into(), vec!["claude-acp".into()])]
                .into_iter()
                .collect(),
            PathBuf::from("."),
        );
        assert_eq!(
            observed_bindings(&resolver).await.values().next(),
            Some(&CredentialObservationState::Available)
        );
        assert!(
            resolver.capability_observations().await.unwrap()[0]
                .negotiated()
                .is_some()
        );
        source.status = CredentialStatus::Disabled;
        add_sources(&resolver, [source]).unwrap();
        for installed in [false, true] {
            discovery.detected.store(installed, Ordering::SeqCst);
            assert_eq!(
                observed_bindings(&resolver).await.values().next(),
                Some(&CredentialObservationState::Disabled)
            );
            assert_eq!(
                resolver.capability_observations().await.unwrap()[0]
                    .negotiated()
                    .is_some(),
                installed
            );
            assert_eq!(
                fixture.calls.load(Ordering::SeqCst),
                if installed { 2 } else { 1 }
            );
        }
    }

    #[tokio::test]
    async fn capability_routes_follow_the_selected_profile_without_local_credentials() {
        // C1 installed host CLIs exceed the authored allowlist; C2 no host-login
        // bindings exist; C3 allowlist is empty/one/two CLIs. R1 only the selected
        // routes are acquired, negotiated, and refreshed; R2 C2 never erases
        // protocol evidence or manufactures credential evidence; R3 empty means
        // none, not automatic discovery. This catches the previously masked
        // detected-but-unselected routes when capabilities followed bindings.
        use std::sync::atomic::{AtomicUsize, Ordering};
        for cli_ids in [vec![], vec!["codex"], vec!["claude", "codex"]] {
            let directory = tempfile::tempdir().unwrap();
            let fixture = Arc::new(CapabilityFixture {
                success: true,
                calls: AtomicUsize::new(0),
            });
            let prepared = prepare_host_acp_with(
                LocalAcpPreparation {
                    probe_cwd: directory.path().join("probe"),
                    wrapper_root: directory.path().join("wrappers"),
                    initial_workspace: None,
                    selected_cli_ids: Some(cli_ids.iter().map(|id| (*id).to_string()).collect()),
                    credentials: Arc::new(InMemoryCredentialRepo::new()),
                    clock: awaken_time::system_clock(),
                },
                Arc::new(AvailableDiscovery),
                fixture.clone(),
                fixture.clone(),
            )
            .await
            .unwrap();
            assert_eq!(prepared.routable_cli_ids(), cli_ids);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), cli_ids.len());
            assert!(
                prepared
                    .resolver
                    .credential_observations()
                    .await
                    .unwrap()
                    .is_empty()
            );
            let capabilities = prepared.resolver.capability_observations().await.unwrap();
            assert_eq!(
                capabilities
                    .iter()
                    .map(|row| row.backend_ref().to_string())
                    .collect::<Vec<_>>(),
                cli_ids
                    .iter()
                    .map(|id| format!("acp:{id}"))
                    .collect::<Vec<_>>()
            );
            assert_eq!(fixture.calls.load(Ordering::SeqCst), cli_ids.len() * 2);
        }
    }

    #[tokio::test]
    async fn workspace_credentials_share_one_probe_and_capability_per_host_backend() {
        // Causes: C1 zero/three bindings; C2 each binding Active/Disabled;
        // C3 two bindings share Codex across Workspaces, one uses Claude;
        // C4 negotiation succeeds/fails; C5 another heartbeat refresh occurs.
        // Decision table: every C2 combination (2^3) crosses C4, plus C1=zero.
        // R1 retain every exact credential ref and its independent state.
        // R2 active refs sharing a CLI -> ONE login probe per refresh, never
        //    one per Workspace. A disabled ref must not borrow Available.
        // R3 each configured backend -> ONE capability row/negotiation even
        //    with zero active bindings (Provider-owned ACP needs no host login).
        // R4 failed negotiation -> one failed row, not an earlier verified row.
        // R5 another refresh -> fresh login evidence; no permanent probe cache.
        // C6 Codex's adapter replies with Claude's identity; R6 neither login
        // nor capability may borrow that backend evidence, including the zero-
        // active-binding fallback. Cross C6 with every existing rule. Flip C6
        // on refresh 2 to cover both recovery and loss after a verified batch;
        // Claude remains independent. This expands this matrix's authority.
        // This is per-batch de-duplication, not cross-request coalescing or an
        // exactly-once external-probe claim. WorkerObservationCache remains the
        // sole serialized publication/expiry boundary. AcpObservationRefresh.tla
        // covers the two-reference/two-refresh abstraction of this projection.
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingDiscovery {
            calls: AtomicUsize,
            wrong_backend: std::sync::atomic::AtomicBool,
        }
        #[async_trait]
        impl AcpDiscovery for CountingDiscovery {
            async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let mut observed = AvailableDiscovery.discover(cli).await;
                if cli.id == "codex" && self.wrong_backend.load(Ordering::SeqCst) {
                    observed.cli_id = "claude".into();
                }
                observed
            }
        }
        let repo = InMemoryCredentialRepo::new();
        let mut sources = Vec::new();
        for (workspace, cli) in [("a", "codex"), ("b", "codex"), ("c", "claude")] {
            sources.push(
                ensure_worker_local(
                    &repo,
                    workspace,
                    WorkerLocalBinding::new(format!("acp:{cli}"), "default"),
                    None,
                )
                .await
                .unwrap(),
            );
        }
        for initially_wrong in [false, true] {
            for mask in 0..=8 {
                for success in [true, false] {
                    let discovery = Arc::new(CountingDiscovery {
                        calls: AtomicUsize::new(0),
                        wrong_backend: std::sync::atomic::AtomicBool::new(initially_wrong),
                    });
                    let negotiator = Arc::new(CapabilityFixture {
                        success,
                        calls: AtomicUsize::new(0),
                    });
                    let selected = if mask == 8 {
                        Vec::new()
                    } else {
                        sources
                            .iter()
                            .enumerate()
                            .map(|(index, source)| {
                                let mut source = source.clone();
                                if mask & (1 << index) == 0 {
                                    source.status = CredentialStatus::Disabled;
                                }
                                source
                            })
                            .collect()
                    };
                    let resolver = resolver_from_sources(
                        discovery.clone(),
                        selected.clone(),
                        awaken_time::system_clock(),
                    )
                    .unwrap()
                    .with_capability_observations(
                        negotiator.clone(),
                        [
                            ("codex".into(), vec!["codex-acp".into()]),
                            ("claude".into(), vec!["claude-acp".into()]),
                        ]
                        .into_iter()
                        .collect(),
                        PathBuf::from("."),
                    );
                    let mut expected_negotiations = 0;
                    for refresh in 1..=2 {
                        let wrong_backend = initially_wrong == (refresh == 1);
                        discovery
                            .wrong_backend
                            .store(wrong_backend, Ordering::SeqCst);
                        let credentials = observed_bindings(&resolver).await;
                        assert_eq!(credentials.len(), selected.len(), "R1 mask={mask}");
                        for source in &selected {
                            assert_eq!(
                                credentials[&CredentialRef {
                                    id: source.id.0.clone(),
                                    revision: 1
                                }],
                                if source.status == CredentialStatus::Active {
                                    if wrong_backend
                                        && source.worker_local_binding.as_ref().unwrap().driver_id
                                            == "acp:codex"
                                    {
                                        CredentialObservationState::ProbeFailed
                                    } else {
                                        CredentialObservationState::Available
                                    }
                                } else {
                                    CredentialObservationState::Disabled
                                },
                                "R1/R2/R6 mask={mask} wrong_backend={wrong_backend}"
                            );
                        }
                        let capabilities = resolver.capability_observations().await.unwrap();
                        assert_eq!(
                            capabilities
                                .iter()
                                .map(|row| row.backend_ref())
                                .collect::<Vec<_>>(),
                            ["acp:claude", "acp:codex"],
                            "R3 mask={mask}"
                        );
                        assert!(
                            capabilities.iter().all(|row| row.negotiated().is_some()
                                == (success
                                    && (!wrong_backend || row.backend_ref() != "acp:codex"))),
                            "R4/R6 capability evidence is backend scoped"
                        );
                        expected_negotiations += if wrong_backend { 1 } else { 2 };
                        assert_eq!(
                            negotiator.calls.load(Ordering::SeqCst),
                            expected_negotiations,
                            "R3/R5 mask={mask}"
                        );
                        assert_eq!(
                            discovery.calls.load(Ordering::SeqCst),
                            refresh * 2,
                            "R2/R5 mask={mask}"
                        );
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn protocol_capability_does_not_grant_backend_owned_login() {
        // Graph: detected CLI + active registration -> prompt-free negotiation;
        // independent login observation -> BackendOwned credential admission.
        // | rule | login state | negotiation | protocol | credential |
        // | C1 | Available | succeeds | verified | available |
        // | C2 | any other | succeeds | verified | original denial |
        // | C3 | any | fails | unverified | unchanged |
        // | C4 | CLI disappears | not attempted | unavailable | no grant |
        // | C5 | source disabled | still probed | unchanged | disabled |
        // Cross C1-C3 over all six credential states at startup AND heartbeat.
        // Every row supplies the exact observed credential reference; the
        // contract-owned admission test separately covers identity mismatch.
        // C4 keeps installation detection; C5 keeps exact credential revocation
        // without revoking the independent Provider-owned protocol route.
        // No Provider key is present in any negotiation.
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        for state in [
            CredentialObservationState::Available,
            CredentialObservationState::LoginRequired,
            CredentialObservationState::Expired,
            CredentialObservationState::Invalid,
            CredentialObservationState::ProbeFailed,
            CredentialObservationState::Disabled,
        ] {
            for success in [true, false] {
                let directory = tempfile::tempdir().unwrap();
                let discovery = Arc::new(CapabilityDiscovery {
                    state,
                    detected: AtomicBool::new(true),
                });
                let fixture = Arc::new(CapabilityFixture {
                    success,
                    calls: AtomicUsize::new(0),
                });
                let prepared = prepare_host_acp_with(
                    LocalAcpPreparation {
                        probe_cwd: directory.path().join("probe"),
                        wrapper_root: directory.path().join("wrappers"),
                        initial_workspace: Some("workspace".into()),
                        selected_cli_ids: Some(["claude".into()].into_iter().collect()),
                        credentials: Arc::new(InMemoryCredentialRepo::new()),
                        clock: awaken_time::system_clock(),
                    },
                    discovery.clone(),
                    fixture.clone(),
                    fixture.clone(),
                )
                .await
                .unwrap();
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
                assert_eq!(prepared.effective_profiles.contains_key("claude"), success);
                let credentials = prepared.resolver.credential_observations().await.unwrap();
                assert_eq!(credentials.len(), 1);
                let credential = credentials.into_iter().next().unwrap();
                assert_eq!(credential.state, state);
                // C1-C3 exercise the production use-time revalidator, not a
                // second state-only helper. The neutral contract separately
                // owns the id/revision mismatch decision table.
                assert_eq!(
                    prepared
                        .resolver
                        .revalidate_worker_reference(&credential.credential)
                        .await
                        .is_ok(),
                    state == CredentialObservationState::Available
                );
                let capabilities = prepared.resolver.capability_observations().await.unwrap();
                assert_eq!(capabilities.len(), 1);
                assert_eq!(capabilities[0].negotiated().is_some(), success);
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
                discovery.detected.store(false, Ordering::SeqCst);
                prepared.resolver.credential_observations().await.unwrap();
                assert!(
                    prepared.resolver.capability_observations().await.unwrap()[0]
                        .negotiated()
                        .is_none()
                );
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
                discovery.detected.store(true, Ordering::SeqCst);
                prepared
                    .resolver
                    .add_bindings([AcpLocalCredentialBinding {
                        credential: credential.credential.clone(),
                        cli_id: "claude".into(),
                        enabled: false,
                    }])
                    .unwrap();
                prepared.resolver.credential_observations().await.unwrap();
                assert_eq!(
                    prepared.resolver.capability_observations().await.unwrap()[0]
                        .negotiated()
                        .is_some(),
                    success
                );
                assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
                assert_eq!(
                    prepared
                        .resolver
                        .credential_observations()
                        .await
                        .unwrap()
                        .into_iter()
                        .next()
                        .unwrap()
                        .state,
                    CredentialObservationState::Disabled
                );
            }
        }
    }

    async fn observed_bindings(
        resolver: &AcpLocalCredentialResolver,
    ) -> BTreeMap<CredentialRef, CredentialObservationState> {
        // Observation time is freshly sampled, not part of binding identity.
        resolver
            .credential_observations()
            .await
            .unwrap()
            .into_iter()
            .map(|observation| (observation.credential, observation.state))
            .collect()
    }

    #[tokio::test]
    async fn binding_batch_rejection_preserves_every_existing_and_absent_entry() {
        // Cause/effect decision table (the source iterator order is not map order):
        // R1 malformed revision / missing locator / unknown ACP / duplicate id
        //    => reject before any mutation, including a preceding valid source.
        // R2 existing id has another revision, at first/middle/last map position
        //    => reject the WHOLE batch: neither new keys nor status updates leak.
        // R3 empty batch => no-op; retry after rejection => ordinary full success;
        //    same-revision status update => publish the requested Disabled state.
        // Constraint: WorkerLocal rows are generated by the canonical repository;
        // only the named fault is injected. Status-only updates intentionally keep
        // the existing API semantics; this test does not claim revision rotation.
        let repo = InMemoryCredentialRepo::new();
        let mut sources = Vec::new();
        for workspace in ["a", "m", "z"] {
            sources.push(
                ensure_worker_local(
                    &repo,
                    workspace,
                    WorkerLocalBinding::new("acp:codex", "default"),
                    None,
                )
                .await
                .unwrap(),
            );
        }
        for conflict_index in 0..sources.len() {
            for reverse in [false, true] {
                let resolver = resolver_from_sources(
                    Arc::new(AvailableDiscovery),
                    sources.clone(),
                    awaken_time::system_clock(),
                )
                .unwrap();
                let before = observed_bindings(&resolver).await;
                let fresh = ensure_worker_local(
                    &repo,
                    "b",
                    WorkerLocalBinding::new("acp:claude", "default"),
                    None,
                )
                .await
                .unwrap();
                let mut batch = sources.clone();
                for source in &mut batch {
                    source.status = CredentialStatus::Disabled;
                }
                batch[conflict_index].version += 1;
                batch.push(fresh.clone());
                if reverse {
                    batch.reverse();
                }
                assert!(add_sources(&resolver, batch).is_err(), "R2");
                assert_eq!(observed_bindings(&resolver).await, before, "R2");
                add_sources(&resolver, []).expect("R3 empty");
                assert_eq!(observed_bindings(&resolver).await, before);
                add_sources(&resolver, [fresh]).expect("R3 retry");
                assert_eq!(resolver.credential_observations().await.unwrap().len(), 4);
                let mut disabled = sources[0].clone();
                disabled.status = CredentialStatus::Disabled;
                add_sources(&resolver, [disabled]).expect("R3 status update");
                assert_eq!(
                    observed_bindings(&resolver).await[&CredentialRef {
                        id: sources[0].id.0.clone(),
                        revision: 1,
                    }],
                    CredentialObservationState::Disabled
                );
            }
        }
        for fault in [
            "revision",
            "negative_revision",
            "locator",
            "cli",
            "duplicate",
        ] {
            let resolver = resolver_from_sources(
                Arc::new(AvailableDiscovery),
                [sources[2].clone()],
                awaken_time::system_clock(),
            )
            .unwrap();
            let before = observed_bindings(&resolver).await;
            let mut malformed = sources[1].clone();
            match fault {
                "revision" => malformed.version = 0,
                "negative_revision" => malformed.version = -1,
                "locator" => malformed.worker_local_binding = None,
                "cli" => {
                    malformed.worker_local_binding =
                        Some(WorkerLocalBinding::new("acp:unknown", "default"))
                }
                "duplicate" => malformed = sources[0].clone(),
                _ => unreachable!(),
            }
            assert!(
                add_sources(&resolver, [sources[0].clone(), malformed]).is_err(),
                "R1 {fault}"
            );
            assert_eq!(observed_bindings(&resolver).await, before, "R1 {fault}");
        }
    }

    #[tokio::test]
    async fn workspace_binding_extends_one_shared_liveness_resolver() {
        // Cause/effect decision table:
        // B1 first Workspace -> one idempotent WorkerLocal revision observed;
        // B2 second Workspace -> both revisions observed by the same resolver;
        // B3 repeat Workspace -> no duplicate source or competing resolver.
        let directory = tempfile::tempdir().unwrap();
        let fixture = Arc::new(CapabilityFixture {
            success: true,
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let prepared = prepare_host_acp_with(
            LocalAcpPreparation {
                probe_cwd: directory.path().join("probe"),
                wrapper_root: directory.path().join("wrappers"),
                initial_workspace: None,
                selected_cli_ids: Some(["codex".into()].into_iter().collect()),
                credentials: Arc::new(InMemoryCredentialRepo::new()),
                clock: awaken_time::system_clock(),
            },
            Arc::new(AvailableDiscovery),
            fixture.clone(),
            fixture,
        )
        .await
        .unwrap();
        let resolver = prepared.resolver.clone();
        let credentials = InMemoryCredentialRepo::new();
        prepared
            .bind_workspace(&credentials, "workspace-a")
            .await
            .expect("B1");
        assert_eq!(
            resolver.credential_observations().await.unwrap().len(),
            1,
            "B1"
        );
        prepared
            .bind_workspace(&credentials, "workspace-b")
            .await
            .expect("B2");
        assert_eq!(
            resolver.credential_observations().await.unwrap().len(),
            2,
            "B2"
        );
        prepared
            .bind_workspace(&credentials, "workspace-a")
            .await
            .expect("B3");
        assert_eq!(
            resolver.credential_observations().await.unwrap().len(),
            2,
            "B3"
        );
    }
}
