//! Provider-connection application service shared by HTTP and embedded hosts.
//!
//! The service owns the cross-aggregate use case; transports own only scope,
//! authorization, request decoding, and error projection.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_vault::repo::{CredentialRepo, enter_credential_idempotent};
use awaken_credential_vault::{
    CredentialCreateParams, CredentialError, CredentialKind, CredentialSource, CredentialSourceId,
    CredentialStatus, OAuthHelper, SecretStore,
};
use awaken_model_catalog::repo::{CatalogRepo, RepoError};
use awaken_model_catalog::{
    ApiDialect, CatalogSyncResult, DiscoveredModel, ProtocolEndpoint, ProtocolEndpointId, Provider,
    ProviderId,
};

#[async_trait::async_trait]
pub trait ModelCatalogDiscovery: Send + Sync {
    async fn discover(
        &self,
        endpoint: &ProtocolEndpoint,
        credential: &CredentialSource,
    ) -> Result<Vec<DiscoveredModel>, ModelCatalogDiscoveryError>;

    async fn discover_with_secret(
        &self,
        _endpoint: &ProtocolEndpoint,
        _secret: &RedactedString,
    ) -> Result<Vec<DiscoveredModel>, ModelCatalogDiscoveryError> {
        Err(ModelCatalogDiscoveryError::CredentialUnavailable(
            "adapter does not support pre-save credential testing".into(),
        ))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ModelCatalogDiscoveryError {
    #[error("credential cannot be materialized by this provisioning adapter: {0}")]
    CredentialUnavailable(String),
    #[error("provider model listing failed: {0}")]
    Provider(String),
}

pub enum ProviderConnectionAuthentication {
    ApiKey(RedactedString),
    OAuth(OAuthHelper),
    Existing(CredentialSourceId),
}

pub struct ConnectProviderCommand {
    /// Stable identity supplied by the command initiator and reused on retry.
    pub idempotency_key: String,
    pub workspace_id: String,
    pub provider_id: String,
    pub display_name: String,
    pub endpoint_id: String,
    pub dialect: ApiDialect,
    pub base_url: Option<String>,
    /// Descriptor-owned non-secret configuration. Provider-specific endpoint
    /// construction stays in this application service, never in a UI client.
    pub configuration: BTreeMap<String, String>,
    pub timeout_secs: u64,
    pub authentication: ProviderConnectionAuthentication,
}

pub struct ProviderConnectionResult {
    pub provider: Provider,
    pub endpoint: ProtocolEndpoint,
    pub credential: CredentialSource,
    pub sync: CatalogSyncResult,
}

#[derive(Debug, thiserror::Error)]
pub enum ProviderConnectionError {
    #[error("provider `{0}` has no installed descriptor")]
    UnsupportedProvider(String),
    #[error("provider `{provider}` does not support {dialect:?}")]
    UnsupportedDialect {
        provider: String,
        dialect: ApiDialect,
    },
    #[error("provider `{provider}` does not accept {method}")]
    UnsupportedAuthentication {
        provider: String,
        method: &'static str,
    },
    #[error("provider connection configuration is invalid: {0}")]
    Invalid(String),
    #[error(transparent)]
    Credential(#[from] CredentialError),
    #[error(transparent)]
    Discovery(#[from] ModelCatalogDiscoveryError),
    #[error("the provider connection succeeded but returned no models")]
    NoModelsDiscovered,
    #[error(transparent)]
    Catalog(#[from] RepoError),
}

#[derive(Clone)]
pub struct ProviderConnectionService {
    catalog: Arc<dyn CatalogRepo>,
    credentials: Arc<dyn CredentialRepo>,
    secrets: Arc<dyn SecretStore>,
    discovery: Arc<dyn ModelCatalogDiscovery>,
}

impl ProviderConnectionService {
    #[must_use]
    pub fn new(
        catalog: Arc<dyn CatalogRepo>,
        credentials: Arc<dyn CredentialRepo>,
        secrets: Arc<dyn SecretStore>,
        discovery: Arc<dyn ModelCatalogDiscovery>,
    ) -> Self {
        Self {
            catalog,
            credentials,
            secrets,
            discovery,
        }
    }

    pub async fn connect(
        &self,
        command: ConnectProviderCommand,
    ) -> Result<ProviderConnectionResult, ProviderConnectionError> {
        if command.idempotency_key.trim().is_empty() || command.idempotency_key.len() > 200 {
            return Err(ProviderConnectionError::Invalid(
                "idempotency_key must contain 1 to 200 characters".into(),
            ));
        }
        let descriptor = awaken_model_catalog::provider_driver_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.provider_kind == command.provider_id)
            .ok_or_else(|| {
                ProviderConnectionError::UnsupportedProvider(command.provider_id.clone())
            })?;
        if !descriptor.supported_dialects.contains(&command.dialect) {
            return Err(ProviderConnectionError::UnsupportedDialect {
                provider: command.provider_id,
                dialect: command.dialect,
            });
        }
        match &command.authentication {
            ProviderConnectionAuthentication::ApiKey(secret) => {
                if secret.expose_secret().trim().is_empty() {
                    return Err(ProviderConnectionError::Invalid(
                        "vault secret is required".into(),
                    ));
                }
                if !descriptor
                    .auth_methods
                    .contains(&awaken_model_catalog::ProviderAuthMethod::ApiKey)
                {
                    return Err(ProviderConnectionError::UnsupportedAuthentication {
                        provider: command.provider_id,
                        method: "API keys",
                    });
                }
            }
            ProviderConnectionAuthentication::OAuth(_) => {
                if !descriptor
                    .auth_methods
                    .contains(&awaken_model_catalog::ProviderAuthMethod::OAuth)
                {
                    return Err(ProviderConnectionError::UnsupportedAuthentication {
                        provider: command.provider_id,
                        method: "an OAuth helper",
                    });
                }
            }
            ProviderConnectionAuthentication::Existing(_) => {}
        }

        let base_url = provider_base_url(&command, &descriptor)?;
        let credential_id = provider_credential_id(&command);
        let provider = Provider {
            id: ProviderId::new(command.provider_id.clone()),
            slug: command.provider_id.clone(),
            display_name: command.display_name,
            version: 1,
        };
        let endpoint = ProtocolEndpoint {
            id: ProtocolEndpointId::new(command.endpoint_id),
            provider_id: provider.id.clone(),
            dialect: command.dialect,
            base_url,
            timeout_secs: command.timeout_secs,
            display_name: format!("{} · {:?}", provider.display_name, command.dialect),
            version: 1,
        };

        enum ConnectionCredential {
            ApiKey(RedactedString),
            OAuth(OAuthHelper),
            Existing(Box<CredentialSource>),
        }

        let authentication = match command.authentication {
            ProviderConnectionAuthentication::ApiKey(secret) => {
                ConnectionCredential::ApiKey(secret)
            }
            ProviderConnectionAuthentication::OAuth(helper) => ConnectionCredential::OAuth(helper),
            ProviderConnectionAuthentication::Existing(id) => {
                let credential = self.credentials.get(&id).await?;
                if credential.workspace_id != command.workspace_id {
                    return Err(CredentialError::SourceNotFound(credential.id.0).into());
                }
                if credential.status != CredentialStatus::Active {
                    return Err(CredentialError::NotActive(credential.id.0).into());
                }
                if credential
                    .provider_id
                    .as_deref()
                    .is_some_and(|provider_id| provider_id != command.provider_id)
                {
                    return Err(CredentialError::NoCredential.into());
                }
                if credential.is_claude_code_setup_token() {
                    return Err(ProviderConnectionError::UnsupportedAuthentication {
                        provider: command.provider_id,
                        method: "a Claude Code setup token",
                    });
                }
                ConnectionCredential::Existing(Box::new(credential))
            }
        };

        let models = match &authentication {
            ConnectionCredential::ApiKey(secret) => {
                self.discovery.discover_with_secret(&endpoint, secret).await
            }
            ConnectionCredential::OAuth(helper) => {
                let probe = CredentialSource {
                    id: CredentialSourceId("cred:provider-connection-probe".into()),
                    workspace_id: command.workspace_id.clone(),
                    kind: CredentialKind::Oauth,
                    provider_id: Some(command.provider_id.clone()),
                    env_key: None,
                    material_ref: None,
                    auxiliary_material_refs: Default::default(),
                    oauth_command: Some(helper.command()),
                    worker_local_binding: None,
                    status: CredentialStatus::Active,
                    version: 1,
                };
                self.discovery.discover(&endpoint, &probe).await
            }
            ConnectionCredential::Existing(credential) => {
                self.discovery
                    .discover(&endpoint, credential.as_ref())
                    .await
            }
        }?;
        if models.is_empty() {
            return Err(ProviderConnectionError::NoModelsDiscovered);
        }

        let (mut credential, command_owned, created) = match authentication {
            ConnectionCredential::Existing(source) => (*source, false, false),
            ConnectionCredential::ApiKey(secret) => {
                let entry = enter_credential_idempotent(
                    credential_id,
                    CredentialCreateParams {
                        workspace_id: command.workspace_id.clone(),
                        kind: CredentialKind::Vault,
                        provider_id: Some(provider.id.0.clone()),
                        env_key: None,
                        secret: Some(secret),
                        oauth_command: None,
                    },
                    self.secrets.as_ref(),
                    self.credentials.as_ref(),
                )
                .await?;
                (entry.source, true, entry.created)
            }
            ConnectionCredential::OAuth(helper) => {
                let entry = enter_credential_idempotent(
                    credential_id,
                    CredentialCreateParams {
                        workspace_id: command.workspace_id.clone(),
                        kind: CredentialKind::Oauth,
                        provider_id: Some(provider.id.0.clone()),
                        env_key: None,
                        secret: None,
                        oauth_command: Some(helper.command()),
                    },
                    self.secrets.as_ref(),
                    self.credentials.as_ref(),
                )
                .await?;
                (entry.source, true, entry.created)
            }
        };
        if created {
            credential = awaken_credential_vault::repo::transition_credential_status(
                &credential.id,
                CredentialStatus::Disabled,
                self.credentials.as_ref(),
            )
            .await?;
        }
        let sync = self
            .catalog
            .put_discovered_connection(provider.clone(), endpoint.clone(), models, unix_time_ms())
            .await?;
        if command_owned && credential.status != CredentialStatus::Active {
            credential = awaken_credential_vault::repo::transition_credential_status(
                &credential.id,
                CredentialStatus::Active,
                self.credentials.as_ref(),
            )
            .await?;
        }
        Ok(ProviderConnectionResult {
            provider,
            endpoint,
            credential,
            sync,
        })
    }
}

fn provider_credential_id(command: &ConnectProviderCommand) -> CredentialSourceId {
    let fingerprint = awaken_agent_contract::stable_fingerprint(&(
        "provider-connection/v1",
        &command.workspace_id,
        &command.provider_id,
        &command.endpoint_id,
        &command.idempotency_key,
    ));
    CredentialSourceId(format!("cred:{}:{fingerprint}", command.workspace_id))
}

fn provider_base_url(
    command: &ConnectProviderCommand,
    descriptor: &awaken_model_catalog::ProviderDriverDescriptor,
) -> Result<Option<String>, ProviderConnectionError> {
    if descriptor.provider_kind == "vertex" {
        let project = required_provider_segment(&command.configuration, "project_id")?;
        let location = command
            .configuration
            .get("location")
            .map(String::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or("global");
        validate_provider_segment("location", location)?;
        let host = if location == "global" {
            "aiplatform.googleapis.com".to_string()
        } else {
            format!("{location}-aiplatform.googleapis.com")
        };
        return Ok(Some(format!(
            "https://{host}/v1/projects/{project}/locations/{location}/"
        )));
    }

    Ok(command
        .base_url
        .as_ref()
        .filter(|url| !url.trim().is_empty())
        .cloned()
        .or_else(|| {
            descriptor
                .default_endpoints
                .iter()
                .find(|endpoint| endpoint.dialect == command.dialect)
                .map(|endpoint| endpoint.base_url.clone())
        }))
}

fn required_provider_segment<'a>(
    configuration: &'a BTreeMap<String, String>,
    key: &str,
) -> Result<&'a str, ProviderConnectionError> {
    let value = configuration
        .get(key)
        .map(String::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| ProviderConnectionError::Invalid(format!("{key} is required")))?;
    validate_provider_segment(key, value)?;
    Ok(value)
}

fn validate_provider_segment(key: &str, value: &str) -> Result<(), ProviderConnectionError> {
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Ok(())
    } else {
        Err(ProviderConnectionError::Invalid(format!(
            "{key} contains unsupported characters"
        )))
    }
}

fn unix_time_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};

    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}
