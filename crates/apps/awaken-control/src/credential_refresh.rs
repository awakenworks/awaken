// @formal surface obligation=credential.rotation_revision_cas
// @formal surface obligation=credential_reference.exact_revision_never_resolves_another_revision
// @formal reason: Refresh path preserves revision-bound credential identity.
//! Exact OAuth refresh and Vault rotation adapter.

use std::sync::Arc;

use awaken_agent_contract::credential::{AuthChallenge, Credential, CredentialRefresher};
use awaken_agent_contract::credential::{CredentialRefreshAccess, TokenEndpointAuth};
use awaken_credential_contract::CredentialRefreshFactory;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::repo::CredentialRepo;
use awaken_credential_vault::{
    CredentialStatus, OAUTH_CLIENT_SECRET_SLOT, OAUTH_REFRESH_TOKEN_SLOT, SecretStore,
};
use awaken_session_configuration_contract::{
    CredentialRefreshExchange, ProbeHttpResponse, RefreshExchangeOutcome, RefreshExchangeStatus,
};
use base64::Engine as _;

#[derive(Clone)]
pub struct VaultRefreshFactory {
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    clock: Arc<dyn awaken_time::WallClock>,
    coordinator: Arc<awaken_credential_vault::token_acquisition::TokenAcquisitionCoordinator>,
}

impl VaultRefreshFactory {
    #[must_use]
    pub fn new(
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            coordinator: Arc::new(
                awaken_credential_vault::token_acquisition::TokenAcquisitionCoordinator::new(
                    clock.clone(),
                ),
            ),
            credentials,
            secrets,
            clock,
        }
    }
}

#[async_trait::async_trait]
impl awaken_admin_config_api::CredentialGrantRenewal for VaultRefreshFactory {
    async fn inspect(
        &self,
        workspace: &str,
        source: &awaken_agent_contract::credential::CredentialRef,
        min_validity_ms: u64,
    ) -> Result<Option<awaken_credential_contract::CredentialRenewalStatus>, TokenAcquisitionError>
    {
        self.inspect_described(workspace, source, min_validity_ms)
            .await
    }

    async fn renew(
        &self,
        workspace: &str,
        source: &awaken_agent_contract::credential::CredentialRef,
        replacement: CredentialSourceId,
    ) -> Result<awaken_credential_vault::CredentialSource, TokenAcquisitionError> {
        self.renew_described(workspace, source, replacement).await
    }
}

impl CredentialRefreshFactory for VaultRefreshFactory {
    fn refresher(
        &self,
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
    ) -> Arc<dyn CredentialRefresher> {
        Arc::new(VaultRefresher::from_factory(
            credential_id,
            access,
            self.clone(),
        ))
    }

    fn bearer_reloader(
        &self,
        credential_id: CredentialSourceId,
        credential_revision: u64,
    ) -> Arc<dyn CredentialRefresher> {
        Arc::new(VaultBearerReloader {
            credential_id,
            credential_revision,
            credentials: self.credentials.clone(),
            secrets: self.secrets.clone(),
        })
    }
}

struct VaultBearerReloader {
    credential_id: CredentialSourceId,
    credential_revision: u64,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
}

#[async_trait::async_trait]
impl CredentialRefresher for VaultBearerReloader {
    async fn refresh(&self, _challenge: &AuthChallenge) -> Option<Credential> {
        let source = self.credentials.get(&self.credential_id).await.ok()?;
        if source.status != CredentialStatus::Active
            || u64::try_from(source.version).ok()? != self.credential_revision
        {
            return None;
        }
        let bearer = self.secrets.get(source.material_ref.as_ref()?).await.ok()?;
        Some(Credential::Bearer(bearer.expose_secret().to_owned()))
    }
}

pub struct VaultRefresher {
    credential_id: CredentialSourceId,
    access: tokio::sync::Mutex<CredentialRefreshAccess>,
    factory: VaultRefreshFactory,
}
impl VaultRefresher {
    #[must_use]
    pub fn new(
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self::from_factory(
            credential_id,
            access,
            VaultRefreshFactory::new(credentials, secrets, clock),
        )
    }
    fn from_factory(
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
        factory: VaultRefreshFactory,
    ) -> Self {
        Self {
            credential_id,
            access: tokio::sync::Mutex::new(access),
            factory,
        }
    }
}

mod acquisition;
use awaken_credential_contract::{CredentialTokenAcquisition, TokenAcquisitionError};
use awaken_time::WallClockExt as _;

fn access_for_source(
    source: &awaken_credential_vault::CredentialSource,
    old: &CredentialRefreshAccess,
    expiry: Option<u64>,
) -> CredentialRefreshAccess {
    CredentialRefreshAccess::new(
        u64::try_from(source.version).unwrap_or(0),
        old.token_endpoint.clone(),
        old.client_id.clone(),
        old.token_endpoint_auth,
        source
            .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
            .map(|v| v.0.clone()),
        source
            .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
            .map(|v| v.0.clone())
            .unwrap_or_default(),
        source
            .material_ref
            .as_ref()
            .map(|v| v.0.clone())
            .unwrap_or_default(),
        old.scope.clone(),
        old.resource.clone(),
    )
    .with_provider_metadata(old.account_id.clone(), old.account_plan.clone(), expiry)
}

impl VaultRefreshFactory {
    /// Observe only one exact described generation; no token material leaves
    /// this authority and no observation dispatches an issuer request.
    pub async fn inspect_described(
        &self,
        workspace: &str,
        reference: &awaken_agent_contract::credential::CredentialRef,
        min_validity_ms: u64,
    ) -> Result<Option<awaken_credential_contract::CredentialRenewalStatus>, TokenAcquisitionError>
    {
        use awaken_credential_contract::CredentialRenewalStatus as Status;
        use awaken_credential_vault::repo::CredentialAcquisitionPhase as Phase;
        let source = self
            .credentials
            .get(&CredentialSourceId(reference.id.clone()))
            .await
            .map_err(|_| TokenAcquisitionError::AuthorityDenied)?;
        if source.workspace_id != workspace
            || source.status != CredentialStatus::Active
            || u64::try_from(source.version).ok() != Some(reference.revision)
        {
            return Err(TokenAcquisitionError::AuthorityDenied);
        }
        let Some(profile) = source
            .descriptor
            .as_ref()
            .and_then(|v| v.acquisition.as_ref())
        else {
            return Ok(None);
        };
        if let Some(pending) = self
            .credentials
            .pending_acquisition(&source.id)
            .await
            .map_err(|_| TokenAcquisitionError::PublicationFailed)?
        {
            let acquisition = pending
                .acquisition
                .as_ref()
                .ok_or(TokenAcquisitionError::PublicationFailed)?;
            return Ok(Some(match acquisition.phase {
                Phase::Rejected => Status::NeedsAuthorization,
                Phase::OutcomeUnknown => Status::OutcomeUnknown,
                Phase::Dispatched
                    if pending.material_fence.writer_lease_expires_at_unix_ms
                        <= self.clock.now_ms() =>
                {
                    Status::OutcomeUnknown
                }
                _ => Status::InProgress,
            }));
        }
        let state =
            awaken_credential_vault::repo::token_publication_state(&source, self.secrets.as_ref())
                .await
                .map_err(|_| TokenAcquisitionError::PublicationFailed)?;
        let Some(state) = state else {
            return Ok(Some(Status::Due));
        };
        if profile.fingerprint().ok().as_ref() != Some(&state.profile_fingerprint) {
            return Err(TokenAcquisitionError::AuthorityDenied);
        }
        let now = self.clock.now_ms();
        if state.refresh_expires_at_unix_ms.is_some_and(|at| at <= now) {
            return Ok(Some(Status::NeedsAuthorization));
        }
        let Some(expires_at) = state.access_expires_at_unix_ms else {
            return Ok(Some(Status::ExpiryUnknown));
        };
        let window = min_validity_ms.max(profile.response.refresh_margin_ms);
        if state.predecessor.is_some()
            && expires_at.saturating_sub(state.issued_at_unix_ms) <= window
        {
            return Ok(Some(Status::InsufficientLifetime));
        }
        Ok(Some(if expires_at <= now.saturating_add(window) {
            Status::Due
        } else {
            Status::Ready
        }))
    }
    /// Renew one described grant into an exact replacement for its governance
    /// owner. The caller receives only a source receipt, never issued material.
    pub async fn renew_described(
        &self,
        workspace_id: &str,
        source_ref: &awaken_agent_contract::credential::CredentialRef,
        replacement: CredentialSourceId,
    ) -> Result<awaken_credential_vault::CredentialSource, TokenAcquisitionError> {
        let source = self
            .credentials
            .get(&CredentialSourceId(source_ref.id.clone()))
            .await
            .map_err(|_| TokenAcquisitionError::AuthorityDenied)?;
        if source.workspace_id != workspace_id
            || u64::try_from(source.version).ok() != Some(source_ref.revision)
            || source.status != CredentialStatus::Active
        {
            return Err(TokenAcquisitionError::AuthorityDenied);
        }
        let profile = source
            .descriptor
            .as_ref()
            .and_then(|v| v.acquisition.clone())
            .ok_or(TokenAcquisitionError::InvalidContract)?;
        let port = acquisition::VaultAcquisitionPort::new(
            self,
            source,
            profile.clone(),
            Some(replacement),
        )?;
        self.coordinator
            .acquire(None, &profile.response, &port)
            .await?;
        Ok(port.source.lock().await.clone())
    }
    #[must_use]
    pub fn token_acquisition(&self) -> Arc<dyn CredentialTokenAcquisition> {
        self.coordinator.clone()
    }
}

/// A token endpoint that never answers must not pin a refresh attempt open.
const OAUTH_REFRESH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

fn basic_client_auth(client_id: &str, client_secret: &str) -> String {
    let encode =
        |value: &str| form_urlencoded::byte_serialize(value.as_bytes()).collect::<String>();
    let pair = format!("{}:{}", encode(client_id), encode(client_secret));
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(pair)
    )
}

fn http_client_for(url: &str) -> reqwest::Client {
    let mut builder = awaken_outbound_http::rustls_client_builder(
        awaken_outbound_http::TransportDeadline::total(OAUTH_REFRESH_TIMEOUT),
    );
    if awaken_outbound_http::target_is_loopback(url) {
        builder = builder.no_proxy();
    }
    builder.build().expect("build OAuth HTTP client")
}

/// What one exact exchange did: the reportable outcome plus, on success, the
/// resealed bearer the transport retries with.
struct ExchangeReport {
    outcome: RefreshExchangeOutcome,
    bearer: Option<String>,
}

impl ExchangeReport {
    fn failed(http_response: Option<ProbeHttpResponse>) -> Self {
        Self {
            outcome: RefreshExchangeOutcome {
                status: RefreshExchangeStatus::Failed,
                http_response,
            },
            bearer: None,
        }
    }

    fn connect_error() -> Self {
        Self {
            outcome: RefreshExchangeOutcome {
                status: RefreshExchangeStatus::ConnectError,
                http_response: None,
            },
            bearer: None,
        }
    }
}

impl VaultRefresher {
    /// Run one exact refresh-token exchange for the pinned revision, reseal the
    /// issued token on success, and report how it went. The token endpoint's
    /// response is reported scrubbed of every token and client secret.
    pub async fn exchange(&self) -> RefreshExchangeOutcome {
        self.run_exchange().await.outcome
    }

    async fn run_exchange(&self) -> ExchangeReport {
        let mut access = self.access.lock().await;
        if !access.has_valid_client_authentication_binding()
            || !access.has_valid_configuration_fingerprint()
        {
            return ExchangeReport::failed(None);
        }
        let Ok(source) = self.factory.credentials.get(&self.credential_id).await else {
            return ExchangeReport::failed(None);
        };
        if source.status != CredentialStatus::Active {
            return ExchangeReport::failed(None);
        }
        let profile = source
            .descriptor
            .as_ref()
            .and_then(|v| v.acquisition.clone())
            .unwrap_or_else(|| acquisition::standard_profile(&access));
        if profile.token_endpoint != access.token_endpoint {
            return ExchangeReport::failed(None);
        }
        let exact = u64::try_from(source.version).ok() == Some(access.credential_revision)
            && source.material_ref.as_ref().map(|v| &v.0) == Some(&access.access_token_ref)
            && source
                .auxiliary_material_ref(OAUTH_REFRESH_TOKEN_SLOT)
                .map(|v| &v.0)
                == Some(&access.refresh_token_ref)
            && access.client_secret_ref.as_ref()
                == source
                    .auxiliary_material_ref(OAUTH_CLIENT_SECRET_SLOT)
                    .map(|v| &v.0);
        if !exact {
            // Explicit refresh adoption requires a direct publication receipt;
            // a newer row alone never authorizes reading another generation.
            let state = awaken_credential_vault::repo::token_publication_state(
                &source,
                self.factory.secrets.as_ref(),
            )
            .await
            .ok()
            .flatten();
            let parent = awaken_agent_contract::credential::CredentialRef {
                id: self.credential_id.0.clone(),
                revision: access.credential_revision,
            };
            if !state.as_ref().is_some_and(|v| {
                v.predecessor.as_ref() == Some(&parent)
                    && profile.fingerprint().ok().as_ref() == Some(&v.profile_fingerprint)
            }) {
                return ExchangeReport::failed(None);
            }
            let expiry = state.as_ref().and_then(|v| v.access_expires_at_unix_ms);
            *access = access_for_source(&source, &access, expiry);
            if expiry.is_none_or(|at| at > self.factory.clock.now_ms()) {
                let Ok(port) =
                    acquisition::VaultAcquisitionPort::new(&self.factory, source, profile, None)
                else {
                    return ExchangeReport::failed(None);
                };
                if let Ok(token) = port.access_token(&port.source.lock().await.clone()).await {
                    return ExchangeReport {
                        outcome: RefreshExchangeOutcome {
                            status: RefreshExchangeStatus::Succeeded,
                            http_response: None,
                        },
                        bearer: Some(token.expose_secret().to_owned()),
                    };
                }
                return ExchangeReport::failed(None);
            }
        }
        let Ok(port) =
            acquisition::VaultAcquisitionPort::new(&self.factory, source, profile.clone(), None)
        else {
            return ExchangeReport::failed(None);
        };
        let result = self
            .factory
            .coordinator
            .acquire(None, &profile.response, &port)
            .await;
        let published = port.source.lock().await.clone();
        if published.version > i64::try_from(access.credential_revision).unwrap_or(i64::MAX) {
            let expiry = result.as_ref().ok().and_then(|v| v.expires_at_unix_ms);
            *access = access_for_source(&published, &access, expiry);
        }
        match result {
            Ok(token) => ExchangeReport {
                outcome: RefreshExchangeOutcome {
                    status: RefreshExchangeStatus::Succeeded,
                    http_response: port.probe(),
                },
                bearer: Some(token.access_token.expose_secret().to_owned()),
            },
            Err(TokenAcquisitionError::IssuerUnavailable) => ExchangeReport::connect_error(),
            Err(_) => ExchangeReport::failed(port.probe()),
        }
    }
}

#[async_trait::async_trait]
impl CredentialRefresher for VaultRefresher {
    async fn refresh(&self, _challenge: &AuthChallenge) -> Option<Credential> {
        self.run_exchange().await.bearer.map(Credential::Bearer)
    }
}

#[async_trait::async_trait]
impl CredentialRefreshExchange for VaultRefreshFactory {
    async fn exchange(
        &self,
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
    ) -> RefreshExchangeOutcome {
        VaultRefresher::new(
            credential_id,
            access,
            self.credentials.clone(),
            self.secrets.clone(),
            self.clock.clone(),
        )
        .exchange()
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_credential_vault::SecretRef;

    #[tokio::test]
    async fn tampered_exact_refresh_configuration_fails_before_secret_or_network_access() {
        // Causes: C1 exact fingerprint remains valid; C2 an executable OAuth
        // fact changes. Effects: E1 exact revision may refresh; E2 tampering
        // fails before Vault/network access. R1 C1&&!C2 -> E1 is covered by the
        // MCP integration suite; R2 C2 -> E2 is isolated here.
        let mut access = CredentialRefreshAccess::new(
            1,
            "https://auth.example/token".into(),
            "client".into(),
            TokenEndpointAuth::None,
            None,
            "refresh-ref".into(),
            "access-ref".into(),
            None,
            None,
        );
        access.scope = Some("tampered".into());
        let refresher = VaultRefresher::new(
            CredentialSourceId("cred:test".into()),
            access,
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            awaken_time::system_clock(),
        );
        assert_eq!(
            refresher
                .refresh(&AuthChallenge {
                    status: 401,
                    www_authenticate: None,
                })
                .await,
            None,
            "R2/E2"
        );
    }

    #[tokio::test]
    async fn pinned_bearer_reload_decision_table() {
        // Bearer-reload FMECA / cause-effect graph: C1 the frozen credential id
        // exists; C2 lifecycle is Active; C3 revision equals the Session pin;
        // C4 its exact material reference resolves. Effect E1 returns only the
        // current bytes behind that pin; every other combination yields E2 None
        // before retry. F1 identity widening (S10,O3,D2,RPN60), F2 revoked-use
        // (S10,O3,D2,RPN60), and F3 missing material (S8,O4,D2,RPN64) fail closed.
        //
        // | Rule | exists | active | revision | material | Effect |
        // |---|---|---|---|---|---|
        // | B1 | yes | yes | exact | present | E1 |
        // | B2 | yes | no | exact | present | E2/F2 |
        // | B3 | yes | yes | changed | present | E2/F1 |
        // | B4 | yes | yes | exact | missing | E2/F3 |
        // | B5 | no | any | any | any | E2 |
        let secret_ref = SecretRef("secret:run-mcp".into());
        let secrets: Arc<dyn SecretStore> =
            Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        secrets
            .put(&secret_ref, RedactedString::new("current-bearer"))
            .await
            .unwrap();
        let credentials: Arc<dyn CredentialRepo> =
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let source = awaken_credential_vault::CredentialSource {
            id: CredentialSourceId("cred:run-mcp".into()),
            replacement_of: None,
            workspace_id: "workspace".into(),
            kind: awaken_credential_vault::CredentialKind::Vault,
            descriptor: None,
            provider_id: Some("awaken-flow/run-mcp".into()),
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: Some(secret_ref.clone()),
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: None,
            status: CredentialStatus::Active,
            version: 1,
        };
        credentials.put(source.clone()).await.unwrap();
        let factory = VaultRefreshFactory::new(
            credentials.clone(),
            secrets.clone(),
            awaken_time::system_clock(),
        );
        let challenge = AuthChallenge {
            status: 401,
            www_authenticate: None,
        };
        assert_eq!(
            CredentialRefreshFactory::bearer_reloader(&factory, source.id.clone(), 1,)
                .refresh(&challenge)
                .await,
            Some(Credential::Bearer("current-bearer".into())),
            "B1/E1"
        );

        for (case, replacement) in [
            (
                "B2/E2/F2",
                awaken_credential_vault::CredentialSource {
                    status: CredentialStatus::Disabled,
                    ..source.clone()
                },
            ),
            (
                "B3/E2/F1",
                awaken_credential_vault::CredentialSource {
                    version: 2,
                    ..source.clone()
                },
            ),
            (
                "B4/E2/F3",
                awaken_credential_vault::CredentialSource {
                    material_ref: Some(SecretRef("secret:missing".into())),
                    ..source.clone()
                },
            ),
        ] {
            let case_credentials: Arc<dyn CredentialRepo> =
                Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
            case_credentials.put(replacement).await.unwrap();
            let case_factory = VaultRefreshFactory::new(
                case_credentials,
                secrets.clone(),
                awaken_time::system_clock(),
            );
            assert_eq!(
                CredentialRefreshFactory::bearer_reloader(&case_factory, source.id.clone(), 1,)
                    .refresh(&challenge)
                    .await,
                None,
                "{case}"
            );
        }
        assert_eq!(
            CredentialRefreshFactory::bearer_reloader(
                &factory,
                CredentialSourceId("cred:missing".into()),
                1,
            )
            .refresh(&challenge)
            .await,
            None,
            "B5/E2"
        );
    }
}
