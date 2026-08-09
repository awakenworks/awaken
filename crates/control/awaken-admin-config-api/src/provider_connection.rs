//! Provider-connection application service shared by HTTP and embedded hosts.
//!
//! The service owns the cross-aggregate use case; transports own only scope,
//! authorization, request decoding, and error projection.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_agent_contract::RedactedString;
use awaken_credential_contract::CredentialSourceId;
use awaken_credential_vault::repo::{CredentialRepo, enter_credential_idempotent};
use awaken_credential_vault::{
    CredentialCreateParams, CredentialError, CredentialKind, CredentialSource, CredentialStatus,
    OAuthHelper, SecretStore,
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
    /// Optional operator-authored label used only to make the otherwise opaque
    /// credential source id recognizable in management surfaces. The stable
    /// fingerprint remains the uniqueness/idempotency authority.
    pub credential_name: Option<String>,
    pub dialect: ApiDialect,
    /// Optional qualifier only when this Provider exposes more than one
    /// endpoint speaking the same dialect.
    pub endpoint_name: Option<String>,
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
            .find(|descriptor| descriptor.provider_kind == command.provider_id);
        if descriptor
            .as_ref()
            .is_some_and(|descriptor| !descriptor.supported_dialects.contains(&command.dialect))
        {
            return Err(ProviderConnectionError::UnsupportedDialect {
                provider: command.provider_id.clone(),
                dialect: command.dialect,
            });
        }
        let auth_methods = descriptor.as_ref().map_or_else(
            || installed_dialect_auth_methods(command.dialect),
            |descriptor| descriptor.auth_methods.clone(),
        );
        match &command.authentication {
            ProviderConnectionAuthentication::ApiKey(secret) => {
                if secret.expose_secret().trim().is_empty() {
                    return Err(ProviderConnectionError::Invalid(
                        "vault secret is required".into(),
                    ));
                }
                if !auth_methods.contains(&awaken_model_catalog::ProviderAuthMethod::ApiKey) {
                    return Err(ProviderConnectionError::UnsupportedAuthentication {
                        provider: command.provider_id,
                        method: "API keys",
                    });
                }
            }
            ProviderConnectionAuthentication::OAuth(_) => {
                if !auth_methods.contains(&awaken_model_catalog::ProviderAuthMethod::OAuth) {
                    return Err(ProviderConnectionError::UnsupportedAuthentication {
                        provider: command.provider_id,
                        method: "an OAuth helper",
                    });
                }
            }
            ProviderConnectionAuthentication::Existing(_) => {}
        }

        let endpoint_name = command
            .endpoint_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        if let Some(name) = endpoint_name.as_deref() {
            validate_provider_segment("endpoint_name", name)?;
        }
        let base_url = provider_base_url(&command, descriptor.as_ref())?;
        let provider_id = ProviderId::new(command.provider_id.clone());
        let endpoint_id = ProtocolEndpointId::for_surface(
            &provider_id,
            command.dialect,
            endpoint_name.as_deref(),
        );
        let credential_id = provider_credential_id(&command, &endpoint_id);
        let provider = Provider {
            id: provider_id,
            slug: command.provider_id.clone(),
            display_name: command.display_name.clone(),
            version: 1,
        };
        let endpoint = ProtocolEndpoint {
            id: endpoint_id,
            provider_id: provider.id.clone(),
            dialect: command.dialect,
            base_url,
            timeout_secs: command.timeout_secs,
            display_name: endpoint_name.unwrap_or_else(|| command.dialect.as_str().to_owned()),
            version: 1,
        };
        let discovery_endpoint =
            provider_discovery_endpoint(&command, descriptor.as_ref(), &endpoint);

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
                if credential
                    .protocol_endpoint_id
                    .as_deref()
                    .is_some_and(|current| current != endpoint.id.as_str())
                {
                    return Err(CredentialError::NoCredential.into());
                }
                if credential.is_claude_code_setup_token() {
                    return Err(ProviderConnectionError::UnsupportedAuthentication {
                        provider: command.provider_id,
                        method: "a Claude Code setup token",
                    });
                }
                let existing_method = match credential.kind {
                    CredentialKind::Oauth => awaken_model_catalog::ProviderAuthMethod::OAuth,
                    _ => awaken_model_catalog::ProviderAuthMethod::ApiKey,
                };
                if !auth_methods.contains(&existing_method) {
                    return Err(ProviderConnectionError::UnsupportedAuthentication {
                        provider: command.provider_id,
                        method: if existing_method
                            == awaken_model_catalog::ProviderAuthMethod::OAuth
                        {
                            "an OAuth credential"
                        } else {
                            "an API-key credential"
                        },
                    });
                }
                ConnectionCredential::Existing(Box::new(credential))
            }
        };

        let models = match &authentication {
            ConnectionCredential::ApiKey(secret) => {
                self.discovery
                    .discover_with_secret(&discovery_endpoint, secret)
                    .await
            }
            ConnectionCredential::OAuth(helper) => {
                let probe = CredentialSource {
                    id: CredentialSourceId("cred:provider-connection-probe".into()),
                    workspace_id: command.workspace_id.clone(),
                    kind: CredentialKind::Oauth,
                    provider_id: Some(command.provider_id.clone()),
                    protocol_endpoint_id: Some(endpoint.id.0.clone()),
                    env_key: None,
                    material_ref: None,
                    auxiliary_material_refs: Default::default(),
                    oauth_command: Some(helper.command()),
                    worker_local_binding: None,
                    status: CredentialStatus::Active,
                    version: 1,
                };
                self.discovery.discover(&discovery_endpoint, &probe).await
            }
            ConnectionCredential::Existing(credential) => {
                self.discovery
                    .discover(&discovery_endpoint, credential.as_ref())
                    .await
            }
        }?;
        let models = provider_compatible_models(
            command.dialect,
            command
                .base_url
                .as_deref()
                .is_none_or(|value| value.trim().is_empty()),
            descriptor.as_ref(),
            models,
        );
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
                    Some(endpoint.id.0.clone()),
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
                    Some(endpoint.id.0.clone()),
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

fn provider_credential_id(
    command: &ConnectProviderCommand,
    endpoint_id: &ProtocolEndpointId,
) -> CredentialSourceId {
    let fingerprint = awaken_agent_contract::stable_fingerprint(&(
        "provider-connection/v1",
        &command.workspace_id,
        &command.provider_id,
        endpoint_id,
        &command.idempotency_key,
    ));
    let requested = command
        .credential_name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or(&command.provider_id);
    let label = credential_label(requested);
    CredentialSourceId(format!(
        "cred:{}:model:{}:{label}:{fingerprint}",
        command.workspace_id, command.provider_id
    ))
}

fn credential_label(value: &str) -> String {
    let mut label = String::with_capacity(value.len().min(48));
    let mut separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            label.push(character.to_ascii_lowercase());
            separator = false;
        } else if !separator && !label.is_empty() {
            label.push('-');
            separator = true;
        }
        if label.len() >= 48 {
            break;
        }
    }
    while label.ends_with('-') {
        label.pop();
    }
    if label.is_empty() {
        "credential".into()
    } else {
        label
    }
}

fn provider_base_url(
    command: &ConnectProviderCommand,
    descriptor: Option<&awaken_model_catalog::ProviderDriverDescriptor>,
) -> Result<Option<String>, ProviderConnectionError> {
    if descriptor.is_some_and(|descriptor| descriptor.provider_kind == "vertex") {
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

    let base_url = command
        .base_url
        .as_ref()
        .filter(|url| !url.trim().is_empty())
        .cloned()
        .or_else(|| {
            descriptor?
                .default_endpoints
                .iter()
                .find(|endpoint| endpoint.dialect == command.dialect)
                .map(|endpoint| endpoint.base_url.clone())
        });
    if descriptor.is_none() && base_url.is_none() {
        return Err(ProviderConnectionError::Invalid(
            "base_url is required for a provider without a built-in template".into(),
        ));
    }
    Ok(base_url)
}

fn provider_discovery_endpoint(
    command: &ConnectProviderCommand,
    descriptor: Option<&awaken_model_catalog::ProviderDriverDescriptor>,
    endpoint: &ProtocolEndpoint,
) -> ProtocolEndpoint {
    let discovery_base_url = if command
        .base_url
        .as_ref()
        .is_some_and(|value| !value.trim().is_empty())
    {
        None
    } else {
        descriptor.and_then(|descriptor| {
            descriptor
                .default_endpoints
                .iter()
                .find(|candidate| candidate.dialect == command.dialect)
                .and_then(|candidate| candidate.model_discovery_base_url.as_ref())
        })
    };
    let Some(discovery_base_url) = discovery_base_url else {
        return endpoint.clone();
    };
    let mut discovery = endpoint.clone();
    discovery.base_url = Some(discovery_base_url.clone());
    discovery
}

fn provider_compatible_models(
    dialect: ApiDialect,
    uses_default_endpoint: bool,
    descriptor: Option<&awaken_model_catalog::ProviderDriverDescriptor>,
    mut models: Vec<DiscoveredModel>,
) -> Vec<DiscoveredModel> {
    // An explicitly authored endpoint is operator-owned and may implement a
    // different compatibility set. Built-in endpoints apply their verified
    // protocol/model matrix to the provider-wide directory response.
    if !uses_default_endpoint {
        return models;
    }
    let Some(endpoint) = descriptor.and_then(|descriptor| {
        descriptor
            .default_endpoints
            .iter()
            .find(|endpoint| endpoint.dialect == dialect)
    }) else {
        return models;
    };
    if endpoint.supported_model_ids.is_empty() {
        return models;
    }
    models.retain(|model| endpoint.supported_model_ids.contains(&model.model_id));
    models
}

fn installed_dialect_auth_methods(
    dialect: ApiDialect,
) -> Vec<awaken_model_catalog::ProviderAuthMethod> {
    match dialect {
        ApiDialect::VertexGemini => vec![awaken_model_catalog::ProviderAuthMethod::OAuth],
        ApiDialect::AnthropicMessages
        | ApiDialect::OpenAiChat
        | ApiDialect::OpenAiResponses
        | ApiDialect::Gemini => vec![awaken_model_catalog::ProviderAuthMethod::ApiKey],
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn command(
        provider_id: &str,
        dialect: ApiDialect,
        base_url: Option<&str>,
    ) -> ConnectProviderCommand {
        ConnectProviderCommand {
            idempotency_key: "test".into(),
            workspace_id: "workspace".into(),
            provider_id: provider_id.into(),
            display_name: provider_id.into(),
            credential_name: None,
            dialect,
            endpoint_name: None,
            base_url: base_url.map(str::to_string),
            configuration: Default::default(),
            timeout_secs: 30,
            authentication: ProviderConnectionAuthentication::ApiKey(RedactedString::new("key")),
        }
    }

    #[test]
    fn provider_credential_ids_are_classified_and_human_readable() {
        let command = ConnectProviderCommand {
            credential_name: Some("Production Team / Primary".into()),
            ..command("deepseek", ApiDialect::AnthropicMessages, None)
        };
        let id = provider_credential_id(
            &command,
            &ProtocolEndpointId::for_surface(
                &ProviderId::new("deepseek"),
                ApiDialect::AnthropicMessages,
                None,
            ),
        );
        assert!(
            id.0.starts_with("cred:workspace:model:deepseek:production-team-primary:"),
            "the stable source id carries its model/provider classification and operator label"
        );
    }

    #[test]
    fn templates_supply_defaults_but_do_not_gate_provider_identity() {
        // Causes: C1 built-in template exists; C2 custom identity; C3 explicit
        // base URL; C4 dialect auth contract. Effects: E1 template default;
        // E2 custom endpoint accepted; E3 missing custom endpoint rejected;
        // E4 dialect-specific auth methods. This separates identity, dialect,
        // endpoint, and credential acquisition instead of coupling them in one row.
        let openai = awaken_model_catalog::provider_driver_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.provider_kind == "openai")
            .unwrap();
        assert_eq!(
            provider_base_url(
                &command("openai", ApiDialect::OpenAiChat, None),
                Some(&openai)
            )
            .unwrap()
            .as_deref(),
            Some("https://api.openai.com/v1"),
            "E1"
        );
        assert_eq!(
            provider_base_url(
                &command(
                    "glm",
                    ApiDialect::AnthropicMessages,
                    Some("https://api.example/v1")
                ),
                None,
            )
            .unwrap()
            .as_deref(),
            Some("https://api.example/v1"),
            "E2"
        );
        assert!(
            provider_base_url(&command("glm", ApiDialect::OpenAiChat, None), None).is_err(),
            "E3"
        );
        assert_eq!(
            installed_dialect_auth_methods(ApiDialect::VertexGemini),
            vec![awaken_model_catalog::ProviderAuthMethod::OAuth],
            "E4"
        );
    }

    #[test]
    fn provider_owned_discovery_surface_does_not_replace_inference_base() {
        let deepseek = awaken_model_catalog::provider_driver_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.provider_kind == "deepseek")
            .unwrap();
        let command = command("deepseek", ApiDialect::AnthropicMessages, None);
        let endpoint = ProtocolEndpoint {
            id: ProtocolEndpointId::new("deepseek.anthropic_messages"),
            provider_id: ProviderId::new("deepseek"),
            dialect: ApiDialect::AnthropicMessages,
            base_url: provider_base_url(&command, Some(&deepseek)).unwrap(),
            timeout_secs: 30,
            display_name: "anthropic_messages".into(),
            version: 1,
        };
        let discovery = provider_discovery_endpoint(&command, Some(&deepseek), &endpoint);
        assert_eq!(
            endpoint.base_url.as_deref(),
            Some("https://api.deepseek.com/anthropic")
        );
        assert_eq!(
            discovery.base_url.as_deref(),
            Some("https://api.deepseek.com")
        );
    }

    #[test]
    fn built_in_protocol_filter_does_not_publish_incompatible_discovered_models() {
        let deepseek = awaken_model_catalog::provider_driver_descriptors()
            .into_iter()
            .find(|descriptor| descriptor.provider_kind == "deepseek")
            .unwrap();
        let discovered = vec![
            DiscoveredModel {
                model_id: "deepseek-v4-pro".into(),
                upstream_model: None,
            },
            DiscoveredModel {
                model_id: "deepseek-v4-flash".into(),
                upstream_model: None,
            },
        ];
        let filtered = provider_compatible_models(
            ApiDialect::OpenAiResponses,
            true,
            Some(&deepseek),
            discovered.clone(),
        );
        assert_eq!(filtered, vec![discovered[1].clone()], "built-in surface");

        let custom = provider_compatible_models(
            ApiDialect::OpenAiResponses,
            false,
            Some(&deepseek),
            discovered.clone(),
        );
        assert_eq!(custom, discovered, "explicit endpoint owns compatibility");
    }
}
