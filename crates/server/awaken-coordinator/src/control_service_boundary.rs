// @formal entrypoint management_http.crash_preserves_pending_audit
// @formal entrypoint management_http.no_business_write_without_durable_audit
// @formal entrypoint management_http.successful_write_marks_audit_committed
//! Authenticated Control-to-Coordinator application adapters.
//!
//! The domain ports and their state machines remain in their authoritative
//! crates. This module only serializes secret-free commands and results so a
//! split Coordinator never receives Control database handles or a seal key.

use awaken_credential_contract::{SessionCredentialAccessRequest, SessionCredentialSource};
use std::sync::Arc;
use std::time::Duration;

use awaken_agent_config::{AuditedConfigWrite, ManagementAuditEntry, ManagementAuditRecord};
use awaken_config_service::{ManagementAuditPlane, ManagementAuditRepository};
use awaken_credential_contract::CredentialSourceId;

use awaken_agent_contract::ScopeId;
use awaken_session_contract::ManagedLifecycleFact;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::routing::post;
use axum::{Json, Router, response::IntoResponse};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Total per-attempt deadline for one private control-plane call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const AUDIT_RECORD_PATH: &str = "/internal/v1/control/audit/record";
const AUDIT_GET_PATH: &str = "/internal/v1/control/audit/get";
const AUDIT_COMMIT_PATH: &str = "/internal/v1/control/audit/commit";
const VAULT_EXISTS_PATH: &str = "/internal/v1/control/credentials/vault-exists";
const MCP_SOURCE_PATH: &str = "/internal/v1/control/credentials/mcp-source";
const MCP_ACCESS_PATH: &str = "/internal/v1/control/credentials/mcp-access";
const CREDENTIAL_ACCESS_PATH: &str = "/internal/v1/control/credentials/access";
const REPOSITORY_ADMISSION_PATH: &str = "/internal/v1/control/credentials/repository-admission";
const WEBHOOK_DELIVER_PATH: &str = "/internal/v1/control/webhooks/deliver";
const CONSENT_CEILING_PATH: &str = "/internal/v1/control/data-subjects/consent-ceiling";
const TELEMETRY_NARROWING_PATH: &str = "/internal/v1/control/telemetry/workspace-narrowing";
pub const DATA_SUBJECT_ERASE_PATH: &str = "/internal/v1/control/data-subjects/erase";
pub const ORGANIZATION_ERASE_PATH: &str = "/internal/v1/control/privacy/organizations/erase";
pub const ORGANIZATION_EXPORT_PATH: &str = "/internal/v1/control/privacy/organizations/export";
pub const CONTROL_SERVICE_ACCESS_PERMISSION: &str = "control:access";
const IDEMPOTENT_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct ControlServiceState {
    audit: ManagementAuditPlane,
    credentials: Arc<dyn SessionCredentialSource>,
    repository_credentials: Arc<dyn awaken_resource_contract::RepositoryCredentialValidator>,
    webhooks: Arc<dyn awaken_session_contract::LifecycleFactDelivery>,
    data_subjects: Arc<dyn awaken_data_governance_contract::DataSubjectResolver>,
    organization_privacy: Arc<dyn awaken_data_governance_contract::OrganizationPrivacyResolver>,
    telemetry: Arc<dyn awaken_data_governance_contract::TelemetryNarrowingSource>,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditRecordCommand {
    scope: ScopeId,
    audit: ManagementAuditRecord,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditLookupCommand {
    scope: ScopeId,
    tool: String,
    call_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VaultExistsCommand {
    workspace_id: String,
    vault_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct McpSourceCommand {
    workspace_id: String,
    vault_ids: Vec<String>,
    url: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceCommand {
    source_id: CredentialSourceId,
    workspace_id: String,
    target: awaken_agent_contract::McpTarget,
    selected_holder: awaken_agent_contract::credential::PlaintextHolder,
    binding: awaken_credential_contract::CredentialMaterialBinding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialAccessCommand {
    source_id: CredentialSourceId,
    workspace_id: String,
    target: awaken_agent_contract::credential::CredentialTarget,
    usage: awaken_agent_contract::credential::CredentialUsage,
    policy: awaken_agent_contract::credential::CredentialExecutionPolicy,
    selected_holder: awaken_agent_contract::credential::PlaintextHolder,
    binding: awaken_credential_contract::CredentialMaterialBinding,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepositoryAdmissionCommand {
    workspace_id: String,
    source_id: String,
    remote_url: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RepositoryAdmission {
    Admitted,
    Rejected,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConsentCeilingCommand {
    subject: awaken_runtime_contract::DataSubjectId,
    purpose: awaken_runtime_contract::Purpose,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetryNarrowingCommand {
    workspace_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DataSubjectEraseCommand {
    subject: awaken_runtime_contract::DataSubjectId,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizationEraseCommand {
    organization_id: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OrganizationExportCommand {
    organization_id: String,
    subject: Option<awaken_runtime_contract::DataSubjectId>,
}

/// The Control-owned privacy ports this private boundary projects: subject
/// consent and erasure, organization privacy, and the Workspace narrowing of
/// the content-capture ceiling.
#[derive(Clone)]
pub struct PrivacyPorts {
    pub data_subjects: Arc<dyn awaken_data_governance_contract::DataSubjectResolver>,
    pub organization_privacy: Arc<dyn awaken_data_governance_contract::OrganizationPrivacyResolver>,
    pub telemetry: Arc<dyn awaken_data_governance_contract::TelemetryNarrowingSource>,
}

pub fn router(
    audit: ManagementAuditPlane,
    credentials: Arc<dyn SessionCredentialSource>,
    repository_credentials: Arc<dyn awaken_resource_contract::RepositoryCredentialValidator>,
    webhooks: Arc<dyn awaken_session_contract::LifecycleFactDelivery>,
    privacy: PrivacyPorts,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    Ok(router_with_authenticator(
        audit,
        credentials,
        repository_credentials,
        webhooks,
        privacy,
        awaken_service_auth_contract::static_token_authenticator(bearer_token)?,
    ))
}

/// Compose the private Control surface with the process-owned authentication
/// policy. The router resolves that policy for every request, so token rotation
/// does not rebuild domain state or route ownership.
pub fn router_with_authenticator(
    audit: ManagementAuditPlane,
    credentials: Arc<dyn SessionCredentialSource>,
    repository_credentials: Arc<dyn awaken_resource_contract::RepositoryCredentialValidator>,
    webhooks: Arc<dyn awaken_session_contract::LifecycleFactDelivery>,
    privacy: PrivacyPorts,
    authenticator: Arc<dyn awaken_service_auth_contract::ServiceRequestAuthenticator>,
) -> Router {
    let state = ControlServiceState {
        audit,
        credentials,
        repository_credentials,
        webhooks,
        data_subjects: privacy.data_subjects,
        organization_privacy: privacy.organization_privacy,
        telemetry: privacy.telemetry,
        authenticator,
    };
    Router::new()
        .route(AUDIT_RECORD_PATH, post(record_audit))
        .route(AUDIT_GET_PATH, post(get_audit))
        .route(AUDIT_COMMIT_PATH, post(commit_audit))
        .route(VAULT_EXISTS_PATH, post(vault_exists))
        .route(MCP_SOURCE_PATH, post(mcp_source))
        .route(MCP_ACCESS_PATH, post(mcp_access))
        .route(CREDENTIAL_ACCESS_PATH, post(credential_access))
        .route(REPOSITORY_ADMISSION_PATH, post(repository_admission))
        .route(WEBHOOK_DELIVER_PATH, post(deliver_webhook))
        .route(CONSENT_CEILING_PATH, post(consent_ceiling))
        .route(TELEMETRY_NARROWING_PATH, post(telemetry_narrowing))
        .route(DATA_SUBJECT_ERASE_PATH, post(erase_data_subject))
        .route(ORGANIZATION_ERASE_PATH, post(erase_organization))
        .route(ORGANIZATION_EXPORT_PATH, post(export_organization))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_authorization,
        ))
        .with_state(state)
}

async fn require_authorization(
    State(state): State<ControlServiceState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> axum::response::Response {
    match state.authenticator.authenticate(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        awaken_service_auth_contract::ServiceAuthorizationRequirement::new(
            awaken_service_auth_contract::CONTROL_SERVICE_AUDIENCE,
            CONTROL_SERVICE_ACCESS_PERMISSION,
        ),
    ) {
        Ok(_) => next.run(request).await,
        Err(awaken_service_auth_contract::ServiceAuthError::Unauthorized) => {
            StatusCode::UNAUTHORIZED.into_response()
        }
        Err(awaken_service_auth_contract::ServiceAuthError::Forbidden) => {
            StatusCode::FORBIDDEN.into_response()
        }
        Err(awaken_service_auth_contract::ServiceAuthError::Unavailable(_)) => {
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

fn response<T: Serialize>(result: Result<T, String>) -> axum::response::Response {
    let status = if result.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(result)).into_response()
}

async fn record_audit(
    State(state): State<ControlServiceState>,
    Json(command): Json<AuditRecordCommand>,
) -> axum::response::Response {
    let result = state.audit.record(&command.scope, &command.audit).await;
    response(result)
}

async fn get_audit(
    State(state): State<ControlServiceState>,
    Json(command): Json<AuditLookupCommand>,
) -> axum::response::Response {
    let result = state
        .audit
        .get(&command.scope, &command.tool, &command.call_id)
        .await;
    response(result)
}

async fn commit_audit(
    State(state): State<ControlServiceState>,
    Json(command): Json<AuditLookupCommand>,
) -> axum::response::Response {
    let result = state
        .audit
        .mark_committed(&command.scope, &command.tool, &command.call_id)
        .await;
    response(result)
}

async fn vault_exists(
    State(state): State<ControlServiceState>,
    Json(command): Json<VaultExistsCommand>,
) -> axum::response::Response {
    let result = state
        .credentials
        .has_vault(&command.workspace_id, &command.vault_id)
        .await;
    response(result)
}

async fn mcp_source(
    State(state): State<ControlServiceState>,
    Json(command): Json<McpSourceCommand>,
) -> axum::response::Response {
    let result = state
        .credentials
        .mcp_credential_source_for_url(&command.workspace_id, &command.vault_ids, &command.url)
        .await;
    response(result)
}

async fn mcp_access(
    State(state): State<ControlServiceState>,
    Json(command): Json<SourceCommand>,
) -> axum::response::Response {
    let result = state
        .credentials
        .mcp_access_for_source(
            &command.source_id,
            &command.workspace_id,
            &command.target,
            &command.selected_holder,
            &command.binding,
        )
        .await;
    response(result)
}

async fn credential_access(
    State(state): State<ControlServiceState>,
    Json(command): Json<CredentialAccessCommand>,
) -> axum::response::Response {
    let result = state
        .credentials
        .credential_access_for_source(
            &command.source_id,
            &command.workspace_id,
            SessionCredentialAccessRequest {
                target: command.target,
                usage: command.usage,
                policy: command.policy,
                selected_holder: command.selected_holder,
                binding: command.binding,
            },
        )
        .await;
    response(result)
}

async fn repository_admission(
    State(state): State<ControlServiceState>,
    Json(command): Json<RepositoryAdmissionCommand>,
) -> axum::response::Response {
    let result = state
        .repository_credentials
        .validate(
            &command.workspace_id,
            &command.source_id,
            &command.remote_url,
        )
        .await;
    // Rejection is a successful read of an admission decision, not a transport
    // failure. Never forward credential/storage diagnostics to another process.
    response(match result {
        Ok(()) => Ok(RepositoryAdmission::Admitted),
        Err(awaken_resource_contract::ResourceRegistryError::AdmissionRejected { .. }) => {
            Ok(RepositoryAdmission::Rejected)
        }
        Err(_) => Err("Repository authorization service is unavailable".into()),
    })
}

async fn deliver_webhook(
    State(state): State<ControlServiceState>,
    Json(fact): Json<ManagedLifecycleFact>,
) -> axum::response::Response {
    let result = state.webhooks.deliver(&fact).await;
    response(result)
}

async fn consent_ceiling(
    State(state): State<ControlServiceState>,
    Json(command): Json<ConsentCeilingCommand>,
) -> axum::response::Response {
    response(Ok(state
        .data_subjects
        .consent_ceiling(&command.subject, command.purpose)
        .await))
}

async fn telemetry_narrowing(
    State(state): State<ControlServiceState>,
    Json(command): Json<TelemetryNarrowingCommand>,
) -> axum::response::Response {
    response(
        state
            .telemetry
            .workspace_narrowing(&command.workspace_id)
            .await
            .map_err(|error| error.to_string()),
    )
}

async fn erase_data_subject(
    State(state): State<ControlServiceState>,
    Json(command): Json<DataSubjectEraseCommand>,
) -> axum::response::Response {
    response(
        state
            .data_subjects
            .erase(&command.subject)
            .await
            .map_err(|error| error.to_string()),
    )
}

async fn erase_organization(
    State(state): State<ControlServiceState>,
    Json(command): Json<OrganizationEraseCommand>,
) -> axum::response::Response {
    response(
        state
            .organization_privacy
            .erase_organization(&command.organization_id)
            .await
            .map_err(|error| error.to_string()),
    )
}

async fn export_organization(
    State(state): State<ControlServiceState>,
    Json(command): Json<OrganizationExportCommand>,
) -> axum::response::Response {
    response(
        state
            .organization_privacy
            .export_organization(&command.organization_id, command.subject.as_ref())
            .await
            .map_err(|error| error.to_string()),
    )
}

#[derive(Clone)]
pub struct HttpControlServiceClient {
    base_url: String,
    token_source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
    client: awaken_outbound_http::InternalHttpClient,
}

impl HttpControlServiceClient {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        Self::with_token_source(
            base_url,
            awaken_service_auth_contract::static_token_source(bearer_token)?,
        )
    }

    pub fn with_token_source(
        base_url: impl Into<String>,
        token_source: Arc<dyn awaken_service_auth_contract::ServiceBearerTokenSource>,
    ) -> Result<Self, String> {
        let base_url = awaken_outbound_http::normalize_http_base_url(&base_url.into()).map_err(
            |rejected| match rejected {
                awaken_outbound_http::BaseUrlRejected::Malformed => {
                    format!("invalid Control service URL: {rejected}")
                }
                _ => "Control service requires an http(s) base URL and non-empty bearer token"
                    .to_owned(),
            },
        )?;
        awaken_service_auth_contract::resolve_service_bearer_token(token_source.as_ref())?;
        Ok(Self {
            base_url,
            token_source,
            client: awaken_outbound_http::InternalHttpClient::new(REQUEST_TIMEOUT)
                .map_err(|error| error.to_string())?,
        })
    }

    async fn post<C: Serialize + ?Sized, O: DeserializeOwned>(
        &self,
        path: &str,
        command: &C,
    ) -> Result<O, String> {
        let mut last_error = None;
        for attempt in 1..=IDEMPOTENT_ATTEMPTS {
            let bearer_token = awaken_service_auth_contract::resolve_service_bearer_token(
                self.token_source.as_ref(),
            )?;
            let body = serde_json::to_string(command)
                .map_err(|error| format!("encode Control command: {error}"))?;
            let response = self
                .client
                .send(
                    &awaken_outbound_http::JsonRequest::post(format!("{}{}", self.base_url, path))
                        .bearer(bearer_token.as_ref())
                        .json_body(body),
                )
                .await;
            let response = match response {
                Ok(response) => response,
                Err(error) => {
                    last_error = Some(error.to_string());
                    if attempt < IDEMPOTENT_ATTEMPTS {
                        tokio::time::sleep(RETRY_DELAY).await;
                        continue;
                    }
                    break;
                }
            };
            if response.is_unauthorized() {
                let rotated = awaken_service_auth_contract::service_bearer_token_rotated(
                    self.token_source.as_ref(),
                    bearer_token.as_ref(),
                )?;
                if attempt < IDEMPOTENT_ATTEMPTS && rotated {
                    continue;
                }
                return Err("Control service rejected bearer credentials".into());
            }
            let status = response.status();
            match serde_json::from_str::<Result<O, String>>(response.body()) {
                Ok(Ok(value)) if response.is_success() => return Ok(value),
                Ok(Err(error)) => last_error = Some(error),
                Ok(Ok(_)) => last_error = Some(format!("Control service returned {status}")),
                Err(error) => last_error = Some(format!("decode Control response: {error}")),
            }
            if attempt < IDEMPOTENT_ATTEMPTS && response.is_server_error() {
                tokio::time::sleep(RETRY_DELAY).await;
                continue;
            }
            break;
        }
        Err(last_error.unwrap_or_else(|| "Control service unavailable".into()))
    }
}

#[async_trait::async_trait]
impl ManagementAuditRepository for HttpControlServiceClient {
    async fn record(
        &self,
        scope: &ScopeId,
        audit: &ManagementAuditRecord,
    ) -> Result<AuditedConfigWrite, String> {
        self.post(
            AUDIT_RECORD_PATH,
            &AuditRecordCommand {
                scope: scope.clone(),
                audit: audit.clone(),
            },
        )
        .await
    }

    async fn get(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<Option<ManagementAuditEntry>, String> {
        self.post(
            AUDIT_GET_PATH,
            &AuditLookupCommand {
                scope: scope.clone(),
                tool: tool.to_owned(),
                call_id: call_id.to_owned(),
            },
        )
        .await
    }

    async fn mark_committed(
        &self,
        scope: &ScopeId,
        tool: &str,
        call_id: &str,
    ) -> Result<(), String> {
        self.post(
            AUDIT_COMMIT_PATH,
            &AuditLookupCommand {
                scope: scope.clone(),
                tool: tool.to_owned(),
                call_id: call_id.to_owned(),
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl SessionCredentialSource for HttpControlServiceClient {
    async fn has_vault(&self, workspace_id: &str, id: &str) -> Result<bool, String> {
        self.post(
            VAULT_EXISTS_PATH,
            &VaultExistsCommand {
                workspace_id: workspace_id.to_owned(),
                vault_id: id.to_owned(),
            },
        )
        .await
    }

    async fn mcp_credential_source_for_url(
        &self,
        workspace_id: &str,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, String> {
        self.post(
            MCP_SOURCE_PATH,
            &McpSourceCommand {
                workspace_id: workspace_id.to_owned(),
                vault_ids: vault_ids.to_vec(),
                url: url.to_owned(),
            },
        )
        .await
    }

    async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        target: &awaken_agent_contract::McpTarget,
        selected_holder: &awaken_agent_contract::credential::PlaintextHolder,
        binding: &awaken_credential_contract::CredentialMaterialBinding,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, String> {
        self.post(
            MCP_ACCESS_PATH,
            &SourceCommand {
                source_id: source_id.clone(),
                workspace_id: workspace_id.to_owned(),
                target: target.clone(),
                selected_holder: selected_holder.clone(),
                binding: binding.clone(),
            },
        )
        .await
    }

    async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        request: SessionCredentialAccessRequest,
    ) -> Result<awaken_agent_contract::credential::CredentialAccess, String> {
        let SessionCredentialAccessRequest {
            target,
            usage,
            policy,
            selected_holder,
            binding,
        } = request;
        self.post(
            CREDENTIAL_ACCESS_PATH,
            &CredentialAccessCommand {
                source_id: source_id.clone(),
                workspace_id: workspace_id.to_owned(),
                target,
                usage,
                policy,
                selected_holder,
                binding,
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl awaken_resource_contract::RepositoryCredentialValidator for HttpControlServiceClient {
    async fn validate(
        &self,
        workspace_id: &str,
        source_id: &str,
        remote_url: &str,
    ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
        use awaken_resource_contract::ResourceRegistryError;
        let decision: RepositoryAdmission = self
            .post(
                REPOSITORY_ADMISSION_PATH,
                &RepositoryAdmissionCommand {
                    workspace_id: workspace_id.into(),
                    source_id: source_id.into(),
                    remote_url: remote_url.into(),
                },
            )
            .await
            .map_err(|_| ResourceRegistryError::AuthorityUnavailable {
                detail: "Repository authorization service is unavailable".into(),
            })?;
        match decision {
            RepositoryAdmission::Admitted => Ok(()),
            RepositoryAdmission::Rejected => Err(ResourceRegistryError::AdmissionRejected {
                detail: "Choose an active repository authorization in this Workspace for this Git server"
                    .into(),
            }),
        }
    }
}

#[async_trait::async_trait]
impl awaken_session_contract::LifecycleFactDelivery for HttpControlServiceClient {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
        self.post(WEBHOOK_DELIVER_PATH, fact).await
    }
}

#[async_trait::async_trait]
impl awaken_data_governance_contract::DataSubjectConsentSource for HttpControlServiceClient {
    async fn consent_ceiling(
        &self,
        subject: &awaken_runtime_contract::DataSubjectId,
        purpose: awaken_runtime_contract::Purpose,
    ) -> awaken_runtime_contract::ContentCapture {
        self.post(
            CONSENT_CEILING_PATH,
            &ConsentCeilingCommand {
                subject: subject.clone(),
                purpose,
            },
        )
        .await
        // Fail closed: an unavailable Control consent authority must never
        // widen content capture in Coordinator.
        .unwrap_or(awaken_runtime_contract::ContentCapture::Structured)
    }
}

#[async_trait::async_trait]
impl awaken_data_governance_contract::TelemetryNarrowingSource for HttpControlServiceClient {
    async fn workspace_narrowing(
        &self,
        workspace: &str,
    ) -> Result<
        awaken_data_governance_contract::TelemetryNarrowing,
        awaken_data_governance_contract::TelemetryPolicyError,
    > {
        // An error reaches the Run boundary, which closes the ceiling: an
        // unavailable Control authority must never widen content capture.
        self.post(
            TELEMETRY_NARROWING_PATH,
            &TelemetryNarrowingCommand {
                workspace_id: workspace.to_owned(),
            },
        )
        .await
        .map_err(|_| awaken_data_governance_contract::TelemetryPolicyError::AuthorityUnreachable)
    }
}

#[async_trait::async_trait]
impl awaken_data_governance_contract::DataSubjectResolver for HttpControlServiceClient {
    async fn erase(
        &self,
        subject: &awaken_runtime_contract::DataSubjectId,
    ) -> Result<
        awaken_data_governance_contract::ErasureReceipt,
        awaken_data_governance_contract::ErasureError,
    > {
        self.post(
            DATA_SUBJECT_ERASE_PATH,
            &DataSubjectEraseCommand {
                subject: subject.clone(),
            },
        )
        .await
        .map_err(awaken_data_governance_contract::ErasureError)
    }
}

#[async_trait::async_trait]
impl awaken_data_governance_contract::OrganizationPrivacyResolver for HttpControlServiceClient {
    async fn erase_organization(
        &self,
        organization_id: &str,
    ) -> Result<
        awaken_data_governance_contract::ErasureReceipt,
        awaken_data_governance_contract::ErasureError,
    > {
        self.post(
            ORGANIZATION_ERASE_PATH,
            &OrganizationEraseCommand {
                organization_id: organization_id.to_owned(),
            },
        )
        .await
        .map_err(awaken_data_governance_contract::ErasureError)
    }

    async fn export_organization(
        &self,
        organization_id: &str,
        subject: Option<&awaken_runtime_contract::DataSubjectId>,
    ) -> Result<
        awaken_data_governance_contract::OrganizationPrivacyExport,
        awaken_data_governance_contract::PrivacyExportError,
    > {
        self.post(
            ORGANIZATION_EXPORT_PATH,
            &OrganizationExportCommand {
                organization_id: organization_id.to_owned(),
                subject: subject.cloned(),
            },
        )
        .await
        .map_err(awaken_data_governance_contract::PrivacyExportError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, RwLock};

    struct RotatingTokenSource(RwLock<Arc<str>>);

    impl awaken_service_auth_contract::ServiceBearerTokenSource for RotatingTokenSource {
        fn current_token(&self) -> Result<Arc<str>, String> {
            Ok(self.0.read().expect("token source read").clone())
        }
    }

    /// Control's authored narrowing: `workspace-a` captures no content.
    struct ClosedWorkspace;

    #[async_trait::async_trait]
    impl awaken_data_governance_contract::TelemetryNarrowingSource for ClosedWorkspace {
        async fn workspace_narrowing(
            &self,
            workspace: &str,
        ) -> Result<
            awaken_data_governance_contract::TelemetryNarrowing,
            awaken_data_governance_contract::TelemetryPolicyError,
        > {
            Ok(awaken_data_governance_contract::TelemetryNarrowing {
                content_capture: (workspace == "workspace-a")
                    .then_some(awaken_runtime_contract::ContentCapture::Off),
            })
        }
    }

    #[derive(Default)]
    struct RecordingDelivery(Mutex<Vec<ManagedLifecycleFact>>);

    #[derive(Default)]
    struct RecordingRepositoryAdmission(Mutex<Vec<(String, String, String)>>);

    #[async_trait::async_trait]
    impl awaken_resource_contract::RepositoryCredentialValidator for RecordingRepositoryAdmission {
        async fn validate(
            &self,
            workspace: &str,
            source: &str,
            remote: &str,
        ) -> Result<(), awaken_resource_contract::ResourceRegistryError> {
            use awaken_resource_contract::ResourceRegistryError;
            self.0
                .lock()
                .unwrap()
                .push((workspace.into(), source.into(), remote.into()));
            match source {
                "allowed" => Ok(()),
                "outage" => Err(ResourceRegistryError::AuthorityUnavailable {
                    detail: "private storage detail".into(),
                }),
                _ => Err(ResourceRegistryError::AdmissionRejected {
                    detail: "private credential detail".into(),
                }),
            }
        }
    }

    #[async_trait::async_trait]
    impl awaken_session_contract::LifecycleFactDelivery for RecordingDelivery {
        async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
            self.0.lock().expect("delivery lock").push(fact.clone());
            Ok(())
        }
    }

    #[test]
    fn client_configuration_fails_closed() {
        // Cause/effect decision table: C1 valid HTTP(S) base + non-empty token
        // -> construct the one cross-boundary adapter; C2 invalid URL, C3 query
        // bearing URL, or C4 blank token -> reject before any request is sent.
        assert!(
            HttpControlServiceClient::new("http://control:3000", "token").is_ok(),
            "C1"
        );
        assert!(
            HttpControlServiceClient::new("control", "token").is_err(),
            "C2"
        );
        assert!(
            HttpControlServiceClient::new("http://control?scope=x", "token").is_err(),
            "C3"
        );
        assert!(
            HttpControlServiceClient::new("http://control", " ").is_err(),
            "C4"
        );
    }

    #[tokio::test]
    async fn authenticated_transport_preserves_control_authorities() {
        // Cause/effect decision table:
        // R1 valid bearer + audit commands -> the one Control audit state machine
        // records, reads, and commits the stable call identity; R2 valid bearer +
        // lifecycle fact -> the injected delivery port observes the exact fact once;
        // R3 valid bearer + unknown vault -> false without exposing secret material;
        // R4 invalid bearer -> reject before any authoritative mutation; R5
        // authenticated consent read preserves Control's result; R6 rejected or
        // unavailable consent reads fail closed to Structured; R7 authenticated
        // subject erase reaches the same resolver and returns its stable receipt;
        // R8 rotating the injected source makes router and client accept only
        // the successor token without reconstruction; R9 organization erase and
        // export use the same authenticated private boundary; R11 the
        // Workspace telemetry narrowing crosses it unchanged and R12 a rejected
        // read is an error, never an open ceiling. These rules cover
        // every boundary authority and the authentication gate.
        let audit = ManagementAuditPlane::new(Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory()
                .await
                .expect("open audit test store"),
        ));
        let credential_repo =
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let credentials = Arc::new(awaken_protocol_managed::VaultState::new(
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            credential_repo.clone(),
            awaken_time::system_clock(),
            None,
        ));
        let delivery = Arc::new(RecordingDelivery::default());
        let token_source = Arc::new(RotatingTokenSource(RwLock::new(Arc::from("correct-token"))));
        let repository_admission = Arc::new(RecordingRepositoryAdmission::default());
        let app = router_with_authenticator(
            audit,
            credentials.application().credential_access(),
            repository_admission.clone(),
            delivery.clone(),
            PrivacyPorts {
                data_subjects: Arc::new(awaken_data_governance_contract::NullResolver),
                organization_privacy: Arc::new(awaken_data_governance_contract::NullResolver),
                telemetry: Arc::new(ClosedWorkspace),
            },
            Arc::new(awaken_service_auth_contract::TokenSourceAuthenticator::new(
                token_source.clone(),
            )),
        );
        // Observe only transport paths; original auth and handlers remain authoritative.
        let credential_requests = Arc::new(Mutex::new(Vec::<String>::new()));
        let observed_requests = credential_requests.clone();
        let app = app.layer(middleware::from_fn(move |request: Request, next: Next| {
            let requests = observed_requests.clone();
            async move {
                requests
                    .lock()
                    .unwrap()
                    .push(request.uri().path().to_owned());
                next.run(request).await
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve test boundary");
        });
        let client = HttpControlServiceClient::with_token_source(
            format!("http://{address}"),
            token_source.clone(),
        )
        .unwrap();
        let scope = ScopeId::from("workspace-a");
        // R10 exact metadata reaches Control; rejection is not retried, outage
        // is retried, and invalid service authentication never reaches the port.
        use awaken_resource_contract::{RepositoryCredentialValidator, ResourceRegistryError};
        let remote = "https://github.com/example/workshop.git";
        RepositoryCredentialValidator::validate(&client, "workspace-a", "allowed", remote)
            .await
            .unwrap();
        assert_eq!(
            repository_admission.0.lock().unwrap().as_slice(),
            &[("workspace-a".into(), "allowed".into(), remote.into())]
        );
        assert!(matches!(
            RepositoryCredentialValidator::validate(&client, "workspace-a", "denied", remote).await,
            Err(ResourceRegistryError::AdmissionRejected { .. })
        ));
        assert_eq!(
            repository_admission.0.lock().unwrap().len(),
            2,
            "rejections are not transport retries"
        );
        let outage =
            RepositoryCredentialValidator::validate(&client, "workspace-a", "outage", remote)
                .await
                .unwrap_err();
        assert!(matches!(
            outage,
            ResourceRegistryError::AuthorityUnavailable { .. }
        ));
        assert!(!outage.to_string().contains("private storage detail"));
        assert_eq!(repository_admission.0.lock().unwrap().len(), 5);
        let unauthorized =
            HttpControlServiceClient::new(format!("http://{address}"), "incorrect").unwrap();
        assert!(
            RepositoryCredentialValidator::validate(
                &unauthorized,
                "workspace-a",
                "allowed",
                remote
            )
            .await
            .is_err()
        );
        assert_eq!(repository_admission.0.lock().unwrap().len(), 5);
        let record = ManagementAuditRecord {
            tool: "http:POST:/v1/sessions".into(),
            call_id: "call-1".into(),
            summary: "body_sha256=stable".into(),
        };
        assert_eq!(
            client.record(&scope, &record).await.unwrap(),
            AuditedConfigWrite::Applied,
            "R1"
        );
        assert_eq!(
            client
                .get(&scope, &record.tool, &record.call_id)
                .await
                .unwrap()
                .expect("audit exists")
                .record,
            record,
            "R1"
        );
        client
            .mark_committed(&scope, &record.tool, &record.call_id)
            .await
            .unwrap();
        assert!(
            client
                .get(&scope, &record.tool, &record.call_id)
                .await
                .unwrap()
                .expect("audit exists")
                .business_committed,
            "R1"
        );

        let fact = ManagedLifecycleFact {
            id: "fact-1".into(),
            object_id: "session-1".into(),
            workspace_id: Some("workspace-a".into()),
            event_type: "session.status_idled".into(),
            timestamp: 7,
            runtime_interval: None,
        };
        awaken_session_contract::LifecycleFactDelivery::deliver(&client, &fact)
            .await
            .unwrap();
        assert_eq!(*delivery.0.lock().unwrap(), vec![fact], "R2");
        assert!(
            !client.has_vault("workspace-a", "missing").await.unwrap(),
            "R3"
        );
        assert_eq!(
            awaken_data_governance_contract::DataSubjectConsentSource::consent_ceiling(
                &client,
                &awaken_runtime_contract::DataSubjectId("dsub-a".into()),
                awaken_runtime_contract::Purpose::TelemetryContent,
            )
            .await,
            awaken_runtime_contract::ContentCapture::Full,
            "R5"
        );
        assert_eq!(
            awaken_data_governance_contract::TelemetryNarrowingSource::workspace_narrowing(
                &client,
                "workspace-a",
            )
            .await
            .unwrap()
            .content_capture,
            Some(awaken_runtime_contract::ContentCapture::Off),
            "R11 authenticated narrowing read preserves Control's value"
        );
        assert_eq!(
            awaken_data_governance_contract::TelemetryNarrowingSource::workspace_narrowing(
                &client,
                "workspace-b",
            )
            .await
            .unwrap(),
            awaken_data_governance_contract::TelemetryNarrowing::default(),
            "R11 another Workspace keeps its own narrowing"
        );
        assert_eq!(
            awaken_data_governance_contract::DataSubjectResolver::erase(
                &client,
                &awaken_runtime_contract::DataSubjectId("dsub-a".into()),
            )
            .await
            .unwrap(),
            awaken_data_governance_contract::ErasureReceipt::default(),
            "R7"
        );
        assert_eq!(
            awaken_data_governance_contract::OrganizationPrivacyResolver::erase_organization(
                &client, "org-a",
            )
            .await
            .unwrap(),
            awaken_data_governance_contract::ErasureReceipt::default(),
            "R9 erase"
        );
        assert!(
            awaken_data_governance_contract::OrganizationPrivacyResolver::export_organization(
                &client, "org-a", None,
            )
            .await
            .unwrap()
            .records
            .is_empty(),
            "R9 export"
        );

        {
            use awaken_credential_vault::repo::CredentialRepo as _;
            use awaken_protocol_managed_testkit::ManagedStateFixture as _;
            use awaken_session_application::{
                McpAttachmentCandidate, McpAttachmentCandidateTarget,
            };
            use awaken_session_configuration_contract::mcp::McpAttachmentOrigin;
            use awaken_session_execution_contract::NeutralRuntime;

            // MCP causes: C1 published pin; C2 source capability installed;
            // C3 HTTP/stdio; C4 discovery success/miss/error; C5 access result;
            // C6 published revision matches; C7 any LATER target malformed.
            // Effects: E1 exact pin, E2 optional None, E3 original mapped error,
            // E4 exact Control request order, E5 no credential I/O for C7.
            // The observer passes unchanged requests to real authentication,
            // handlers and Vault; it cannot fabricate a source or access.
            // Original transport retries 503 THREE times: one normalizer SPI
            // access can yield three requests. A rejected static bearer does
            // not rotate and is tried once. Neither retry law is changed.
            //
            // | rule | causes | effects |
            // | M0 | valid first + malformed later, both C1 edges | E3 URL + E5; M4/M10 are working positive controls |
            // | M1 | C1 + !C2 | canonical-authority error, zero requests |
            // | M2 | C1 + C2 + missing source | pin error; access x3, no discovery/fallback |
            // | M3 | C1 + successful access + !C6 | access once THEN revision error, no discovery |
            // | M4 | C1 + successful access + C6 | E1; access once, unrelated Vault list ignored |
            // | M5 | !C1 + !C2 | E2, zero requests |
            // | M6 | !C1 + C2 + stdio | E2, zero requests |
            // | M7 | !C1 + HTTP + rejected discovery auth | selection error; source once, no access |
            // | M8 | !C1 + HTTP + miss | E2; source once, no access |
            // | M9 | !C1 + selected active child + Disabled source | pin error; source once then access x3; no anonymous fallback |
            // | M10 | !C1 + HTTP + selected active source | E1; source once then access once |
            //
            // M9 deliberately leaves the real Managed child Active. URL
            // discovery selects it; the original exact-source compiler rejects
            // its Disabled source. Issuer/state assertions cannot establish
            // request counts: E4/E5 use the actual Control transport trace.
            let url = "https://normalizer.example.test/mcp";
            let (vault_id, source_id, revision, adoption) = credentials
                .enter_application_mcp_bearer(
                    "workspace-a",
                    "normalizer-fixture",
                    url,
                    "normalizer-command",
                    1,
                    // awaken-allow(secret): test-only material never recorded by the observer.
                    awaken_agent_contract::RedactedString::new("normalizer-test-bearer"),
                )
                .await
                .unwrap();
            assert!(
                adoption.is_converged(),
                "new source has no predecessor rollout"
            );
            // O10: the original M0-M10 table below calls the same unique credential service.
            let no_authority = awaken_protocol_managed::ManagedState::new(NeutralRuntime)
                .await
                .session_application();
            let selected = awaken_protocol_managed::ManagedState::new(NeutralRuntime)
                .await
                .with_credential_source(Arc::new(client.clone()))
                .session_application();
            let denied = awaken_protocol_managed::ManagedState::new(NeutralRuntime)
                .await
                .with_credential_source(Arc::new(unauthorized.clone()))
                .session_application();
            let holder =
                awaken_credential_contract::CredentialRealizationProfile::self_hosted_native()
                    .mcp_holder;
            let missing_id = "missing-for-normalizer";
            let pin_missing = format!(
                "MCP credential could not be pinned exactly: {}",
                awaken_credential_vault::CredentialError::SourceNotFound(missing_id.into()),
            );
            let pin_disabled = format!(
                "MCP credential could not be pinned exactly: {}",
                awaken_credential_vault::CredentialError::NotActive(source_id.0.clone()),
            );
            for (rule, application, published, target_url, error, paths, authenticated) in [
                (
                    "M1", &no_authority, Some((source_id.0.as_str(), revision)), Some(url),
                    Some("published MCP credential requires the canonical credential authority".to_owned()),
                    vec![], false,
                ),
                (
                    "M2", &selected, Some((missing_id, revision)), Some(url),
                    Some(pin_missing), vec![MCP_ACCESS_PATH; 3], false,
                ),
                (
                    "M3", &selected, Some((source_id.0.as_str(), revision + 1)), Some(url),
                    Some("published MCP credential revision no longer matches".to_owned()),
                    vec![MCP_ACCESS_PATH], false,
                ),
                (
                    "M4", &selected, Some((source_id.0.as_str(), revision)), Some(url),
                    None, vec![MCP_ACCESS_PATH], true,
                ),
                ("M5", &no_authority, None, Some(url), None, vec![], false),
                ("M6", &selected, None, None, None, vec![], false),
                (
                    "M7", &denied, None, Some(url),
                    Some("MCP credential selection failed: Control service rejected bearer credentials".to_owned()),
                    vec![MCP_SOURCE_PATH], false,
                ),
                (
                    "M8", &selected, None, Some("https://unmatched.example.test/mcp"),
                    None, vec![MCP_SOURCE_PATH], false,
                ),
                (
                    "M9", &selected, None, Some(url), Some(pin_disabled),
                    vec![MCP_SOURCE_PATH, MCP_ACCESS_PATH, MCP_ACCESS_PATH, MCP_ACCESS_PATH], false,
                ),
                (
                    "M10", &selected, None, Some(url), None,
                    vec![MCP_SOURCE_PATH, MCP_ACCESS_PATH], true,
                ),
            ] {
                if rule == "M9" || rule == "M10" {
                    let mut source = credential_repo.get(&source_id).await.unwrap();
                    source.status = if rule == "M9" {
                        awaken_credential_vault::CredentialStatus::Disabled
                    } else {
                        awaken_credential_vault::CredentialStatus::Active
                    };
                    credential_repo.put(source).await.unwrap();
                    assert!(
                        credentials.application().credential_access()
                            .credential_of_source("workspace-a", &source_id)
                            .await.unwrap().unwrap().lifecycle.is_active(),
                        "{rule}: original Managed child remains Active",
                    );
                }
                let target = match target_url {
                    Some(url) => McpAttachmentCandidateTarget::HttpUrl(url.into()),
                    None => McpAttachmentCandidateTarget::SandboxStdio {
                        command: "normalizer-fixture".into(), args: vec![],
                    },
                };
                let candidates = vec![McpAttachmentCandidate {
                    name: rule.into(), target, prompts_as_skills: true,
                    published_credential: published.map(|(id, revision)| (id.into(), revision)),
                    origin: McpAttachmentOrigin::Session,
                }];
                let ordered_vaults = if rule == "M4" {
                    vec!["unrelated-vault".into()]
                } else {
                    vec![vault_id.clone()]
                };
                credential_requests.lock().unwrap().clear();
                let result = application.credentials().normalize_mcp_drafts(
                    "workspace-a", candidates, &ordered_vaults, &holder,
                ).await;
                match error {
                    Some(message) => {
                        let error = result.expect_err(rule);
                        assert_eq!(error.kind, awaken_session_types::RunErrorKind::BadRequest, "{rule}");
                        assert_eq!(error.message, message, "{rule}: original error projection");
                    }
                    None => {
                        let drafts = result.expect(rule);
                        assert_eq!(drafts.len(), 1, "{rule}");
                        assert_eq!(drafts[0].name, rule, "{rule}");
                        assert!(drafts[0].prompts_as_skills, "{rule}");
                        assert_eq!(drafts[0].origin, McpAttachmentOrigin::Session, "{rule}");
                        assert_eq!(drafts[0].credential.is_some(), authenticated, "{rule}");
                        if let Some(access) = &drafts[0].credential {
                            assert_eq!(access.credential.id, source_id.0, "{rule}");
                            assert_eq!(access.credential.revision, revision, "{rule}");
                        }
                    }
                }
                assert_eq!(credential_requests.lock().unwrap().as_slice(), paths, "{rule}: actual transport order");
            }
            for published in [false, true] {
                let first = McpAttachmentCandidate {
                    name: "working-first".into(),
                    target: McpAttachmentCandidateTarget::HttpUrl(url.into()),
                    prompts_as_skills: false,
                    published_credential: published.then(|| (source_id.0.clone(), revision)),
                    origin: McpAttachmentOrigin::Session,
                };
                let later = McpAttachmentCandidate {
                    name: "malformed-later".into(),
                    target: McpAttachmentCandidateTarget::HttpUrl("not an HTTP URL".into()),
                    published_credential: None,
                    ..first.clone()
                };
                credential_requests.lock().unwrap().clear();
                let error = selected
                    .credentials()
                    .normalize_mcp_drafts(
                        "workspace-a",
                        vec![first, later],
                        std::slice::from_ref(&vault_id),
                        &holder,
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.kind,
                    awaken_session_types::RunErrorKind::BadRequest,
                    "M0 published={published}"
                );
                assert_eq!(
                    error.message, "invalid MCP server URL for `malformed-later`",
                    "M0 published={published}"
                );
                assert!(
                    credential_requests.lock().unwrap().is_empty(),
                    "M0 published={published}: all targets normalize before ANY credential request"
                );
            }
        }

        *token_source.0.write().expect("token source write") = Arc::from("next-token");
        assert!(
            !client
                .has_vault("workspace-a", "still-missing")
                .await
                .unwrap(),
            "R8"
        );

        let rejected =
            HttpControlServiceClient::new(format!("http://{address}"), "correct-token").unwrap();
        let mut rejected_record = record;
        rejected_record.call_id = "call-rejected".into();
        assert!(
            rejected.record(&scope, &rejected_record).await.is_err(),
            "R4"
        );
        assert_eq!(
            awaken_data_governance_contract::DataSubjectConsentSource::consent_ceiling(
                &rejected,
                &awaken_runtime_contract::DataSubjectId("dsub-a".into()),
                awaken_runtime_contract::Purpose::TelemetryContent,
            )
            .await,
            awaken_runtime_contract::ContentCapture::Structured,
            "R6"
        );
        assert_eq!(
            awaken_data_governance_contract::TelemetryNarrowingSource::workspace_narrowing(
                &rejected,
                "workspace-a",
            )
            .await,
            Err(awaken_data_governance_contract::TelemetryPolicyError::AuthorityUnreachable),
            "R12 a rejected narrowing read is an error the Run boundary closes"
        );
        assert!(
            awaken_data_governance_contract::DataSubjectResolver::erase(
                &rejected,
                &awaken_runtime_contract::DataSubjectId("dsub-rejected".into()),
            )
            .await
            .is_err(),
            "R4"
        );
        assert!(
            awaken_data_governance_contract::OrganizationPrivacyResolver::erase_organization(
                &rejected,
                "org-rejected",
            )
            .await
            .is_err(),
            "R4 organization erase"
        );
        assert!(
            awaken_data_governance_contract::OrganizationPrivacyResolver::export_organization(
                &rejected,
                "org-rejected",
                None,
            )
            .await
            .is_err(),
            "R4 organization export"
        );
        assert!(
            client
                .get(&scope, &rejected_record.tool, &rejected_record.call_id)
                .await
                .unwrap()
                .is_none(),
            "R4"
        );
        server.abort();
    }
}
