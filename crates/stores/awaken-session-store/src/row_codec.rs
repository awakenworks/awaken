//! Canonical aggregate decoding and the one-way migration from retained legacy
//! Session columns. Backends share this codec so SQLite and Postgres cannot
//! interpret the same durable row differently.

use awaken_credential_contract::{
    CredentialRealizationProfile, PlaintextBoundary, PlaintextHolder,
};
use awaken_session_contract::{
    EnvironmentFingerprint, EnvironmentSnapshot, McpAttachmentDraft, McpAttachmentOrigin,
    McpAttachmentState, McpTarget, PersistedSession, ResolvedSessionResources, SessionBaseline,
    SessionBaselineState, SessionMcpAttachmentSet, SessionMcpAuthoringContext,
    SessionNetworkPolicy, SessionResourceState, SessionRevision,
};

pub(super) struct EncodedSessionRow {
    pub aggregate_json: Option<String>,
    pub session_id: String,
    pub agent_id: String,
    pub model: String,
    pub title: Option<String>,
    pub metadata_json: String,
    pub environment_id: String,
    pub environment_binding: Option<String>,
    pub runtime_json: String,
    pub effective_inputs_json: String,
    pub status: String,
    pub archived_at: Option<String>,
    pub revision: i64,
}

#[derive(serde::Deserialize)]
struct LegacyPersistedSessionRuntime {
    #[serde(default)]
    mcp_servers: Vec<LegacyMcpServerBinding>,
    #[serde(default)]
    delegate_ids: Vec<String>,
    runtime: Option<String>,
    #[serde(default)]
    deny_egress: bool,
    sandbox: Option<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct LegacyMcpServerBinding {
    name: String,
    url: String,
    credential_source_id: Option<String>,
    credential_revision: Option<u64>,
    refresh: Option<LegacyMcpRefreshBinding>,
}

#[derive(serde::Deserialize)]
struct LegacyMcpRefreshBinding {
    token_endpoint: String,
    client_id: String,
    refresh_token_ref: String,
    token_endpoint_auth: LegacyTokenEndpointAuthBinding,
    scope: Option<String>,
    resource: Option<String>,
}

#[derive(serde::Deserialize)]
enum LegacyTokenEndpointAuthBinding {
    None,
    ClientSecretBasic { secret_ref: String },
    ClientSecretPost { secret_ref: String },
}

fn decode_resource_state(data: &str) -> Result<SessionResourceState, serde_json::Error> {
    let value: serde_json::Value = serde_json::from_str(data)?;
    if value.get("inputs").is_some() {
        return serde_json::from_value::<ResolvedSessionResources>(value)
            .map(SessionResourceState::from_legacy);
    }
    serde_json::from_value(value)
}

pub(super) fn decode(row: EncodedSessionRow) -> Result<PersistedSession, serde_json::Error> {
    let revision = SessionRevision(
        u64::try_from(row.revision).expect("managed Session revision is non-negative"),
    );
    if let Some(aggregate_json) = row.aggregate_json {
        let mut aggregate: PersistedSession = serde_json::from_str(&aggregate_json)?;
        aggregate.revision = revision;
        return Ok(aggregate);
    }
    let runtime: LegacyPersistedSessionRuntime = serde_json::from_str(&row.runtime_json)?;
    let resources = decode_resource_state(&row.effective_inputs_json)?;
    let holder = if runtime
        .runtime
        .as_deref()
        .is_some_and(|runtime| runtime.starts_with("acp:"))
    {
        PlaintextHolder::new(PlaintextBoundary::Workload, "awaken.workload.acp")
    } else {
        PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker")
    };
    let mcp_holder = PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker");
    let credential_realization = CredentialRealizationProfile {
        inference_holder: holder,
        mcp_holder: mcp_holder.clone(),
        resource_holder: mcp_holder.clone(),
    };
    let sandbox = runtime.sandbox.unwrap_or_else(|| serde_json::json!({}));
    let network = if runtime.deny_egress {
        SessionNetworkPolicy::None
    } else {
        SessionNetworkPolicy::Unrestricted
    };
    let packages = awaken_session_contract::env_registry::EnvironmentPackages::default();
    let environment = EnvironmentSnapshot {
        environment_id: row.environment_id,
        revision: awaken_session_contract::env_registry::EnvironmentRevision(0),
        config_fingerprint: EnvironmentFingerprint(awaken_session_contract::stable_fingerprint(&(
            &sandbox,
            &packages,
            &network,
            &credential_realization,
        ))),
        sandbox,
        sandbox_provisioning: Default::default(),
        packages,
        network,
        credential_realization,
    };
    let baseline = SessionBaseline::compile(awaken_session_contract::SessionBaselineInputs {
        environment,
        mcp_authoring: SessionMcpAuthoringContext::default(),
        agent_id: row.agent_id,
        model: row.model,
        runtime: runtime.runtime,
        application: None,
        delegate_ids: runtime.delegate_ids,
        toolsets: Vec::new(),
        mounts: Vec::new(),
        env: Vec::new(),
        prompts: Vec::new(),
    });
    let mut drafts = Vec::with_capacity(runtime.mcp_servers.len());
    for server in runtime.mcp_servers {
        let credential = match (server.credential_source_id, server.credential_revision) {
            (None, None) => None,
            (Some(id), Some(revision)) => {
                Some(legacy_credential_access(id, revision, server.refresh))
            }
            _ => {
                return Err(<serde_json::Error as serde::de::Error>::custom(
                    "legacy protected MCP attachment has no exact credential revision",
                ));
            }
        };
        drafts.push(McpAttachmentDraft {
            name: server.name,
            target: McpTarget::parse_http(server.url).map_err(|error| {
                <serde_json::Error as serde::de::Error>::custom(error.to_string())
            })?,
            prompts_as_skills: false,
            credential,
            origin: McpAttachmentOrigin::Session,
        });
    }
    let mut mcp = SessionMcpAttachmentSet::from_initial(drafts, Some(mcp_holder))
        .map_err(<serde_json::Error as serde::de::Error>::custom)?;
    for attachment in &mut mcp.attachments {
        attachment.state = McpAttachmentState::Active;
    }
    Ok(PersistedSession {
        session_id: row.session_id,
        revision,
        baseline: SessionBaselineState::Frozen(baseline),
        title: row.title,
        metadata: serde_json::from_str(&row.metadata_json)?,
        agent_tools: Vec::new(),
        environment_binding: row.environment_binding,
        mcp,
        resources,
        realization: None,
        status: row.status,
        archived_at: row.archived_at,
    })
}

fn legacy_credential_access(
    id: String,
    revision: u64,
    refresh: Option<LegacyMcpRefreshBinding>,
) -> awaken_credential_contract::CredentialAccess {
    use awaken_credential_contract::{
        CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
        CredentialRefreshAccess, CredentialUsage, TokenEndpointAuth,
    };

    let mut access = CredentialAccess::new(
        CredentialRef { id, revision },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
        CredentialExecutionPolicy::self_hosted_provider(),
    );
    if let Some(refresh) = refresh {
        let (token_endpoint_auth, client_secret_ref) = match refresh.token_endpoint_auth {
            LegacyTokenEndpointAuthBinding::None => (TokenEndpointAuth::None, None),
            LegacyTokenEndpointAuthBinding::ClientSecretBasic { secret_ref } => {
                (TokenEndpointAuth::ClientSecretBasic, Some(secret_ref))
            }
            LegacyTokenEndpointAuthBinding::ClientSecretPost { secret_ref } => {
                (TokenEndpointAuth::ClientSecretPost, Some(secret_ref))
            }
        };
        access = access.with_refresh(CredentialRefreshAccess::new(
            revision,
            refresh.token_endpoint,
            refresh.client_id,
            token_endpoint_auth,
            client_secret_ref,
            refresh.refresh_token_ref,
            format!("credential:{revision}:access"),
            refresh.scope,
            refresh.resource,
        ));
    }
    access
}
