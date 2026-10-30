//! Persisted-Session fixtures and Session-application test ports shared by the
//! Managed adapter's state tests and the Coordinator's Session adoption tests.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_session_contract::PersistedSession;
use awaken_session_types::SessionId;

pub struct RecordingProjectionRefresh {
    pub calls: std::sync::atomic::AtomicUsize,
    pub fail: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl awaken_session_application_contract::ExecutableAgentProjectionRefresh
    for RecordingProjectionRefresh
{
    async fn refresh_agent_projection(&self) -> Result<(), String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err("secret-projection-canary".into())
        } else {
            Ok(())
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_application_contract::ExecutableEnvironmentProjectionRefresh
    for RecordingProjectionRefresh
{
    async fn refresh_environment_projection(&self) -> Result<(), String> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            Err("secret-projection-canary".into())
        } else {
            Ok(())
        }
    }
}

pub fn paired_projection_refresh(
    refresh: Arc<RecordingProjectionRefresh>,
) -> Arc<awaken_session_application_contract::ExecutableProjectionRefresh> {
    Arc::new(
        awaken_session_application_contract::ExecutableProjectionRefresh::new(
            refresh.clone(),
            refresh,
        ),
    )
}

pub fn sample_persisted(id: &str) -> PersistedSession {
    let mut metadata = BTreeMap::new();
    metadata.insert("team".to_string(), "research".to_string());
    let holder = awaken_credential_contract::PlaintextHolder::new(
        awaken_credential_contract::PlaintextBoundary::Workload,
        "awaken.workload.acp",
    );
    let environment = awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "env_local".into(),
        revision: awaken_environment_contract::EnvironmentRevision(1),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "env-1".into(),
        ),
        sandbox: awaken_provisioning_contract::SandboxOverride {
            isolation: Some(awaken_provisioning_contract::IsolationClass::Namespace),
            ..Default::default()
        },
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::None,
        credential_realization: awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: holder.clone(),
            mcp_holder: holder.clone(),
            resource_holder: holder,
        },
    };
    let mut mcp =
        awaken_session_configuration_contract::mcp::SessionMcpAttachmentSet::from_initial(
            vec![
                awaken_session_configuration_contract::mcp::McpAttachmentDraft {
                    name: "calc".into(),
                    target: awaken_agent_contract::McpTarget::parse_http("https://x").unwrap(),
                    credential: None,
                    prompts_as_skills: false,
                    origin:
                        awaken_session_configuration_contract::mcp::McpAttachmentOrigin::Session,
                },
            ],
            None,
        )
        .unwrap();
    mcp.attachments[0].state =
        awaken_session_configuration_contract::mcp::McpAttachmentState::Active;
    let mut legacy = PersistedSession::frozen_with_budget(
        SessionId::from(id),
        awaken_session_configuration_contract::SessionBaseline::compile(
            awaken_session_configuration_contract::SessionBaselineInputs {
                environment,
                runtime_placement: awaken_session_types::SessionRuntimePlacement::Local,
                mcp_authoring: Default::default(),
                agent_id: "coder".into(),
                agent_revision: None,
                model_override: None,
                model: "kimi-k2".into(),
                runtime: Some("acp:custom".into()),
                delegate_ids: Vec::new(),
                toolsets: Vec::new(),
                mounts: Vec::new(),
                env: Vec::new(),
                prompts: Vec::new(),
                transcript_prefix: None,
            },
        ),
        awaken_session_contract::SessionResourceState::from_active(sample_inputs()),
        mcp,
        Some("My session".to_string()),
        metadata,
        Default::default(),
        Default::default(),
    );
    legacy.execution = Default::default();
    // This fixture models the pre-cutover missing field, not a fresh creation.
    let mut wire = serde_json::to_value(legacy).unwrap();
    wire.as_object_mut()
        .unwrap()
        .remove("environment_materialization");
    serde_json::from_value(wire).unwrap()
}

pub fn sample_inputs() -> awaken_session_configuration_contract::resource::ResolvedSessionResources
{
    awaken_session_configuration_contract::resource::ResolvedSessionResources::try_new(
        vec![
            awaken_session_configuration_contract::resource::ResolvedInput {
                created_at_unix_ms: None,
                updated_at_unix_ms: None,
                binding_id: awaken_resource_contract::BindingId::from("input-file"),
                source:
                    awaken_session_configuration_contract::resource::ResolvedInputSource::File {
                        file_id: awaken_resource_contract::FileId::from("file-hash"),
                    },
                mount_path: "/input.txt".into(),
                access: awaken_resource_contract::ResourceAccess::ReadOnly,
                instructions: None,
            },
        ],
        Vec::new(),
    )
    .unwrap()
}

pub fn persisted_with_mcp_source(
    session_id: &str,
    source_id: &str,
    authored_vault_id: &str,
) -> PersistedSession {
    let mut session = sample_persisted(session_id);
    session.mcp.attachments[0].credential = Some(
        awaken_credential_contract::CredentialAccess::new(
            awaken_credential_contract::CredentialRef {
                id: source_id.into(),
                revision: 1,
            },
            awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_credential_contract::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            awaken_credential_contract::CredentialExecutionPolicy::self_hosted_provider(),
        )
        .with_target(awaken_credential_contract::CredentialTarget::new(
            awaken_credential_contract::CredentialPurpose::McpAuthorization,
            "https://x",
        )),
    );
    let awaken_session_configuration_contract::SessionBaselineState::Frozen(baseline) =
        &mut session.baseline
    else {
        unreachable!("fixture baseline is frozen")
    };
    baseline.mcp_authoring.ordered_vault_ids = vec![authored_vault_id.into()];
    session
}

/// Frozen, known-unattempted preparation used by both credential adoption and
/// the real registered-Worker process composition. The reserved activity is
/// logical admission, never evidence of a realization or MCP effect.
pub fn preparing_session_with_mcp_source(
    with_activity: bool,
) -> awaken_session_contract::PersistedSession {
    let old = persisted_with_mcp_source("preparing", "source-a", "vault-event");
    let mut baseline = old.frozen_baseline().unwrap().clone();
    baseline.mutation_policy =
        awaken_session_configuration_contract::resource::SessionMutationPolicy::Frozen;
    baseline.runtime_placement = awaken_session_types::SessionRuntimePlacement::Worker;
    let mut mcp = old.mcp;
    mcp.attachments[0].state =
        awaken_session_configuration_contract::mcp::McpAttachmentState::Requested;
    mcp.attachments[0].publication_acknowledged = false;
    mcp.attachments[0].realization = None;
    let mut root = awaken_session_contract::PersistedSession::frozen_with_budget(
        "preparing",
        baseline,
        Default::default(),
        mcp,
        None,
        Default::default(),
        Default::default(),
        Default::default(),
    );
    root.created_at_unix_ms = Some(1);
    if with_activity {
        let mut aggregate = awaken_session_contract::SessionAggregate::try_from(root).unwrap();
        assert_eq!(
            aggregate.activity().open(
                None,
                true,
                false,
                1,
                0,
                Some((
                    "reserved-operation",
                    &awaken_agent_contract::ThreadId("preparing".into()),
                    &awaken_agent_contract::RunId("reserved-run".into())
                )),
            ),
            awaken_session_contract::SessionActivityAdmissionPlan::Open(1)
        );
        root = aggregate.into_snapshot();
    }
    root
}

pub struct RolloutCredentialSource;

#[async_trait::async_trait]
impl awaken_credential_contract::SessionCredentialSource for RolloutCredentialSource {
    async fn has_vault(&self, _workspace_id: &str, _id: &str) -> Result<bool, String> {
        Ok(true)
    }

    async fn mcp_credential_source_for_url(
        &self,
        _workspace_id: &str,
        _vault_ids: &[String],
        _url: &str,
    ) -> Result<Option<awaken_credential_contract::CredentialSourceId>, String> {
        Ok(None)
    }

    async fn mcp_access_for_source(
        &self,
        source_id: &awaken_credential_contract::CredentialSourceId,
        _workspace_id: &str,
        target: &awaken_agent_contract::McpTarget,
        selected_holder: &awaken_credential_contract::PlaintextHolder,
        _binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
        let revision = if source_id.0 == "source-a" { 2 } else { 1 };
        let audience = awaken_agent_contract::McpTarget::identity(
            target.http_url().ok_or("MCP target is not HTTP")?,
        )
        .map_err(|error| error.to_string())?
        .canonical_url();
        Ok(awaken_credential_contract::CredentialAccess::new(
            awaken_credential_contract::CredentialRef {
                id: source_id.0.clone(),
                revision,
            },
            awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
            awaken_credential_contract::CredentialUsage::HttpHeader {
                name: "authorization".into(),
                scheme: Some("Bearer".into()),
            },
            awaken_credential_contract::CredentialExecutionPolicy::exact(
                selected_holder.clone(),
                awaken_credential_contract::ModelExposurePolicy::Forbidden,
            ),
        )
        .with_target(awaken_credential_contract::CredentialTarget::new(
            awaken_credential_contract::CredentialPurpose::McpAuthorization,
            audience,
        )))
    }

    async fn credential_access_for_source(
        &self,
        _source_id: &awaken_credential_contract::CredentialSourceId,
        _workspace_id: &str,
        _request: awaken_credential_contract::SessionCredentialAccessRequest,
    ) -> Result<awaken_credential_contract::CredentialAccess, String> {
        Err("not used by MCP rollout fixture".into())
    }
}
