//! Shared native Session roots for File transport fixtures; no authority double.
use awaken_dispatch_testkit::worker_http as support;
use awaken_worker_contract::WorkerIdentity;

pub(super) fn resources(
    file_id: &str,
) -> awaken_session_configuration_contract::resource::ResolvedSessionResources {
    awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
        vec![
            awaken_session_configuration_contract::resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::new("file-binding"),
                source:
                    awaken_session_configuration_contract::resource::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from(file_id),
                    },
                mount_path: "/workspace/input.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            },
        ],
        Vec::new(),
    )
    .unwrap()
}

pub(super) fn frozen_application_session(
    session_id: &str,
    file_id: &str,
    identity: &WorkerIdentity,
) -> awaken_session_contract::PersistedSession {
    let environment = awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "env-worker".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: true,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "env-1".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    };
    let mut resource_state = awaken_session_contract::SessionResourceState::default();
    resource_state
        .prepare(
            &awaken_session_types::SessionId::from(session_id),
            resources(file_id),
        )
        .expect("prepare Worker Session resources");
    let mut legacy = awaken_session_contract::PersistedSession::frozen_with_budget(
        session_id,
        awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment,
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Worker,
                mcp_authoring: Default::default(),
                agent_id: "agent".into(),
                agent_revision: None,
                model_override: None,
                model: "model".into(),
                runtime: None,
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        ),
        resource_state,
        Default::default(),
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    legacy.execution = awaken_session_contract::SessionExecutionState::Running;
    legacy.realization = Some(
        awaken_session_configuration_contract::resource::SessionRealizationLease {
            owner: identity.worker_id.clone(),
            runtime_incarnation: identity.lease_owner(),
            epoch: 1,
            expires_at_unix_ms: support::unix_now_ms().saturating_add(60_000),
        },
    );
    // This fixture models the pre-cutover missing field, not a fresh creation.
    let mut wire = serde_json::to_value(legacy).unwrap();
    wire.as_object_mut()
        .unwrap()
        .remove("environment_materialization");
    serde_json::from_value(wire).unwrap()
}
