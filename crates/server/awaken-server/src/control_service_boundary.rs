//! Authenticated Control-to-Coordinator application adapters.
//!
//! The domain ports and their state machines remain in their authoritative
//! crates. This module only serializes secret-free commands and results so a
//! split Coordinator never receives Control database handles or a seal key.

use std::sync::Arc;
use std::time::Duration;

use awaken_config_service::{ManagementAuditPlane, ManagementAuditRepository};
use awaken_config_store::{AuditedConfigWrite, ManagementAuditEntry, ManagementAuditRecord};
use awaken_credential_vault::CredentialSourceId;
use awaken_protocol_managed::SessionCredentialSource;
use awaken_session_contract::ManagedLifecycleFact;
use awaken_tenancy::ScopeId;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::routing::post;
use axum::{Json, Router, response::IntoResponse};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

const AUDIT_RECORD_PATH: &str = "/internal/v1/control/audit/record";
const AUDIT_GET_PATH: &str = "/internal/v1/control/audit/get";
const AUDIT_COMMIT_PATH: &str = "/internal/v1/control/audit/commit";
const VAULT_EXISTS_PATH: &str = "/internal/v1/control/credentials/vault-exists";
const MCP_SOURCE_PATH: &str = "/internal/v1/control/credentials/mcp-source";
const MCP_ACCESS_PATH: &str = "/internal/v1/control/credentials/mcp-access";
const CREDENTIAL_ACCESS_PATH: &str = "/internal/v1/control/credentials/access";
const WEBHOOK_DELIVER_PATH: &str = "/internal/v1/control/webhooks/deliver";
const CONSENT_CEILING_PATH: &str = "/internal/v1/control/data-subjects/consent-ceiling";
const IDEMPOTENT_ATTEMPTS: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);

#[derive(Clone)]
struct ControlServiceState {
    audit: ManagementAuditPlane,
    credentials: Arc<dyn SessionCredentialSource>,
    webhooks: Arc<dyn awaken_webhook_managed::LifecycleFactDelivery>,
    consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
    bearer_token: Arc<str>,
}

#[derive(Serialize, Deserialize)]
struct AuditRecordCommand {
    scope: ScopeId,
    audit: ManagementAuditRecord,
}

#[derive(Serialize, Deserialize)]
struct AuditLookupCommand {
    scope: ScopeId,
    tool: String,
    call_id: String,
}

#[derive(Serialize, Deserialize)]
struct VaultExistsCommand {
    vault_id: String,
}

#[derive(Serialize, Deserialize)]
struct McpSourceCommand {
    vault_ids: Vec<String>,
    url: String,
}

#[derive(Serialize, Deserialize)]
struct SourceCommand {
    source_id: CredentialSourceId,
}

#[derive(Serialize, Deserialize)]
struct CredentialAccessCommand {
    source_id: CredentialSourceId,
    workspace_id: String,
    usage: awaken_runtime_contract::CredentialUsage,
    policy: awaken_runtime_contract::CredentialExecutionPolicy,
}

#[derive(Serialize, Deserialize)]
struct ConsentCeilingCommand {
    subject: awaken_runtime_contract::DataSubjectId,
    purpose: awaken_runtime_contract::Purpose,
}

pub fn router(
    audit: ManagementAuditPlane,
    credentials: Arc<dyn SessionCredentialSource>,
    webhooks: Arc<dyn awaken_webhook_managed::LifecycleFactDelivery>,
    consent: Arc<dyn awaken_runtime_contract::DataSubjectConsentSource>,
    bearer_token: impl Into<String>,
) -> Result<Router, String> {
    let bearer_token = bearer_token.into();
    if bearer_token.trim().is_empty() {
        return Err("Control service bearer token must not be empty".into());
    }
    let state = ControlServiceState {
        audit,
        credentials,
        webhooks,
        consent,
        bearer_token: Arc::from(bearer_token),
    };
    Ok(Router::new()
        .route(AUDIT_RECORD_PATH, post(record_audit))
        .route(AUDIT_GET_PATH, post(get_audit))
        .route(AUDIT_COMMIT_PATH, post(commit_audit))
        .route(VAULT_EXISTS_PATH, post(vault_exists))
        .route(MCP_SOURCE_PATH, post(mcp_source))
        .route(MCP_ACCESS_PATH, post(mcp_access))
        .route(CREDENTIAL_ACCESS_PATH, post(credential_access))
        .route(WEBHOOK_DELIVER_PATH, post(deliver_webhook))
        .route(CONSENT_CEILING_PATH, post(consent_ceiling))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_authorization,
        ))
        .with_state(state))
}

fn authorized(headers: &HeaderMap, expected: &str) -> bool {
    awaken_service_auth_contract::service_bearer_token_matches(
        headers
            .get(header::AUTHORIZATION)
            .map(|value| value.as_bytes()),
        expected,
    )
}

async fn require_authorization(
    State(state): State<ControlServiceState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> axum::response::Response {
    if !authorized(&headers, &state.bearer_token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    next.run(request).await
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
    let result = state.credentials.has_vault(&command.vault_id).await;
    response(result)
}

async fn mcp_source(
    State(state): State<ControlServiceState>,
    Json(command): Json<McpSourceCommand>,
) -> axum::response::Response {
    let result = state
        .credentials
        .mcp_credential_source_for_url(&command.vault_ids, &command.url)
        .await;
    response(result)
}

async fn mcp_access(
    State(state): State<ControlServiceState>,
    Json(command): Json<SourceCommand>,
) -> axum::response::Response {
    let result = state
        .credentials
        .mcp_access_for_source(&command.source_id)
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
            command.usage,
            command.policy,
        )
        .await;
    response(result)
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
        .consent
        .consent_ceiling(&command.subject, command.purpose)
        .await))
}

#[derive(Clone)]
pub struct HttpControlServiceClient {
    base_url: String,
    bearer_token: String,
    client: reqwest::Client,
}

impl HttpControlServiceClient {
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, String> {
        let base_url = base_url.into().trim_end_matches('/').to_owned();
        let bearer_token = bearer_token.into();
        let parsed = reqwest::Url::parse(&base_url)
            .map_err(|error| format!("invalid Control service URL: {error}"))?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || bearer_token.trim().is_empty()
        {
            return Err(
                "Control service requires an http(s) base URL and non-empty bearer token".into(),
            );
        }
        Ok(Self {
            base_url,
            bearer_token,
            client: reqwest::Client::builder()
                .no_proxy()
                .build()
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
            let response = self
                .client
                .post(format!("{}{}", self.base_url, path))
                .bearer_auth(&self.bearer_token)
                .json(command)
                .send()
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
            if response.status() == StatusCode::UNAUTHORIZED {
                return Err("Control service rejected bearer credentials".into());
            }
            let status = response.status();
            match response.json::<Result<O, String>>().await {
                Ok(Ok(value)) if status.is_success() => return Ok(value),
                Ok(Err(error)) => last_error = Some(error),
                Ok(Ok(_)) => last_error = Some(format!("Control service returned {status}")),
                Err(error) => last_error = Some(format!("decode Control response: {error}")),
            }
            if attempt < IDEMPOTENT_ATTEMPTS && status.is_server_error() {
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
    async fn has_vault(&self, id: &str) -> Result<bool, String> {
        self.post(
            VAULT_EXISTS_PATH,
            &VaultExistsCommand {
                vault_id: id.to_owned(),
            },
        )
        .await
    }

    async fn mcp_credential_source_for_url(
        &self,
        vault_ids: &[String],
        url: &str,
    ) -> Result<Option<CredentialSourceId>, String> {
        self.post(
            MCP_SOURCE_PATH,
            &McpSourceCommand {
                vault_ids: vault_ids.to_vec(),
                url: url.to_owned(),
            },
        )
        .await
    }

    async fn mcp_access_for_source(
        &self,
        source_id: &CredentialSourceId,
    ) -> Result<awaken_runtime_contract::CredentialAccess, String> {
        self.post(
            MCP_ACCESS_PATH,
            &SourceCommand {
                source_id: source_id.clone(),
            },
        )
        .await
    }

    async fn credential_access_for_source(
        &self,
        source_id: &CredentialSourceId,
        workspace_id: &str,
        usage: awaken_runtime_contract::CredentialUsage,
        policy: awaken_runtime_contract::CredentialExecutionPolicy,
    ) -> Result<awaken_runtime_contract::CredentialAccess, String> {
        self.post(
            CREDENTIAL_ACCESS_PATH,
            &CredentialAccessCommand {
                source_id: source_id.clone(),
                workspace_id: workspace_id.to_owned(),
                usage,
                policy,
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl awaken_webhook_managed::LifecycleFactDelivery for HttpControlServiceClient {
    async fn deliver(&self, fact: &ManagedLifecycleFact) -> Result<(), String> {
        self.post(WEBHOOK_DELIVER_PATH, fact).await
    }
}

#[async_trait::async_trait]
impl awaken_runtime_contract::DataSubjectConsentSource for HttpControlServiceClient {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingDelivery(Mutex<Vec<ManagedLifecycleFact>>);

    #[async_trait::async_trait]
    impl awaken_webhook_managed::LifecycleFactDelivery for RecordingDelivery {
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
        // unavailable consent reads fail closed to Structured. These rules cover
        // every boundary authority and the authentication gate.
        let audit = ManagementAuditPlane::new(Arc::new(
            awaken_config_store::SqliteConfigStore::open_in_memory()
                .expect("open audit test store"),
        ));
        let credentials = Arc::new(awaken_protocol_managed::VaultState::new(
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
        ));
        let delivery = Arc::new(RecordingDelivery::default());
        let app = router(
            audit,
            credentials,
            delivery.clone(),
            Arc::new(awaken_runtime_contract::NullResolver),
            "correct-token",
        )
        .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve test boundary");
        });
        let client =
            HttpControlServiceClient::new(format!("http://{address}"), "correct-token").unwrap();
        let scope = ScopeId::from("workspace-a");
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
        };
        awaken_webhook_managed::LifecycleFactDelivery::deliver(&client, &fact)
            .await
            .unwrap();
        assert_eq!(*delivery.0.lock().unwrap(), vec![fact], "R2");
        assert!(!client.has_vault("missing").await.unwrap(), "R3");
        assert_eq!(
            awaken_runtime_contract::DataSubjectConsentSource::consent_ceiling(
                &client,
                &awaken_runtime_contract::DataSubjectId("dsub-a".into()),
                awaken_runtime_contract::Purpose::TelemetryContent,
            )
            .await,
            awaken_runtime_contract::ContentCapture::Full,
            "R5"
        );

        let rejected =
            HttpControlServiceClient::new(format!("http://{address}"), "wrong-token").unwrap();
        let mut rejected_record = record;
        rejected_record.call_id = "call-rejected".into();
        assert!(
            rejected.record(&scope, &rejected_record).await.is_err(),
            "R4"
        );
        assert_eq!(
            awaken_runtime_contract::DataSubjectConsentSource::consent_ceiling(
                &rejected,
                &awaken_runtime_contract::DataSubjectId("dsub-a".into()),
                awaken_runtime_contract::Purpose::TelemetryContent,
            )
            .await,
            awaken_runtime_contract::ContentCapture::Structured,
            "R6"
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
