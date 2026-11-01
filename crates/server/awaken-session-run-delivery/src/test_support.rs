//! Frozen Session projections the crate's tests install.

use awaken_runtime_contract::ExecutableAgentSnapshot;
use awaken_runtime_contract::resolved::ModelBinding;
use awaken_session_configuration_contract::SessionBaseline;
use awaken_session_execution_contract::FrozenSessionProjection;
use awaken_session_types::{SessionRevision, SessionRuntimePlacement};

pub(crate) fn environment() -> awaken_session_configuration_contract::EnvironmentSnapshot {
    awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "dispatch-environment".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "dispatch-environment-v1".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization:
            awaken_credential_contract::CredentialRealizationProfile::self_hosted_native(),
    }
}

pub(crate) fn publication(agent: &str) -> ExecutableAgentSnapshot {
    ExecutableAgentSnapshot::builder(agent)
        .model(ModelBinding::parse("provider", "model", "native").expect("valid model"))
        .build()
}

pub(crate) fn projection(
    revision: u64,
    model: &str,
    published: Option<ExecutableAgentSnapshot>,
) -> FrozenSessionProjection {
    let baseline = SessionBaseline::compile(
        awaken_session_configuration_contract::SessionBaselineInputs {
            environment: environment(),
            runtime_placement: SessionRuntimePlacement::Worker,
            mcp_authoring: Default::default(),
            agent_id: "agent".into(),
            agent_revision: None,
            model_override: None,
            model: model.into(),
            runtime: None,
            delegate_ids: Vec::new(),
            toolsets: Vec::new(),
            mounts: Vec::new(),
            env: Vec::new(),
            prompts: Vec::new(),
            transcript_prefix: None,
        },
    );
    FrozenSessionProjection {
        bound_source_end: None,
        workspace_id: "workspace".into(),
        revision: SessionRevision(revision),
        baseline,
        agent_publication: published,
        environment_materialization: None,
        environment: Default::default(),
        resource_revision: 0,
        resources: Default::default(),
        previous_resource_manifest: None,
        tools: Default::default(),
        mcp: Vec::new(),
        request_context: Vec::new(),
    }
}
