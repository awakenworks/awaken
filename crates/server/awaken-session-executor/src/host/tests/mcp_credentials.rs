//! Published MCP credential materialization and realizer ownership.

use super::*;

#[tokio::test]
async fn published_mcp_credential_is_materialized_only_for_its_workspace_and_revision() {
    use awaken_ext_mcp::McpToolTransport;
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_execution_contract::McpAttachmentRealizer;

    struct PinnedBearerRefresher;

    #[async_trait::async_trait]
    impl awaken_ext_mcp::CredentialRefresher for PinnedBearerRefresher {
        async fn refresh(
            &self,
            _challenge: &awaken_ext_mcp::AuthChallenge,
        ) -> Option<awaken_ext_mcp::Credential> {
            Some(awaken_ext_mcp::Credential::Bearer(
                "published-mcp-token".into(),
            ))
        }
    }

    struct ExactRefreshFactory;

    impl awaken_credential_materializer::CredentialRefreshFactory for ExactRefreshFactory {
        fn refresher(
            &self,
            _credential_id: awaken_credential_contract::CredentialSourceId,
            _access: awaken_runtime_contract::CredentialRefreshAccess,
        ) -> Arc<dyn awaken_ext_mcp::CredentialRefresher> {
            Arc::new(PinnedBearerRefresher)
        }

        fn bearer_reloader(
            &self,
            _credential_id: awaken_credential_contract::CredentialSourceId,
            _credential_revision: u64,
        ) -> Arc<dyn awaken_ext_mcp::CredentialRefresher> {
            Arc::new(PinnedBearerRefresher)
        }
    }

    let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
    let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
    let host = (((crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .with_credentials(credentials.clone(), secrets.clone()))
    .with_session_execution(
        crate::SessionExecutionDependencies::default()
            .with_credential_refresh_factory(Arc::new(ExactRefreshFactory)),
    ))
    .build()
    .expect("validated Host dependencies");

    let mut credential = awaken_credential_vault::repo::enter_credential(
        awaken_credential_vault::CredentialCreateParams {
            workspace_id: "workspace-a".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(awaken_agent_contract::RedactedString::from(
                "published-mcp-token".to_string(),
            )),
            oauth_command: None,
        },
        secrets.as_ref(),
        credentials.as_ref(),
        awaken_time::system_clock().as_ref(),
    )
    .await
    .unwrap();
    let managed = crate::ManagedHost::new(host.clone());
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    let (mcp_url, seen) = crate::test_mcp::start(Some("Bearer published-mcp-token")).await;
    let mcp_audience = awaken_agent_contract::McpTarget::identity(&mcp_url)
        .unwrap()
        .canonical_url();
    credential.descriptor = Some(awaken_credential_contract::CredentialDescriptor::new(
        mcp_audience.clone(),
        awaken_credential_contract::CredentialMaterialDescriptor::secret(
            awaken_credential_contract::OPAQUE_SECRET_MATERIAL_TYPE,
        ),
        [awaken_credential_contract::CredentialTargetContract::new(
            awaken_credential_contract::CredentialTarget::new(
                awaken_credential_contract::CredentialPurpose::McpAuthorization,
                mcp_audience.clone(),
            ),
            CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
        )],
    ));
    awaken_credential_vault::repo::CredentialRepo::put(credentials.as_ref(), credential.clone())
        .await
        .unwrap();
    let generation = |session: &str| {
        crate::test_mcp::mcp_fence(session, "mcp-docs", 1, "runtime-1", 1, u64::MAX - 1)
    };
    let request =
        |session: &str, workspace: &str, revision: u64| crate::session_mcp::StageMcpAttachment {
            workspace_id: workspace.into(),
            generation: generation(session),
            realization_id: format!("realize-{session}"),
            stage_idempotency_key: format!("stage-{session}"),
            name: "docs".into(),
            target: awaken_agent_contract::McpTarget::parse_http(&mcp_url).unwrap(),
            credential: Some(
                CredentialAccess::new(
                    CredentialRef {
                        id: credential.id.0.clone(),
                        revision,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::HttpHeader {
                        name: "authorization".into(),
                        scheme: Some("Bearer".into()),
                    },
                    CredentialExecutionPolicy::exact(
                        holder.clone(),
                        ModelExposurePolicy::VirtualOnly,
                    ),
                )
                .with_target(awaken_runtime_contract::CredentialTarget::new(
                    awaken_credential_contract::CredentialPurpose::McpAuthorization,
                    mcp_audience.clone(),
                )),
            ),
            prompts_as_skills: false,
            selected_plaintext_holder: Some(holder.clone()),
        };

    // Cause graph: exact described MCP target + workspace + revision + allowed
    // Worker holder -> Native host material is staged but invisible; durable
    // publication command -> visible.
    // ACP authentication additionally requires installed no-bypass provider evidence
    // before material resolution; anonymous ACP has no secret and needs no custody proof.
    //
    // | Rule | workspace | revision | stage | publish | Effect |
    // |---|---|---|---|---|---|
    // | H1 | exact | exact | success | no | staged/invisible |
    // | H2 | exact | exact | replay | no | same receipt/no duplicate |
    // | H3 | exact | exact | success | yes | active/visible |
    // | H4 | exact | stale | fail | - | no projection |
    // | H5 | foreign | exact | fail | - | no projection |
    // | H6 | exact | exact | drain | - | hidden/material cleared |
    // | H7 | exact | exact | drain replay | - | idempotent |
    // | H8 | exact | exact | publish removed | - | reject |
    // | H9 | exact | exact | expired stage | - | reject/no projection |
    // | H10 | exact | exact | conflicting replay | - | reject/no duplicate |
    // | H11 | exact | exact | drain unknown | - | idempotent no-op |
    // | H12 | exact | exact | authenticated ACP/no no-bypass | - | reject before resolver |
    // | H13 | - | - | anonymous ACP | no | staged without bearer |
    // | H14 | exact immutable binding | later lease | renew | publish | same generation/material, extended fence |
    // | H15 | changed target | later lease | renew | - | reject/no mutation |
    // | H16 | exact binding | same lease/new key | renew | - | reject/no mutation |
    // | H17 | non-bearer usage | exact holder/revision | stage | - | reject before materialization |
    // | H18 | authenticated ACP/Forbidden exposure | exact | stage | - | reject before relay/no lookup |
    // | H19 | expired predecessor/current same-epoch renewal | exact | stage+publish | admitted effects complete under current local authority |
    // | H20 | authenticated ACP/provider evidence + prepared external relay + exact allowlist | exact | stage+publish | external opaque route; no inline secret |
    // | H21 | non-OAuth bearer/exact factory | exact | stage | - | one neutral challenge refresher; no Vault in Runtime |
    // | H22 | exact removed tombstone | exact request replay | stage | - | rebuild one staged projection/material |
    // | H24 | Managed prompts-as-skills | any backend | stage | - | reject before projection/materialization |
    // | H25 | exact Staging owner | exact replay | stage | - | retryable unavailable/no false staged receipt |
    // | H26 | default weak / frozen BackendOwned strong | exact | stage | - | admit from selected strong provider |
    // | H27 | default strong / frozen BackendOwned weak | exact | stage | - | reject from selected weak provider |
    // | H28 | removed immutable binding | renewed same-owner lease | stage | - | rebuild exactly one projection, never renew absent effects |
    // | H29 | external relay configured/not started + exact allowlist | exact | stage | - | reject before materialization |
    // | H30 | external relay prepared + upstream-only allowlist | exact | stage | - | reject before materialization |
    // | H31 | no external relay config + existing provider capability fixture | exact | stage+publish+call | - | legacy lazy loopback route injects bearer; no isolated reachability claim |
    // | H32 | prepared external relay exits before a later stage | exact allowlist | stage | - | reject dead OnceCell owner before materialization |
    let exact_request = request("mcp-exact", "workspace-a", 1);
    let receipt = managed
        .stage_mcp_attachment(exact_request.clone())
        .await
        .expect("H1");
    assert!(host.active_mcp_projections("mcp-exact").is_empty(), "H1");
    assert_eq!(
        managed
            .stage_mcp_attachment(exact_request)
            .await
            .expect("H2"),
        receipt,
        "H2"
    );
    assert_eq!(
        host.mcp_projection(&generation("mcp-exact"))
            .unwrap()
            .server
            .unwrap()
            .bearer()
            .map(|secret| secret.expose_secret()),
        Some("published-mcp-token")
    );
    let projection = host.mcp_projection(&generation("mcp-exact")).unwrap();
    assert!(
        matches!(
            projection.server.unwrap().transport,
            awaken_session_mcp::McpTransportMaterialKind::Http {
                refresh: Some(_),
                ..
            }
        ),
        "H21"
    );
    managed
        .publish_mcp_generation(generation("mcp-exact"))
        .await
        .expect("H3");
    assert_eq!(host.active_mcp_projections("mcp-exact").len(), 1, "H3");

    let stale_error = managed
        .stage_mcp_attachment(request("mcp-stale", "workspace-a", 2))
        .await
        .unwrap_err();
    assert_eq!(stale_error.code, "mcp_credential_revision_mismatch", "H4");

    let foreign_error = managed
        .stage_mcp_attachment(request("mcp-foreign", "workspace-b", 1))
        .await
        .unwrap_err();
    assert_eq!(foreign_error.code, "mcp_credential_revision_mismatch", "H5");

    let mut unsupported_usage = request("mcp-usage", "workspace-a", 1);
    unsupported_usage.credential.as_mut().unwrap().usage = CredentialUsage::QueryParameter {
        name: "token".into(),
    };
    assert_eq!(
        managed
            .stage_mcp_attachment(unsupported_usage)
            .await
            .unwrap_err()
            .code,
        "mcp_credential_usage_unsupported",
        "H17"
    );
    assert!(
        host.mcp_projection(&generation("mcp-usage")).is_none(),
        "H17"
    );
    let mut prompt_skill = request("mcp-native-prompt-skill", "workspace-a", 1);
    prompt_skill.prompts_as_skills = true;
    assert_eq!(
        managed
            .stage_mcp_attachment(prompt_skill)
            .await
            .unwrap_err()
            .code,
        "mcp_prompt_skills_unsupported",
        "H24"
    );
    assert!(
        host.mcp_projection(&generation("mcp-native-prompt-skill"))
            .is_none(),
        "H24"
    );

    let mut conflicting_renewal = request("mcp-exact", "workspace-a", 1);
    conflicting_renewal.generation = conflicting_renewal.generation.expiring_at(u64::MAX);
    conflicting_renewal.stage_idempotency_key = "renew-conflicting".into();
    conflicting_renewal.target =
        awaken_agent_contract::McpTarget::parse_http("https://other.example.test/mcp").unwrap();
    assert_eq!(
        managed
            .stage_mcp_attachment(conflicting_renewal)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_generation",
        "H15"
    );
    let mut non_increasing = request("mcp-exact", "workspace-a", 1);
    non_increasing.stage_idempotency_key = "renew-without-extension".into();
    assert_eq!(
        managed
            .stage_mcp_attachment(non_increasing)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_generation",
        "H16"
    );
    let mut renewal = request("mcp-exact", "workspace-a", 1);
    renewal.generation = renewal.generation.expiring_at(u64::MAX);
    renewal.stage_idempotency_key = "renew-exact".into();
    let renewed_generation = renewal.generation.clone();
    let renewed = managed
        .stage_mcp_attachment(renewal.clone())
        .await
        .expect("H14 stage");
    assert_eq!(renewed.generation, renewed_generation, "H14");
    managed
        .publish_mcp_generation(renewed_generation.clone())
        .await
        .expect("H14 publish");
    let projection = host
        .mcp_projection(&renewed_generation)
        .expect("H14 projection");
    assert_eq!(
        projection
            .server
            .as_ref()
            .and_then(|server| server.bearer())
            .map(|secret| secret.expose_secret()),
        Some("published-mcp-token"),
        "H14 retained material"
    );
    managed
        .drain_mcp_generation(renewed_generation.clone())
        .await
        .expect("H6");
    assert!(host.active_mcp_projections("mcp-exact").is_empty(), "H6");
    let drained = host.mcp_projection(&renewed_generation).unwrap();
    assert!(
        drained.server.is_none() && drained.native_wiring.is_none(),
        "H6"
    );
    managed
        .drain_mcp_generation(renewed_generation.clone())
        .await
        .expect("H7");
    assert!(
        managed
            .publish_mcp_generation(renewed_generation.clone())
            .await
            .is_err(),
        "H8"
    );
    let recovered = managed
        .stage_mcp_attachment(renewal)
        .await
        .expect("H22 exact removed replay");
    assert_eq!(recovered.generation, renewed_generation, "H22");
    let recovered_projection = host
        .mcp_projection(&renewed_generation)
        .expect("H22 rebuilt projection");
    assert_eq!(
        recovered_projection.state,
        awaken_session_mcp::McpProjectionState::Staged,
        "H22"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization("mcp-exact", |r| r.staging.mcp.projection_count()),
        Some(1),
        "H22 no duplicate tombstone"
    );
    let staging_request = recovered_projection.request.clone();
    // H28: cleanup and a heartbeat can occur between failed activation and
    // retry. A Removed slot owns no effect. A later expiry with the same
    // immutable binding must use ordinary staging, not fail the active-renewal
    // gate or fabricate an already-staged receipt. H15/H16/H25 retain changed
    // binding, equal-expiry/new-key and in-flight Stage rejection coverage.
    let old_removed = request("mcp-removed-renewal", "workspace-a", 1);
    managed
        .stage_mcp_attachment(old_removed.clone())
        .await
        .unwrap();
    managed
        .drain_mcp_generation(old_removed.generation.clone())
        .await
        .unwrap();
    let mut renewed_removed = old_removed.clone();
    renewed_removed.generation = renewed_removed.generation.expiring_at(u64::MAX);
    renewed_removed.stage_idempotency_key = "retry-after-heartbeat".into();
    // H29: any changed immutable coordinate OR a non-increasing expiry with
    // changed attempt key => retain the removed owner. Only H28's strictly
    // renewed binding may remove it. Every rejected row leaves the same slot.
    for changed in [
        "workspace",
        "attachment",
        "generation",
        "incarnation",
        "epoch",
        "realization",
        "name",
        "target",
        "credential",
        "holder",
        "same-expiry",
        "older-expiry",
    ] {
        let mut rejected = renewed_removed.clone();
        let mut w = rejected.generation.to_wire();
        match changed {
            "workspace" => rejected.workspace_id.push_str("-other"),
            "attachment" => {
                w.generation = crate::session_mcp::McpGeneration::new(
                    crate::session_mcp::McpAttachmentId(format!(
                        "{}-other",
                        w.generation.attachment_id().0
                    )),
                    w.generation.version(),
                    w.generation.digest().clone(),
                );
            }
            "generation" => w.generation = w.generation.at_version(w.generation.version() + 1),
            "incarnation" => w.runtime_incarnation.push_str("-other"),
            "epoch" => w.lease_epoch += 1,
            "realization" => rejected.realization_id.push_str("-other"),
            "name" => rejected.name.push_str("-other"),
            "target" => {
                rejected.target =
                    awaken_agent_contract::McpTarget::parse_http("https://other.example.test/mcp")
                        .unwrap()
            }
            "credential" => rejected.credential = None,
            "holder" => rejected.selected_plaintext_holder = None,
            "same-expiry" => w.lease_expires_at_unix_ms -= 1,
            "older-expiry" => w.lease_expires_at_unix_ms -= 2,
            _ => unreachable!(),
        }
        rejected.generation = w.try_into().unwrap();
        assert!(
            !host.forget_replayable_removed_mcp_projection(&rejected),
            "H29 {changed}"
        );
        assert!(
            host.mcp_projection(&old_removed.generation).is_some(),
            "H29 retained {changed}"
        );
    }
    let rebuilt = managed
        .stage_mcp_attachment(renewed_removed.clone())
        .await
        .expect("H28");
    rebuilt
        .verify(&renewed_removed)
        .expect("H28 exact fresh receipt");
    assert!(
        host.mcp_projection(&old_removed.generation).is_none(),
        "H28 old tombstone removed"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization("mcp-removed-renewal", |r| r.staging.mcp.projection_count()),
        Some(1),
        "H28 one projection"
    );
    assert_eq!(
        host.mcp_projection(&rebuilt.generation).unwrap().state,
        awaken_session_mcp::McpProjectionState::Staged,
        "H28 staged, not published"
    );
    host.installed_sessions
        .realizations()
        .modify_realization("mcp-exact", |r| {
            r.staging.mcp.projections_mut()[0].state =
                awaken_session_mcp::McpProjectionState::Staging;
            r.staging.mcp.projections_mut()[0].staging =
                Some(awaken_session_mcp::McpStagingActivity::default());
        });
    assert_eq!(
        managed
            .stage_mcp_attachment(staging_request)
            .await
            .unwrap_err()
            .code,
        "mcp_generation_staging",
        "H25"
    );
    host.installed_sessions
        .realizations()
        .modify_realization("mcp-exact", |r| {
            r.staging.mcp.projections_mut()[0].state =
                awaken_session_mcp::McpProjectionState::Staged;
            r.staging.mcp.projections_mut()[0].staging = None;
        });
    let mut expired = request("mcp-expired", "workspace-a", 1);
    expired.generation = expired.generation.expiring_at(0);
    assert_eq!(
        managed
            .stage_mcp_attachment(expired)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_ownership",
        "H9"
    );
    assert!(host.mcp_projection(&generation("mcp-expired")).is_none());
    let mut admitted_before_renewal = request("mcp-renewed-authority", "workspace-a", 1);
    admitted_before_renewal.generation = admitted_before_renewal.generation.expiring_at(0);
    host.install_session_realization_lease(
        "mcp-renewed-authority",
        crate::session_resource::SessionRealizationLease {
            owner: "worker-a".into(),
            runtime_incarnation: "runtime-1".into(),
            epoch: 1,
            expires_at_unix_ms: u64::MAX,
        },
    )
    .unwrap();
    let admitted_receipt = managed
        .stage_mcp_attachment(admitted_before_renewal)
        .await
        .expect("H19 stage admitted before renewal");
    managed
        .publish_mcp_generation(admitted_receipt.generation)
        .await
        .expect("H19 publish admitted before renewal");
    let first = request("mcp-conflict", "workspace-a", 1);
    managed
        .stage_mcp_attachment(first)
        .await
        .expect("H10 setup");
    let mut conflict = request("mcp-conflict", "workspace-a", 1);
    conflict.stage_idempotency_key = "another-key".into();
    assert_eq!(
        managed
            .stage_mcp_attachment(conflict)
            .await
            .unwrap_err()
            .code,
        "mcp_stale_generation",
        "H10"
    );
    assert_eq!(
        host.installed_sessions
            .realizations()
            .read_realization("mcp-conflict", |r| r.staging.mcp.projection_count()),
        Some(1),
        "H10"
    );
    managed
        .drain_mcp_generation(generation("mcp-never-staged"))
        .await
        .expect("H11");

    let launch = awaken_run_executor_acp::AcpLaunch::custom(vec!["true".into()], vec![]);
    let source = Arc::new(awaken_run_executor_acp::SubprocessChannelSource::new(
        launch,
    ));
    let executor = Arc::new(awaken_run_executor_acp::AcpRunExecutor::new(source));
    let acp_host = ((crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_acp(executor.clone()))
    .with_credentials(credentials.clone(), secrets.clone()))
    .build()
    .expect("validated Host dependencies");
    acp_host.register_thread_backend_projection("mcp-acp-forbidden", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-protected", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-anonymous", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-prompt-skill", "acp:test");
    acp_host.register_thread_backend_projection("mcp-acp-client-basic", "acp:claude");
    let host_acp_snapshot = awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
        .resolved_model(
            awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                awaken_runtime_contract::resolved::ModelBinding::new(
                    "provider", "model", "acp:test",
                ),
            ),
        )
        .build();
    // H12 has one protected-session case: install its exact publication
    // directly so the fixture does not imply an untested multi-case partition.
    let protected_session = "mcp-acp-protected";
    acp_host.register_thread_agent_projection(protected_session, "assistant");
    acp_host
        .realizer
        .publication
        .retain_session_publication(protected_session, Some(&host_acp_snapshot))
        .expect("H12 exact HostExecutor publication");
    // Deliberately install no credential resolver: the no-bypass failure must mask
    // material-source availability and prove no secret lookup was attempted.
    let acp_managed = crate::ManagedHost::new(acp_host.clone());
    let mut forbidden = request("mcp-acp-forbidden", "workspace-a", 1);
    forbidden.credential.as_mut().unwrap().policy.model_exposure = ModelExposurePolicy::Forbidden;
    assert_eq!(
        acp_managed
            .stage_mcp_attachment(forbidden)
            .await
            .unwrap_err()
            .code,
        "mcp_model_exposure_forbidden",
        "H18"
    );
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-forbidden"))
            .is_none(),
        "H18"
    );
    let acp_error = acp_managed
        .stage_mcp_attachment(request("mcp-acp-protected", "workspace-a", 1))
        .await
        .unwrap_err();
    assert_eq!(acp_error.code, "mcp_holder_unsupported", "H12");
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-protected"))
            .is_none(),
        "H12"
    );

    let workload_holder = PlaintextHolder::new(
        PlaintextBoundary::Workload,
        awaken_credential_contract::SELF_HOSTED_ACP_TRUST_DOMAIN,
    );
    let basic_client = crate::ManagedHost::new(acp_host.clone());
    let mut basic_client_request = request("mcp-acp-client-basic", "workspace-a", 1);
    basic_client_request.selected_plaintext_holder = Some(workload_holder.clone());
    basic_client_request.credential.as_mut().unwrap().policy =
        CredentialExecutionPolicy::exact(workload_holder.clone(), ModelExposurePolicy::Forbidden);
    let basic_client_receipt = basic_client
        .stage_mcp_attachment(basic_client_request)
        .await
        .expect("H23 client injection does not require WorkerRelay provider custody");
    assert_eq!(
        basic_client_receipt.actual_realization_kind,
        Some(awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField),
        "H23"
    );

    let mut anonymous = request("mcp-acp-anonymous", "workspace-a", 1);
    anonymous.credential = None;
    anonymous.selected_plaintext_holder = None;
    let mut prompt_skill = anonymous.clone();
    prompt_skill.generation = generation("mcp-acp-prompt-skill");
    prompt_skill.realization_id = "realize-mcp-acp-prompt-skill".into();
    prompt_skill.stage_idempotency_key = "stage-mcp-acp-prompt-skill".into();
    prompt_skill.prompts_as_skills = true;
    assert_eq!(
        acp_managed
            .stage_mcp_attachment(prompt_skill)
            .await
            .unwrap_err()
            .code,
        "mcp_prompt_skills_unsupported",
        "H24 backend-independent Managed rejection"
    );
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-prompt-skill"))
            .is_none(),
        "H24"
    );
    acp_managed
        .stage_mcp_attachment(anonymous)
        .await
        .expect("H13");
    assert!(
        acp_host
            .mcp_projection(&generation("mcp-acp-anonymous"))
            .and_then(|projection| projection.server)
            .is_some_and(|server| server.bearer().is_none()),
        "H13"
    );

    // H20 covers only Host admission and the external relay transport
    // prerequisite. This fixture supplies capability fields but does not
    // implement or observe provider substitution, no-bypass enforcement, or
    // container reachability, so it is not G43 deployment evidence.
    struct SecureExternalProvider;

    #[async_trait::async_trait]
    impl awaken_container_contract::ContainerEnvironmentProvider for SecureExternalProvider {
        fn adoption_layout(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
        ) -> Result<
            awaken_container_contract::ContainerAdoptionLayout,
            awaken_provisioning_contract::SandboxError,
        > {
            awaken_sandbox_container::container_adoption_layout(handle)
        }

        fn physical_incarnation(
            &self,
            handle: &awaken_provisioning_contract::SandboxHandle,
        ) -> Result<String, awaken_provisioning_contract::SandboxError> {
            awaken_sandbox_container::container_physical_incarnation(handle)
        }

        async fn create_environment_for_effect(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
            _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
            _intent: awaken_container_contract::EnvironmentRealizationIntent,
        ) -> Result<
            std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced Environment creation",
            ))
        }
        async fn observe_environment(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
            _basis: awaken_provisioning_contract::SandboxObservationBasis<'_>,
        ) -> Result<
            awaken_provisioning_contract::SandboxObservation,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not observe Environments",
            ))
        }
        async fn adopt_environment_for_effect(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
            _effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        ) -> Result<
            std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced Environment adoption",
            ))
        }
        async fn prepare_terminal_environment_for_effect(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
            _handle: Option<&awaken_provisioning_contract::SandboxHandle>,
            _expected_effect_fence: Option<&awaken_provisioning_contract::SandboxEffectFence>,
            _terminal_effect_fence: &awaken_provisioning_contract::SandboxEffectFence,
            _authority: &dyn awaken_provisioning_contract::CurrentExecutionAuthority,
        ) -> Result<
            Option<std::sync::Arc<dyn awaken_container_contract::ContainerEnvironment>>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "test double does not implement fenced terminal preparation",
            ))
        }

        async fn probe_ready(&self) -> Result<(), awaken_provisioning_contract::SandboxError> {
            Ok(())
        }

        fn sandbox_capabilities(&self) -> awaken_provisioning_contract::SandboxCapabilities {
            managed_test_container_capabilities()
        }

        async fn create_unattached_environment(
            &self,
            _spec: &awaken_provisioning_contract::SandboxSpec,
        ) -> Result<
            Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "H20 exercises pre-environment MCP staging only",
            ))
        }

        async fn adopt_unattached_environment(
            &self,
            _adoption: awaken_container_contract::ContainerEnvironmentAdoption<'_>,
        ) -> Result<
            Arc<dyn awaken_container_contract::ContainerEnvironment>,
            awaken_provisioning_contract::SandboxError,
        > {
            Err(awaken_provisioning_contract::SandboxError::unsupported(
                "H20 exercises pre-environment MCP staging only",
            ))
        }
    }

    struct UnusedHandFactory;

    impl awaken_session_environment_contract::HandExecutorFactory for UnusedHandFactory {
        fn bind(
            &self,
            _channel: Box<dyn awaken_run_executor_acp::AgentChannelType>,
            _operation_scope: &str,
            _recovery: awaken_tool_contract::tool::ToolRecoveryCapability,
        ) -> awaken_session_environment_contract::HandExecutorBinding {
            panic!("H20 does not launch an Agent process")
        }
    }

    let relay_reservation = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let relay_port = relay_reservation.local_addr().unwrap().port();
    drop(relay_reservation);
    let relay_host = "relay.awaken-cloud.svc";
    struct StagingOnlyProxy;
    impl awaken_session_mcp::McpRelayOutboundProxy for StagingOnlyProxy {
        fn issue_for(&self, _upstream_host: &str, _now_unix_ms: u64) -> Result<String, String> {
            Err("stage-only fixture has no upstream egress".into())
        }
    }
    let mut secure_deployment = crate::DeploymentConfig::ephemeral();
    secure_deployment.sandbox.container_mcp_relay = Some(
        awaken_session_mcp::ContainerMcpRelayExposure::new(
            format!("0.0.0.0:{relay_port}").parse().unwrap(),
            format!("http://{relay_host}:{relay_port}"),
        )
        .unwrap(),
    );
    // Causes: external relay exposure without/with the installed signed
    // outbound proxy. Effects: missing issuer refuses before Worker Ready;
    // installed issuer permits transport staging without asserting public
    // upstream reachability. R1 no issuer -> refusal; R2 issuer -> bind.
    let missing_proxy_host = (crate::HostDependencies::ephemeral_with_deployment(
        Arc::new(OkModel),
        "stub",
        secure_deployment.clone(),
    )
    .await)
        .build()
        .expect("validated Host dependencies");
    assert!(
        missing_proxy_host
            .prepare_container_mcp_relay()
            .await
            .is_err(),
        "R1 external relay without signed CONNECT issuer"
    );
    let secure_acp_host = (crate::HostDependencies::ephemeral_with_deployment(
        Arc::new(OkModel),
        "stub",
        secure_deployment,
    )
    .await
    .with_credentials(credentials.clone(), secrets.clone())
    .with_mcp_relay_outbound_proxy(Arc::new(StagingOnlyProxy))
    .with_acp(executor.clone())
    .with_session_container_provider(
        Arc::new(SecureExternalProvider),
        Arc::new(UnusedHandFactory),
    ))
    .build()
    .expect("validated Host dependencies");
    secure_acp_host.register_thread_backend_projection("mcp-acp-secure", "acp:test");
    secure_acp_host.register_thread_backend_projection("mcp-acp-client", "acp:claude");
    secure_acp_host.register_thread_backend_projection("mcp-acp-refresh", "acp:claude");
    secure_acp_host.register_thread_backend_projection("mcp-acp-unstarted", "acp:test");
    secure_acp_host.register_thread_backend_projection("mcp-acp-wrong-network", "acp:test");
    secure_acp_host.register_thread_backend_projection("mcp-acp-dead-relay", "acp:test");
    secure_acp_host
        .install_environment_projection(
            "mcp-acp-secure",
            &session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                    hosts: vec![relay_host.into()],
                },
                serde_json::json!({}),
            ),
        )
        .unwrap();
    for session in [
        "mcp-acp-unstarted",
        "mcp-acp-wrong-network",
        "mcp-acp-dead-relay",
    ] {
        secure_acp_host.register_thread_agent_projection(session, "assistant");
        secure_acp_host
            .realizer
            .publication
            .retain_session_publication(session, Some(&host_acp_snapshot))
            .unwrap();
    }
    secure_acp_host
        .install_environment_projection(
            "mcp-acp-unstarted",
            &session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                    hosts: vec![relay_host.into()],
                },
                serde_json::json!({}),
            ),
        )
        .unwrap();
    secure_acp_host
        .install_environment_projection(
            "mcp-acp-wrong-network",
            &session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                    hosts: vec!["upstream.example.test".into()],
                },
                serde_json::json!({}),
            ),
        )
        .unwrap();
    secure_acp_host
        .install_environment_projection(
            "mcp-acp-dead-relay",
            &session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                    hosts: vec![relay_host.into()],
                },
                serde_json::json!({}),
            ),
        )
        .unwrap();
    let external_gate = crate::ManagedHost::new(secure_acp_host.clone());
    assert_eq!(
        external_gate
            .stage_mcp_attachment(request("mcp-acp-unstarted", "workspace-a", 1))
            .await
            .unwrap_err()
            .code,
        "mcp_relay_unavailable",
        "H29"
    );
    secure_acp_host
        .prepare_container_mcp_relay()
        .await
        .expect("H20 external relay transport is bound before staging");
    assert_eq!(
        external_gate
            .stage_mcp_attachment(request("mcp-acp-wrong-network", "workspace-a", 1))
            .await
            .unwrap_err()
            .code,
        "mcp_relay_network_not_frozen",
        "H30"
    );
    secure_acp_host.register_thread_agent_projection("mcp-acp-secure", "assistant");
    secure_acp_host
        .realizer
        .publication
        .retain_session_publication("mcp-acp-secure", Some(&host_acp_snapshot))
        .expect("H20 exact HostExecutor publication");

    // H31 preserves only the established loopback compatibility behavior. The
    // provider fixture does not observe network topology, so this is not evidence
    // that loopback is reachable from an isolated container or of G43 custody.
    let default_relay_host = ((crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_acp(executor.clone())
        .with_session_container_provider(
            Arc::new(SecureExternalProvider),
            Arc::new(UnusedHandFactory),
        ))
    .with_credentials(credentials.clone(), secrets.clone()))
    .build()
    .expect("validated Host dependencies");
    default_relay_host.register_thread_backend_projection("mcp-acp-default-relay", "acp:test");
    default_relay_host.register_thread_agent_projection("mcp-acp-default-relay", "assistant");
    default_relay_host
        .realizer
        .publication
        .retain_session_publication("mcp-acp-default-relay", Some(&host_acp_snapshot))
        .unwrap();
    let default_relay_managed = crate::ManagedHost::new(default_relay_host.clone());
    let seen_before = seen.lock().unwrap().len();
    let default_generation = generation("mcp-acp-default-relay");
    default_relay_host
        .install_session_realization_lease(
            default_generation.session_id().as_str(),
            crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: default_generation.runtime_incarnation().to_owned(),
                epoch: default_generation.lease_epoch().get(),
                expires_at_unix_ms: default_generation.lease_expires_at_unix_ms(),
            },
        )
        .unwrap();
    let default_receipt = default_relay_managed
        .stage_mcp_attachment(request("mcp-acp-default-relay", "workspace-a", 1))
        .await
        .expect("H31 no-config relay keeps original lazy initialization");
    default_relay_managed
        .publish_mcp_generation(default_generation)
        .await
        .expect("H31 publish the exact loopback route");
    let default_projection = default_relay_host
        .mcp_projection(&default_receipt.generation)
        .unwrap();
    let default_transport = awaken_session_mcp::project_mcp_session_transport(
        default_projection.server.as_ref().unwrap(),
        &default_projection.request,
        default_relay_host.mcp.relay(),
        &default_projection.receipt,
        awaken_run_executor_acp::acp_adapter_policy(),
        "codex",
    )
    .unwrap();
    assert!(
        default_transport
            .url
            .as_deref()
            .is_some_and(|url| url.starts_with("http://127.0.0.1:")),
        "H31"
    );
    let default_route = default_transport.url.expect("H31 loopback route");
    let transport = awaken_ext_mcp::HttpTransportBuilder::new(default_route)
        .credential(awaken_ext_mcp::Credential::None)
        .connect()
        .await
        .expect("H31 initialize through the default Worker relay");
    assert_eq!(transport.list_tools().await.unwrap().len(), 1, "H31");
    let result = transport
        .call_tool("echo", serde_json::json!({"value": "worker-held"}))
        .await
        .expect("H31 tools/call");
    assert_eq!(
        serde_json::to_value(result).unwrap()["content"][0]["text"],
        "worker-held",
        "H31"
    );
    {
        let seen = seen.lock().unwrap();
        assert!(seen.len() > seen_before, "H31 route reached upstream");
        assert!(
            seen[seen_before..]
                .iter()
                .all(|(_, bearer)| bearer == "Bearer published-mcp-token"),
            "H31 every relay request uses only Worker-held material: {seen:?}"
        );
    }

    let backend_owned_snapshot =
        awaken_runtime_contract::ExecutableAgentSnapshot::builder("assistant")
            .resolved_model(
                awaken_runtime_contract::resolved::ResolvedModelCandidate::try_backend_owned(
                    awaken_runtime_contract::resolved::ModelBinding::new("local", "", "acp:test"),
                    awaken_runtime_contract::CredentialRef {
                        id: "backend-login".into(),
                        revision: 1,
                    },
                    awaken_runtime_contract::resolved::BackendModelSelection::Default,
                    "test",
                    "sha256:test-capability",
                    Default::default(),
                )
                .expect("coherent BackendOwned fixture"),
            )
            .build();

    let backend_relay_reservation = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let backend_relay_port = backend_relay_reservation.local_addr().unwrap().port();
    drop(backend_relay_reservation);
    let mut backend_deployment = crate::DeploymentConfig::ephemeral();
    backend_deployment.sandbox.container_mcp_relay = Some(
        awaken_session_mcp::ContainerMcpRelayExposure::new(
            format!("0.0.0.0:{backend_relay_port}").parse().unwrap(),
            format!("http://{relay_host}:{backend_relay_port}"),
        )
        .unwrap(),
    );
    let mut backend_secure_host = (crate::HostDependencies::ephemeral_with_deployment(
        Arc::new(OkModel),
        "stub",
        backend_deployment,
    )
    .await
    .with_mcp_relay_outbound_proxy(Arc::new(StagingOnlyProxy))
    .with_acp(executor.clone()))
    .with_credentials(credentials.clone(), secrets.clone());
    backend_secure_host.env.backend_owned_session_provider = Some(
        awaken_session_environment::SandboxEnvironmentProvider::container(
            Arc::new(SecureExternalProvider),
            Vec::new(),
            Arc::new(UnusedHandFactory),
            "/bin/sh",
        )
        .into_session_provider(),
    );
    let backend_secure_host = (backend_secure_host).build().unwrap();
    backend_secure_host.register_thread_backend_projection("mcp-acp-backend-secure", "acp:test");
    backend_secure_host
        .install_environment_projection(
            "mcp-acp-backend-secure",
            &session_environment(
                awaken_session_configuration_contract::SessionNetworkPolicy::Allowlist {
                    hosts: vec![relay_host.into()],
                },
                serde_json::json!({}),
            ),
        )
        .unwrap();
    backend_secure_host
        .prepare_container_mcp_relay()
        .await
        .expect("H26 external relay transport is bound before staging");
    backend_secure_host.register_thread_agent_projection("mcp-acp-backend-secure", "assistant");
    backend_secure_host
        .realizer
        .publication
        .retain_session_publication("mcp-acp-backend-secure", Some(&backend_owned_snapshot))
        .expect("H26 frozen BackendOwned publication");
    let backend_secure = crate::ManagedHost::new(backend_secure_host.clone());
    backend_secure
        .stage_mcp_attachment(request("mcp-acp-backend-secure", "workspace-a", 1))
        .await
        .expect("H26 selected BackendOwned provider admits WorkerRelay");

    let weak_backend_root = tempfile::tempdir().expect("H27 weak BackendOwned root");
    let mut default_secure_host = crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_acp(executor.clone())
        .with_session_container_provider(
            Arc::new(SecureExternalProvider),
            Arc::new(UnusedHandFactory),
        );
    default_secure_host.env.backend_owned_session_provider = Some(
        awaken_session_environment::SandboxEnvironmentProvider::workdir(weak_backend_root.path())
            .into_session_provider(),
    );
    let default_secure_host = (default_secure_host).build().unwrap();
    default_secure_host.register_thread_backend_projection("mcp-acp-backend-weak", "acp:test");
    default_secure_host.register_thread_agent_projection("mcp-acp-backend-weak", "assistant");
    default_secure_host
        .realizer
        .publication
        .retain_session_publication("mcp-acp-backend-weak", Some(&backend_owned_snapshot))
        .expect("H27 frozen BackendOwned publication");
    let error = crate::ManagedHost::new(default_secure_host.clone())
        .stage_mcp_attachment(request("mcp-acp-backend-weak", "workspace-a", 1))
        .await
        .expect_err("H27 selected BackendOwned provider must reject WorkerRelay");
    assert_eq!(error.code, "mcp_holder_unsupported", "H27");
    assert!(
        default_secure_host
            .mcp_projection(&generation("mcp-acp-backend-weak"))
            .is_none(),
        "H27 rejects before materialization"
    );

    let secure_managed = crate::ManagedHost::new(secure_acp_host.clone());
    let secure_generation = generation("mcp-acp-secure");
    // H20 mirrors the production Control-installed root before projecting the
    // opaque external route. H31 owns the retained real relay/upstream call.
    secure_acp_host
        .install_session_realization_lease(
            secure_generation.session_id().as_str(),
            crate::session_resource::SessionRealizationLease {
                owner: "worker-a".into(),
                runtime_incarnation: secure_generation.runtime_incarnation().to_owned(),
                epoch: secure_generation.lease_epoch().get(),
                expires_at_unix_ms: secure_generation.lease_expires_at_unix_ms(),
            },
        )
        .unwrap();
    let secure_receipt = secure_managed
        .stage_mcp_attachment(request("mcp-acp-secure", "workspace-a", 1))
        .await
        .expect("H20 complete provider evidence admits exact Worker relay staging");
    assert_eq!(
        secure_receipt.actual_realization_kind,
        Some(awaken_credential_contract::CredentialRealizationKind::WorkerRelay),
        "H20"
    );
    secure_managed
        .publish_mcp_generation(secure_generation.clone())
        .await
        .expect("H20 publish exact staged route");
    let projection = secure_acp_host
        .mcp_projection(&secure_generation)
        .expect("H20 exact projection");
    let projected = awaken_session_mcp::project_mcp_session_transport(
        projection.server.as_ref().expect("H20 private material"),
        &projection.request,
        secure_acp_host.mcp.relay(),
        &projection.receipt,
        awaken_run_executor_acp::acp_adapter_policy(),
        "codex",
    )
    .expect("H20 project opaque route");
    let route = projected.url.expect("H20 expected HTTP relay route");
    assert!(projected.auth.is_none(), "H20 relay owns auth");
    assert!(
        !route.contains("published-mcp-token"),
        "H20 secret-free route"
    );
    assert!(
        !route.contains(&mcp_url),
        "H20 original target is not exposed"
    );
    assert!(
        route.starts_with(&format!("http://{relay_host}:{relay_port}/")),
        "H20 projects only the prepared external transport origin: {route}"
    );
    secure_acp_host
        .mcp
        .relay()
        .expect("H32 prepared relay")
        .abort_for_test();
    for _ in 0..100 {
        if !secure_acp_host.mcp.relay().unwrap().is_live() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        secure_managed
            .stage_mcp_attachment(request("mcp-acp-dead-relay", "workspace-a", 1))
            .await
            .unwrap_err()
            .code,
        "mcp_relay_unavailable",
        "H32"
    );

    // H23 proves the distinct ClientInjection branch. The holder and mechanism
    // are exact, no relay route is manufactured, and raw material exists only
    // in the process-local Session field consumed by the declared ACP adapter.
    let mut refreshable = request("mcp-acp-refresh", "workspace-a", 1);
    refreshable.selected_plaintext_holder = Some(workload_holder.clone());
    let access = refreshable.credential.take().unwrap();
    refreshable.credential = Some(access.with_refresh(
        awaken_runtime_contract::CredentialRefreshAccess::new(
            1,
            "https://auth.example/token".into(),
            "client".into(),
            awaken_runtime_contract::TokenEndpointAuth::None,
            None,
            "refresh-ref".into(),
            "access-ref".into(),
            None,
            None,
        ),
    ));
    refreshable.credential.as_mut().unwrap().policy =
        CredentialExecutionPolicy::exact(workload_holder.clone(), ModelExposurePolicy::VirtualOnly);
    assert_eq!(
        secure_managed
            .stage_mcp_attachment(refreshable)
            .await
            .unwrap_err()
            .code,
        "mcp_client_refresh_unsupported",
        "H24 refresh is never silently discarded"
    );
    assert!(
        secure_acp_host
            .mcp_projection(&generation("mcp-acp-refresh"))
            .is_none(),
        "H24"
    );
    let mut client_request = request("mcp-acp-client", "workspace-a", 1);
    client_request.selected_plaintext_holder = Some(workload_holder.clone());
    client_request.credential.as_mut().unwrap().policy =
        CredentialExecutionPolicy::exact(workload_holder, ModelExposurePolicy::VirtualOnly);
    let client_receipt = secure_managed
        .stage_mcp_attachment(client_request)
        .await
        .expect("H23 exact process-protocol client injection");
    assert_eq!(
        client_receipt.actual_realization_kind,
        Some(awaken_credential_contract::CredentialRealizationKind::ProcessProtocolField),
        "H23"
    );
    secure_managed
        .publish_mcp_generation(client_receipt.generation.clone())
        .await
        .expect("H23 publish");
    let client_projection = secure_acp_host
        .mcp_projection(&client_receipt.generation)
        .expect("H23 projection");
    let client_server = awaken_session_mcp::project_mcp_session_transport(
        client_projection
            .server
            .as_ref()
            .expect("H23 private material"),
        &client_projection.request,
        secure_acp_host.mcp.relay(),
        &client_projection.receipt,
        awaken_run_executor_acp::acp_adapter_policy(),
        "claude",
    )
    .expect("H23 exact ACP Session projection");
    assert_eq!(client_server.url.as_deref(), Some(mcp_url.as_str()), "H23");
    assert_eq!(
        client_server
            .auth
            .as_ref()
            .map(|(name, value)| (name.as_str(), value.as_str())),
        Some(("Authorization", "Bearer published-mcp-token")),
        "H23"
    );
    assert!(
        !format!("{client_server:?}").contains("published-mcp-token"),
        "H23"
    );
}

#[tokio::test]
async fn published_mcp_envelope_uses_the_authoring_target_binding_at_realization() {
    use awaken_credential_contract::{
        CredentialMaterialError, CredentialMaterialRequest, CredentialMaterialResolver,
        ResolvedCredentialMaterial,
    };
    use awaken_runtime_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
    };
    use awaken_session_execution_contract::McpAttachmentRealizer;

    struct ExactTargetResolver {
        expected: awaken_credential_contract::CredentialMaterialBinding,
        holder: PlaintextHolder,
    }

    #[async_trait::async_trait]
    impl CredentialMaterialResolver for ExactTargetResolver {
        fn material_capabilities(
            &self,
        ) -> std::collections::BTreeSet<awaken_credential_contract::CredentialMaterialCapability>
        {
            let mut profiles = std::collections::BTreeSet::new();
            for holder in [self.holder.clone()] {
                for source in
                    [awaken_runtime_contract::CredentialMaterialSource::ControlPlaneReference]
                {
                    for envelope in [false, true] {
                        profiles.insert(awaken_credential_contract::CredentialMaterialCapability {
                            holder: holder.clone(),
                            source,
                            envelope,
                        });
                    }
                }
            }
            profiles
        }

        async fn resolve_exact(
            &self,
            request: CredentialMaterialRequest<'_>,
        ) -> Result<ResolvedCredentialMaterial, CredentialMaterialError> {
            if request.binding != &self.expected {
                return Err(CredentialMaterialError::BindingMismatch);
            }
            Ok(ResolvedCredentialMaterial {
                credential: request.access.credential.clone(),
                holder: self.holder.clone(),
                material: awaken_credential_contract::CredentialMaterial::secret(
                    awaken_agent_contract::RedactedString::new("mcp-bearer"),
                ),
            })
        }
    }

    let workspace = "workspace-a";
    let (mcp_url, _seen) = crate::test_mcp::start(Some("Bearer mcp-bearer")).await;
    let target =
        awaken_agent_contract::McpTarget::parse_http(&mcp_url).expect("canonical MCP target");
    let holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    let usage = CredentialUsage::HttpHeader {
        name: "authorization".into(),
        scheme: Some("Bearer".into()),
    };
    let access = CredentialAccess::new(
        CredentialRef {
            id: "flow-mcp".into(),
            revision: 1,
        },
        CredentialMaterialSource::ControlPlaneReference,
        usage.clone(),
        CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::VirtualOnly),
    )
    .with_target(awaken_runtime_contract::CredentialTarget::new(
        awaken_credential_contract::CredentialPurpose::McpAuthorization,
        awaken_agent_contract::McpTarget::identity(target.http_url().unwrap())
            .unwrap()
            .canonical_url(),
    ));
    let resolver = Arc::new(ExactTargetResolver {
        expected: awaken_credential_contract::CredentialMaterialBinding::for_target(
            workspace, &target, &usage,
        ),
        holder: holder.clone(),
    });
    let host = ((crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .with_credential_materializer(
            awaken_credential_materializer::PinnedCredentialMaterializer::external_only(
                resolver,
                awaken_time::system_clock(),
            ),
        ))
    .build()
    .expect("validated Host dependencies");
    let managed = crate::ManagedHost::new(host);

    // Cause/effect decision table:
    // | Rule | authoring binding | runtime server label | runtime target | Effect |
    // | R1 | exact target | arbitrary display name | same target | stage succeeds |
    // The display name is not credential authority and must not enter the
    // recipient-bound payload fingerprint produced during Session authoring.
    managed
        .stage_mcp_attachment(crate::session_mcp::StageMcpAttachment {
            workspace_id: workspace.into(),
            generation: crate::test_mcp::mcp_fence(
                "session-a",
                "flow",
                1,
                "runtime-a",
                1,
                u64::MAX,
            ),
            realization_id: "realize-flow".into(),
            stage_idempotency_key: "stage-flow".into(),
            name: "display-name-not-in-binding".into(),
            target,
            credential: Some(access),
            prompts_as_skills: false,
            selected_plaintext_holder: Some(holder),
        })
        .await
        .expect("R1 exact authoring target binding");
}

#[tokio::test]
async fn dispatch_session_runtime_is_constructed_once_with_weak_host_ownership() {
    // Causes: C1 complete selected dependencies; C2 empty Workspace; C3 all
    // strong Host views dropped. R1 C1 => dispatch and Managed views share Host;
    // R2 C2 => typed construction refusal; R3 C3 => Weak adapter refuses access.
    // No installation, configuration mirror, or retained Host cycle exists.
    let host = (crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .build()
        .unwrap();
    let dispatch = host.session_effects();
    let managed = dispatch.managed().unwrap();
    assert!(Arc::ptr_eq(&host, &managed.host), "R1");
    let weak = Arc::downgrade(&host);
    drop(managed);
    drop(host);
    assert!(weak.upgrade().is_none(), "R3 no cycle");
    assert!(dispatch.managed().is_err(), "R3");
    let invalid = crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub")
        .await
        .with_local_workspace("");
    assert!((invalid).build().is_err(), "R2");
}

/// Public-realizer selection cause graph:
///
/// C1 an external realizer is installed -> every MCP lifecycle effect is sent
/// to it. C2 that realizer rejects staging -> the error is terminal and the
/// local Host projection remains untouched. Without C1, the canonical local
/// Host realizer remains the sole implementation.
///
/// | Rule | external | external stage | Expected path | Local fallback |
/// |---|---|---|---|---|
/// | R1 | yes | success | external stage/publish/drain | never |
/// | R2 | yes | failure | exact external error | never |
#[tokio::test]
async fn injected_mcp_realizer_is_exclusive_and_fails_without_local_fallback() {
    use awaken_session_execution_contract::McpAttachmentRealizer;
    use {
        awaken_agent_contract::McpTarget,
        awaken_session_configuration_contract::mcp::{
            McpGenerationRef, McpRealizationReceipt, StageMcpAttachment,
        },
    };

    #[derive(Default)]
    struct RecordingRealizer {
        calls: std::sync::Mutex<Vec<&'static str>>,
        fail_stage: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl McpAttachmentRealizer for RecordingRealizer {
        async fn stage_mcp_attachment(
            &self,
            request: StageMcpAttachment,
        ) -> Result<McpRealizationReceipt, awaken_session_types::RunError> {
            self.calls.lock().unwrap().push("stage");
            if self.fail_stage.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(awaken_session_types::RunError::classified(
                    "external_stage_rejected",
                    "external realizer rejected stage",
                ));
            }
            let receipt_fingerprint = request.fingerprint();
            Ok(McpRealizationReceipt {
                generation: request.generation,
                realization_id: request.realization_id,
                selected_plaintext_holder: request.selected_plaintext_holder,
                actual_realization_kind: None,
                receipt_fingerprint,
            })
        }

        async fn publish_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), awaken_session_types::RunError> {
            self.calls.lock().unwrap().push("publish");
            Ok(())
        }

        async fn drain_mcp_generation(
            &self,
            _generation: McpGenerationRef,
        ) -> Result<(), awaken_session_types::RunError> {
            self.calls.lock().unwrap().push("drain");
            Ok(())
        }
    }

    let external = Arc::new(RecordingRealizer::default());
    let host = ((crate::HostDependencies::ephemeral(Arc::new(OkModel), "stub").await)
        .with_session_execution(
            crate::SessionExecutionDependencies::default()
                .with_mcp_attachment_realizer(external.clone()),
        ))
    .build()
    .expect("validated Host dependencies");

    let _managed = crate::ManagedHost::new(host.clone());
    let generation =
        crate::test_mcp::mcp_fence("external-mcp", "docs", 1, "runtime-1", 1, u64::MAX);
    let request = StageMcpAttachment {
        workspace_id: "workspace-a".into(),
        generation: generation.clone(),
        realization_id: "realization-1".into(),
        stage_idempotency_key: "stage-1".into(),
        name: "docs".into(),
        target: McpTarget::parse_http("https://mcp.example.test/sse").unwrap(),
        credential: None,
        prompts_as_skills: false,
        selected_plaintext_holder: None,
    };

    host.session_effects()
        .stage_mcp_attachment(request.clone())
        .await
        .expect("R1");
    host.session_effects()
        .publish_mcp_generation(generation.clone())
        .await
        .expect("R1");
    host.session_effects()
        .drain_mcp_generation(generation.clone())
        .await
        .expect("R1");
    assert_eq!(
        *external.calls.lock().unwrap(),
        ["stage", "publish", "drain"]
    );
    assert!(
        host.mcp_projection(&generation).is_none(),
        "R1 no local path"
    );

    external
        .fail_stage
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let error = host
        .session_effects()
        .stage_mcp_attachment(request)
        .await
        .expect_err("R2 external rejection is terminal");
    assert_eq!(error.code, "external_stage_rejected");
    assert!(host.mcp_projection(&generation).is_none(), "R2 no fallback");
}
