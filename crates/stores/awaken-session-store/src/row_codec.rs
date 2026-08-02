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
        let mut value: serde_json::Value = serde_json::from_str(&aggregate_json)?;
        // Collapse the former parallel activity state to its only independent
        // fact: the monotonic overlapping-turn fence. Public `status` remains
        // authoritative for logical lifecycle; physical Hand residency is local
        // Runtime Host state.
        if value.get("activity_epoch").is_none() {
            let epoch = value
                .as_object_mut()
                .and_then(|object| object.remove("activity"))
                .and_then(|activity| activity.get("epoch").and_then(serde_json::Value::as_u64))
                .unwrap_or_default();
            value
                .as_object_mut()
                .expect("Session aggregate is an object")
                .insert("activity_epoch".into(), serde_json::json!(epoch));
        }
        // One-way migration from the former nullable binding. The canonical
        // aggregate now owns a typed environment phase; retained SQL columns are
        // read only for pre-aggregate rows and never become a second write path.
        if value.get("environment").is_none() {
            let legacy_binding = value
                .as_object_mut()
                .and_then(|object| object.remove("environment_binding"));
            let environment = match legacy_binding {
                Some(serde_json::Value::String(binding)) => {
                    awaken_session_contract::SessionEnvironmentState::Resident { binding }
                }
                _ => awaken_session_contract::SessionEnvironmentState::Unmaterialized,
            };
            value
                .as_object_mut()
                .expect("Session aggregate is an object")
                .insert(
                    "environment".into(),
                    serde_json::to_value(environment)
                        .expect("Session environment state serializes"),
                );
        }
        if value.get("tools").is_none()
            && let Some(agent_tools) = value
                .as_object_mut()
                .and_then(|object| object.remove("agent_tools"))
        {
            let legacy: Vec<awaken_session_contract::AgentTool> =
                serde_json::from_value(agent_tools)?;
            let toolsets = awaken_session_contract::toolset_policies(&legacy);
            let client_tools = legacy
                .into_iter()
                .filter_map(|tool| match tool {
                    awaken_session_contract::AgentTool::Custom {
                        name,
                        description,
                        input_schema,
                    } => Some(awaken_agent_contract::ClientToolDescriptor {
                        name,
                        description,
                        input_schema: serde_json::to_value(input_schema)
                            .expect("legacy custom tool schema serializes"),
                    }),
                    awaken_session_contract::AgentTool::AgentToolset20260401 { .. }
                    | awaken_session_contract::AgentTool::McpToolset { .. } => None,
                })
                .collect();
            value
                .as_object_mut()
                .expect("Session aggregate is an object")
                .insert(
                    "tools".into(),
                    serde_json::to_value(awaken_session_contract::SessionToolConfiguration {
                        toolsets,
                        client_tools,
                    })
                    .expect("neutral Session tool configuration serializes"),
                );
        }
        let mut aggregate: PersistedSession = serde_json::from_value(value)?;
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
    let packages = awaken_session_contract::EnvironmentPackages::default();
    let environment = EnvironmentSnapshot {
        environment_id: row.environment_id,
        revision: awaken_session_contract::EnvironmentRevision(0),
        self_hosted: false,
        config_fingerprint: EnvironmentFingerprint(awaken_session_contract::stable_fingerprint(&(
            &sandbox,
            &packages,
            &network,
            &credential_realization,
        ))),
        sandbox,
        sandbox_provisioning: Default::default(),
        packages,
        prepared_image: None,
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
        tools: Default::default(),
        activity_epoch: 0,
        environment: row.environment_binding.map_or(
            awaken_session_contract::SessionEnvironmentState::Unmaterialized,
            |binding| awaken_session_contract::SessionEnvironmentState::Resident { binding },
        ),
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

#[cfg(test)]
mod tests {
    use awaken_session_contract::ManagedSessionRepository as _;
    use rusqlite::params;

    use crate::{SqliteManagedSessionRepository, tests::create_fixture, tests::sample};

    /// Aggregate migration cause/effect decision table.
    /// C1=typed `environment` exists; C2=legacy aggregate binding is a string;
    /// C3=legacy parallel activity object exists with an epoch.
    /// E1=typed value remains authoritative; E2=legacy binding becomes Resident;
    /// E3=missing/null legacy binding becomes Unmaterialized; E4=only the epoch
    /// migrates and the duplicate activity state disappears. Rules: M1 C1=>E1;
    /// M2 !C1+C2=>E2; M3 !C1+!C2=>E3; M4 C3=>E4. This covers M2-M4;
    /// normal repository round-trips cover M1.
    #[tokio::test]
    async fn legacy_aggregate_environment_binding_migrates_once_to_typed_state() {
        let repo = SqliteManagedSessionRepository::open_in_memory().unwrap();
        create_fixture(&repo, "default", sample("legacy-bound"), Vec::new()).await;
        create_fixture(&repo, "default", sample("legacy-unbound"), Vec::new()).await;
        for (id, binding) in [
            ("legacy-bound", serde_json::json!("opaque-binding")),
            ("legacy-unbound", serde_json::Value::Null),
        ] {
            let mut legacy = serde_json::to_value(sample(id)).unwrap();
            let object = legacy.as_object_mut().unwrap();
            object.remove("environment");
            object.remove("activity_epoch");
            object.insert(
                "activity".into(),
                serde_json::json!({ "epoch": 41, "state": { "phase": "active" } }),
            );
            object.insert("environment_binding".into(), binding);
            repo.conn
                .lock()
                .unwrap()
                .execute(
                    "UPDATE managed_session SET aggregate_json = ?2 WHERE session_id = ?1",
                    params![id, serde_json::to_string(&legacy).unwrap()],
                )
                .unwrap();
        }

        let bound = repo.get("legacy-bound").await.unwrap();
        assert_eq!(bound.environment.binding(), Some("opaque-binding"), "M2");
        assert_eq!(bound.activity_epoch, 41, "M4");
        assert!(
            matches!(
                repo.get("legacy-unbound").await.unwrap().environment,
                awaken_session_contract::SessionEnvironmentState::Unmaterialized
            ),
            "M3"
        );
    }
}
