// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Canonical actor/workspace/consumer/Agent and live execution admission surround material IO and recipient sealing; Windows HTTP/Vault E2E supplies component evidence, not a formal proof.
//! Generic application material entry and recipient-bound delivery. No UI,
//! browser, device, selector or business workflow is interpreted here.
use awaken_agent_contract::RedactedString;
use awaken_agent_contract::credential::CredentialHandlingMode;
use awaken_agent_contract::{
    credential::CredentialAccess, credential::CredentialExecutionPolicy,
    credential::CredentialPurpose, credential::CredentialTarget, credential::CredentialUsage,
    credential::ModelExposurePolicy, credential::PlaintextBoundary, credential::PlaintextHolder,
};
use awaken_credential_contract::{
    CredentialDescriptor, CredentialEnvelopeIssuance, CredentialExecutionAuthority,
    CredentialExecutionBinding, CredentialExtensionDescriptor, CredentialMaterialBinding,
    CredentialMaterialDescriptor, CredentialSourceId, CredentialTargetContract,
    SessionCredentialAccessRequest,
};
use awaken_protocol_managed::VaultState;
use awaken_protocol_managed::types::memory::AuthenticatedMemoryActor;
use awaken_resource_contract::MemoryActor;
use awaken_runtime_contract::runs::scope::WorkspaceScope;
use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    routing::post,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::Arc,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Consumer {
    id: String,
    workspace_id: String,
    holder: PlaintextHolder,
    key_file: PathBuf,
    material_types: BTreeSet<String>,
    credential_keys: BTreeSet<String>,
    authorized_actors: Vec<MemoryActor>,
    handling_mode: CredentialHandlingMode,
    authorized_agents: BTreeSet<String>,
}
#[derive(Clone)]
struct Service {
    vault: Arc<VaultState>,
    clock: Arc<dyn awaken_time::WallClock>,
    registry: PathBuf,
    execution_authority: Option<Arc<dyn CredentialExecutionAuthority>>,
}
type Error = (StatusCode, Json<serde_json::Value>);
fn denied() -> Error {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({"code":"credential_delivery_unavailable"})),
    )
}
fn valid_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_.:-".contains(&c))
}
impl Service {
    fn consumer(
        &self,
        workspace: &str,
        actor: &MemoryActor,
        id: &str,
        key: &str,
    ) -> Result<Consumer, Error> {
        let bytes = std::fs::read(&self.registry).map_err(|_| denied())?;
        let all: Vec<Consumer> = serde_json::from_slice(&bytes).map_err(|_| denied())?;
        if all.iter().filter(|c| c.id == id).count() != 1 || !valid_id(id) || !valid_id(key) {
            return Err(denied());
        }
        let c = all.into_iter().find(|c| c.id == id).ok_or_else(denied)?;
        if c.workspace_id != workspace
            || c.holder.boundary != PlaintextBoundary::Workload
            || c.material_types.is_empty()
            || !c.credential_keys.contains(key)
            || !c.authorized_actors.contains(actor)
        {
            return Err(denied());
        }
        Ok(c)
    }
}
fn target(c: &Consumer, key: &str, material_type: &str) -> (CredentialTarget, CredentialUsage) {
    (
        CredentialTarget::new(
            CredentialPurpose::Extension,
            format!("application://{}/{}", c.id, key),
        ),
        CredentialUsage::Extension {
            consumer_id: c.id.clone(),
            material_type: material_type.into(),
            public_config: serde_json::json!({}),
        },
    )
}
fn source(workspace: &str, consumer: &str, key: &str) -> CredentialSourceId {
    CredentialSourceId(format!(
        "credential:application:{:x}",
        Sha256::digest(serde_json::to_vec(&(workspace, consumer, key)).expect("strings"))
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    consumer_id: String,
    credential_key: String,
    material_type: String,
    expected_revision: Option<u64>,
    fields: BTreeMap<String, String>,
}
async fn enter(
    State(s): State<Service>,
    Extension(w): Extension<WorkspaceScope>,
    Extension(actor): Extension<AuthenticatedMemoryActor>,
    Json(cmd): Json<Entry>,
) -> Result<Json<serde_json::Value>, Error> {
    let c = s.consumer(&w.0, &actor.0, &cmd.consumer_id, &cmd.credential_key)?;
    if !c.material_types.contains(&cmd.material_type)
        || cmd.fields.is_empty()
        || cmd.fields.len() > 16
        || cmd
            .fields
            .iter()
            .any(|(k, v)| !valid_id(k) || v.is_empty() || v.len() > 16384)
    {
        return Err(denied());
    }
    let (t, u) = target(&c, &cmd.credential_key, &cmd.material_type);
    let descriptor = CredentialDescriptor::new(
        c.id.clone(),
        CredentialMaterialDescriptor::structured(
            cmd.material_type.clone(),
            cmd.fields.keys().cloned(),
        ),
        [CredentialTargetContract {
            target: t,
            usage: u,
        }],
    );
    let material = awaken_credential_vault::encode_structured_material(
        awaken_credential_vault::StructuredCredentialMaterial {
            type_id: cmd.material_type,
            fields: cmd
                .fields
                .into_iter()
                .map(|(k, v)| (k, RedactedString::new(v)))
                .collect(),
        },
    )
    .map_err(|_| denied())?;
    if let Some(expected) = cmd.expected_revision {
        let id = source(&w.0, &c.id, &cmd.credential_key);
        let revision = s
            .vault
            .application()
            .credential_access()
            .rotate_material(&id, expected, &w.0, &descriptor, material)
            .await
            .map_err(|_| denied())?;
        return Ok(Json(
            serde_json::json!({"credential_ref":{"id":id.0,"revision":revision},"created":false}),
        ));
    }
    let receipt = s
        .vault
        .application()
        .credential_access()
        .enter_material(
            source(&w.0, &c.id, &cmd.credential_key),
            &w.0,
            descriptor,
            material,
        )
        .await
        .map_err(|_| denied())?;
    Ok(Json(
        serde_json::json!({"credential_ref":receipt.credential,"created":receipt.created}),
    ))
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Delivery {
    consumer_id: String,
    credential_key: String,
    material_type: String,
    expected_revision: u64,
    operation_id: String,
    runtime_epoch: awaken_lease_contract::fence::Epoch,
    plan_hash: String,
    target: serde_json::Value,
    recipient: String,
    recipient_key_id: String,
    execution: CredentialExecutionBinding,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveExecution {
    consumer_id: String,
    credential_key: String,
    #[serde(default)]
    session_id: String,
    run_id: String,
}
async fn execution(
    State(s): State<Service>,
    Extension(w): Extension<WorkspaceScope>,
    Extension(actor): Extension<AuthenticatedMemoryActor>,
    Json(cmd): Json<ResolveExecution>,
) -> Result<Json<serde_json::Value>, Error> {
    let c = s.consumer(&w.0, &actor.0, &cmd.consumer_id, &cmd.credential_key)?;
    let execution = s
        .execution_authority
        .as_ref()
        .ok_or_else(denied)?
        .resolve_current(&w.0, &cmd.session_id, &cmd.run_id)
        .await
        .map_err(|_| denied())?;
    if !c.authorized_agents.contains(&execution.agent_id) {
        return Err(denied());
    }
    Ok(Json(
        serde_json::json!({"execution":execution,"handling_mode":c.handling_mode}),
    ))
}
async fn admitted(
    s: &Service,
    workspace: &str,
    actor: &MemoryActor,
    cmd: &Delivery,
) -> Result<(Consumer, CredentialAccess, CredentialMaterialBinding), Error> {
    let c = s.consumer(workspace, actor, &cmd.consumer_id, &cmd.credential_key)?;
    cmd.execution.validate().map_err(|_| denied())?;
    if !c.authorized_agents.contains(&cmd.execution.agent_id) {
        return Err(denied());
    }
    s.execution_authority
        .as_ref()
        .ok_or_else(denied)?
        .assert_current(workspace, &cmd.execution)
        .await
        .map_err(|_| denied())?;
    let key_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&c.key_file).map_err(|_| denied())?)
            .map_err(|_| denied())?;
    if c.holder.trust_domain.0 != cmd.recipient
        || !key_config["keys"].as_array().is_some_and(|keys| {
            keys.iter().any(|k| {
                k["id"].as_str() == Some(&cmd.recipient_key_id)
                    && k["holder"] == serde_json::to_value(&c.holder).unwrap()
            })
        })
    {
        return Err(denied());
    }
    if !c.material_types.contains(&cmd.material_type)
        || !valid_id(&cmd.operation_id)
        || cmd.expected_revision == 0
        || cmd.plan_hash.len() != 64
        || !cmd.plan_hash.bytes().all(|b| b.is_ascii_hexdigit())
        || !cmd.target.is_object()
        || serde_json::to_vec(&cmd.target).map_err(|_| denied())?.len() > 8192
    {
        return Err(denied());
    }
    let (t, u) = target(&c, &cmd.credential_key, &cmd.material_type);
    let binding = CredentialMaterialBinding::for_target(workspace, cmd, &u);
    let access = s
        .vault
        .application()
        .credential_access()
        .credential_access_for_source(
            &source(workspace, &c.id, &cmd.credential_key),
            workspace,
            SessionCredentialAccessRequest {
                target: t,
                usage: u,
                policy: CredentialExecutionPolicy {
                    handling_mode: c.handling_mode,
                    allowed_plaintext_holders: [c.holder.clone()].into_iter().collect(),
                    model_exposure: ModelExposurePolicy::Forbidden,
                },
                selected_holder: c.holder.clone(),
                binding: binding.clone(),
            },
        )
        .await
        .map_err(|_| denied())?;
    if access.credential.revision != cmd.expected_revision {
        return Err(denied());
    }
    Ok((c, access, binding))
}
async fn deliver(
    State(s): State<Service>,
    Extension(w): Extension<WorkspaceScope>,
    Extension(actor): Extension<AuthenticatedMemoryActor>,
    Json(cmd): Json<Delivery>,
) -> Result<Json<serde_json::Value>, Error> {
    let (c, access, binding) = admitted(&s, &w.0, &actor.0, &cmd).await?;
    let a = s.vault.application().credential_access();
    let original = a
        .source(&source(&w.0, &c.id, &cmd.credential_key))
        .await
        .map_err(|_| denied())?;
    let material = a.open_material(&original).await.map_err(|_| denied())?;
    // Recheck authority after secret IO; a read is not a delivery grant.
    let (_, current, _) = admitted(&s, &w.0, &actor.0, &cmd).await?;
    if current.credential != access.credential {
        return Err(denied());
    }
    let key_config: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&c.key_file).map_err(|_| denied())?)
            .map_err(|_| denied())?;
    if !key_config["ttl_seconds"]
        .as_u64()
        .is_some_and(|ttl| ttl > 0 && ttl <= 120)
    {
        return Err(denied());
    }
    let issuer =
        awaken_credential_envelope::RecipientKeyEnvelope::open(&c.key_file, s.clock.clone())
            .map_err(|_| denied())?;
    // Full context is authenticated INSIDE the ciphertext, not authorized by
    // the contract's non-cryptographic equality fingerprint alone.
    let wrapped = RedactedString::new(
        serde_json::to_string(
            &serde_json::json!({"context":cmd,"material":material.expose_secret()}),
        )
        .map_err(|_| denied())?,
    );
    let envelope = issuer
        .seal_for_consumer(
            CredentialEnvelopeIssuance {
                access: access.clone(),
                selected_holder: c.holder.clone(),
                binding: binding.clone(),
                material: wrapped,
            },
            &CredentialExtensionDescriptor {
                consumer_id: c.id.clone(),
                material_types: c.material_types,
            },
        )
        .await
        .map_err(|_| denied())?;
    admitted(&s, &w.0, &actor.0, &cmd).await?;
    Ok(Json(
        serde_json::json!({"access":access,"holder":c.holder,"binding":binding,"envelope":envelope}),
    ))
}
async fn check(
    State(s): State<Service>,
    Extension(w): Extension<WorkspaceScope>,
    Extension(actor): Extension<AuthenticatedMemoryActor>,
    Json(cmd): Json<Delivery>,
) -> Result<Json<serde_json::Value>, Error> {
    admitted(&s, &w.0, &actor.0, &cmd).await?;
    Ok(Json(serde_json::json!({"current":true})))
}

/// Explicitly installed registration; absent configuration exposes no route.
/// The existing management IAM/audit guard must wrap this router.
pub fn application_credentials_router(
    vault: Arc<VaultState>,
    registry: PathBuf,
    execution_authority: Option<Arc<dyn CredentialExecutionAuthority>>,
    clock: Arc<dyn awaken_time::WallClock>,
) -> Router {
    Router::new()
        .route("/v1/config/application-credentials/enter", post(enter))
        .route("/v1/config/application-credentials/deliver", post(deliver))
        .route("/v1/config/application-credentials/check", post(check))
        .route(
            "/v1/config/application-credentials/execution",
            post(execution),
        )
        .with_state(Service {
            vault,
            clock,
            registry,
            execution_authority,
        })
}
