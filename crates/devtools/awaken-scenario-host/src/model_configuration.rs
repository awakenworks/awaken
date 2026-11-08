//! Scenario model upstream, configured only through the config-plane API.
//!
//! A scenario process publishes its fixture Agents on a deterministic
//! in-process model. An operator points them at a real provider the way the
//! console does, over HTTP and never through the process environment:
//!
//! 1. `POST /v1/config/provider-connections` discovers the provider's offerings
//!    and seals its key in the credential vault;
//! 2. `PUT /v1/config/inference-profiles/{id}` binds a model and credential.
//!
//! Each inference then resolves the bound profile through the production
//! publication resolver and realizes it through the vault-backed credential
//! materializer — the chain a published Agent uses in the product.
//!
//! Binding decision table, for one inference whose binding names model `R`:
//!
//! | Rule | profile `R` | profile `default` | Effect                               |
//! |------|-------------|-------------------|--------------------------------------|
//! | B1   | authored    | any               | resolve profile `R`                  |
//! | B2   | absent      | authored          | resolve profile `default`            |
//! | B3   | absent      | absent            | the deterministic in-process model   |
//! | B4   | authored but fails to resolve   || the inference fails, naming the cause |
//!
//! B1 lets one fixture route several labels to distinct upstream models; B2 is
//! the ordinary "this scenario's Agents use that model" binding; B3 keeps an
//! unconfigured scenario deterministic; B4 never falls back silently.

use std::sync::{Arc, OnceLock};

use awaken_agent_config::ModelSelection;
use awaken_runtime_contract::llm::{
    ChatRequest, ChatResponse, DeltaSink, Error as LlmError, LlmExecutor,
};

/// The profile every scenario Agent falls back to (rule B2).
pub const DEFAULT_SCENARIO_PROFILE: &str = "default";

/// The configuration ports one scenario process resolves its model through.
/// They are the same ports the admin config API writes.
#[derive(Clone)]
pub struct ScenarioModelConfiguration {
    catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
    credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    secrets: Arc<dyn awaken_credential_vault::SecretStore>,
    profiles: Arc<dyn awaken_config_resolver::InferenceProfileStore>,
    workspace: String,
    clock: Arc<dyn awaken_time::WallClock>,
}

impl ScenarioModelConfiguration {
    /// Resolve over configuration ports a process already owns (an AllInOne
    /// process's Control stores).
    #[must_use]
    pub fn over(
        catalog: Arc<dyn awaken_model_catalog::repo::CatalogRepo>,
        credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
        secrets: Arc<dyn awaken_credential_vault::SecretStore>,
        profiles: Arc<dyn awaken_config_resolver::InferenceProfileStore>,
        workspace: String,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self {
            catalog,
            credentials,
            secrets,
            profiles,
            workspace,
            clock,
        }
    }

    /// Fresh in-memory configuration for a scenario process that has no
    /// Control plane of its own; [`Self::router`] is its only writer.
    #[must_use]
    pub fn ephemeral(workspace: String, clock: Arc<dyn awaken_time::WallClock>) -> Self {
        Self::over(
            Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new()),
            Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
            Arc::new(awaken_credential_vault::InMemorySecretStore::new()),
            Arc::new(awaken_config_resolver::InMemoryProfileStore::new()),
            workspace,
            clock,
        )
    }

    /// The configuration of a scenario process without a Control plane, in
    /// the installation Workspace its Sessions resolve for `deployment`.
    #[must_use]
    pub fn for_scenario(
        deployment: &awaken_deployment_config::DeploymentConfig,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Self {
        Self::ephemeral(crate::deployment::installation_workspace(deployment), clock)
    }

    /// The production admin config API over these ports, scoped to the
    /// scenario Workspace, for a process that does not mount its own.
    pub fn router(&self) -> axum::Router {
        let admin = awaken_admin_config_api::admin_router(awaken_admin_config_api::AdminState {
            catalog: self.catalog.clone(),
            credentials: self.credentials.clone(),
            secrets: self.secrets.clone(),
            profiles: self.profiles.clone(),
            resources: Arc::new(awaken_config_resolver::InMemoryAgentInputBindingRepository::new()),
            probe: None,
            grant_renewal: None,
            model_discovery: Some(Arc::new(
                awaken_control::model_discovery::GenaiModelDiscovery::new(self.secrets.clone()),
            )),
            brokered_catalog: None,
            availability: Default::default(),
            clock: self.clock.clone(),
        });
        awaken_coordinator::workspace_path::with_platform_workspace(admin, self.workspace.clone())
    }

    /// The profile an inference for `model_ref` binds to (rules B1–B3).
    async fn bound_profile(&self, model_ref: &str) -> Result<Option<String>, String> {
        for profile_id in [model_ref, DEFAULT_SCENARIO_PROFILE] {
            let authored = awaken_config_resolver::get_workspace_profile(
                self.profiles.as_ref(),
                &self.workspace,
                profile_id,
            )
            .await
            .map_err(|error| format!("read inference profile `{profile_id}`: {error}"))?;
            if authored.is_some() {
                return Ok(Some(profile_id.to_owned()));
            }
        }
        Ok(None)
    }

    /// Publish the bound profile's candidate and realize it through the vault,
    /// exactly as a published Agent's attempt does, returning the executor
    /// with the candidate's binding. `None` is rule B3.
    async fn executor_for(&self, model_ref: &str) -> Result<Option<BoundExecutor>, String> {
        let Some(profile_id) = self.bound_profile(model_ref).await? else {
            return Ok(None);
        };
        let (policy, holder) = crate::attempt_credential::direct_provider_execution();
        let resolver =
            awaken_control::model_publication::CatalogModelPublicationResolver::from_repo(
                self.catalog.clone(),
                self.credentials.clone(),
                self.clock.clone(),
            )
            .with_profiles(self.profiles.clone())
            .with_direct_provider_credential_execution(policy, holder);
        let published = awaken_config_service::ModelPublicationResolver::resolve_models(
            &resolver,
            &awaken_agent_contract::ScopeId::from(self.workspace.as_str()),
            &ModelSelection::Profile {
                profile_id: profile_id.clone(),
            },
            &[],
        )
        .await
        .map_err(|error| format!("resolve inference profile `{profile_id}`: {error}"))?;
        let materializer = awaken_credential_materializer::CredentialInferenceMaterializer::new(
            Arc::new(awaken_credential_vault::VaultLocalMaterialAuthority::new(
                self.credentials.clone(),
                self.secrets.clone(),
                self.clock.clone(),
            )),
            self.clock.clone(),
        );
        let context = crate::attempt_credential::context(&published.primary);
        materializer
            .materialize_candidate(&published.primary, &context)
            .await
            .map(|executor| {
                Some(BoundExecutor {
                    executor,
                    binding: published.primary.binding().clone(),
                })
            })
            .ok_or_else(|| {
                format!(
                    "inference profile `{profile_id}` resolved a candidate the vault cannot realize"
                )
            })
    }
}

/// A realized profile candidate. The inference runs under the candidate's own
/// binding: a fixture Agent's binding names its scenario backend, not the
/// operator's provider, so the request is rebound before provider projection.
struct BoundExecutor {
    executor: Arc<dyn LlmExecutor>,
    binding: awaken_runtime_contract::resolved::ModelBinding,
}

static INSTALLED: OnceLock<ScenarioModelConfiguration> = OnceLock::new();

/// Install the process's one model configuration before any scenario
/// inference runs. Each inference reads it, so an AllInOne process can install
/// its Control stores once they are open.
pub fn install_scenario_model_configuration(
    configuration: ScenarioModelConfiguration,
) -> Result<(), std::io::Error> {
    INSTALLED
        .set(configuration)
        .map_err(|_| std::io::Error::other("scenario model configuration was already installed"))
}

/// A scenario model: deterministic until an operator binds it (see the module
/// decision table). Builders obtain it through [`crate::scenario_model`].
pub(crate) struct ConfiguredScenarioModel {
    deterministic: Arc<dyn LlmExecutor>,
    /// `None` reads the process's installed configuration at each inference.
    configuration: Option<&'static ScenarioModelConfiguration>,
}

impl ConfiguredScenarioModel {
    pub(crate) fn new(deterministic: Arc<dyn LlmExecutor>) -> Self {
        Self::with_configuration(deterministic, None)
    }

    fn with_configuration(
        deterministic: Arc<dyn LlmExecutor>,
        configuration: Option<&'static ScenarioModelConfiguration>,
    ) -> Self {
        Self {
            deterministic,
            configuration,
        }
    }

    /// The executor this inference runs on, with the request rebound to a
    /// bound profile's candidate when one applies.
    async fn select(
        &self,
        mut request: ChatRequest,
    ) -> Result<(Arc<dyn LlmExecutor>, ChatRequest), LlmError> {
        let configuration = self.configuration.or_else(|| INSTALLED.get());
        let Some(configuration) = configuration else {
            return Ok((self.deterministic.clone(), request));
        };
        match configuration
            .executor_for(request.model_binding.identity().1)
            .await
        {
            Ok(Some(bound)) => {
                request.model_binding = bound.binding;
                Ok((bound.executor, request))
            }
            Ok(None) => Ok((self.deterministic.clone(), request)),
            Err(reason) => Err(LlmError::Binding(reason)),
        }
    }
}

#[async_trait::async_trait]
impl LlmExecutor for ConfiguredScenarioModel {
    async fn infer(&self, request: ChatRequest) -> Result<ChatResponse, LlmError> {
        let (executor, request) = self.select(request).await?;
        executor.infer(request).await
    }

    async fn infer_streaming(
        &self,
        request: ChatRequest,
        sink: &dyn DeltaSink,
    ) -> Result<ChatResponse, LlmError> {
        let (executor, request) = self.select(request).await?;
        executor.infer_streaming(request, sink).await
    }
}

/// The deterministic side of a scenario whose model must be configured: every
/// inference fails until an operator binds a profile (rule B3 never applies).
pub(crate) struct UnboundModel;

#[async_trait::async_trait]
impl LlmExecutor for UnboundModel {
    async fn infer(&self, _request: ChatRequest) -> Result<ChatResponse, LlmError> {
        Err(LlmError::Binding(
            "this scenario runs only an operator-configured model: connect a provider \
             (POST /v1/config/provider-connections) and bind the `default` inference profile \
             (PUT /v1/config/inference-profiles/default)"
                .into(),
        ))
    }
}

/// The OAuth-kind credential source the `oauth-resolved` scenario reuses.
const OAUTH_FIXTURE_CREDENTIAL: &str = "cred:scenario:oauth-fixture";

/// Hold one OAuth-kind credential whose fixture helper mints
/// `oauth-minted-key`, in the installed configuration's vault. The helper's
/// output is the only key the `oauth-resolved` fake upstream accepts.
pub(crate) async fn seed_oauth_fixture_credential() -> Result<(), String> {
    use awaken_credential_vault::{CredentialKind, CredentialSource, CredentialStatus};
    const OAUTH_MINTED_KEY: &str = "oauth-minted-key"; // awaken-allow(secret):
    let configuration = INSTALLED
        .get()
        .ok_or("the scenario model configuration is not installed")?;
    let source = CredentialSource {
        id: awaken_credential_contract::CredentialSourceId(OAUTH_FIXTURE_CREDENTIAL.into()),
        replacement_of: None,
        workspace_id: configuration.workspace.clone(),
        kind: CredentialKind::Oauth,
        descriptor: None,
        provider_id: Some("anthropic".into()),
        protocol_endpoint_id: None,
        env_key: None,
        material_ref: None,
        auxiliary_material_refs: Default::default(),
        // OAuth helper portability decision table:
        // | Windows | helper                                      | token bytes |
        // | true    | cmd.exe /D /C echo|set /p=<token>           | exact       |
        // | false   | printf <token>                              | exact       |
        // Both rules avoid a trailing newline after materialization trims stdout.
        oauth_command: Some(if cfg!(windows) {
            vec![
                "cmd.exe".into(),
                "/D".into(),
                "/C".into(),
                format!("echo|set /p={OAUTH_MINTED_KEY} & exit /b 0"),
            ]
        } else {
            vec!["printf".into(), OAUTH_MINTED_KEY.into()]
        }),
        worker_local_binding: None,
        status: CredentialStatus::Active,
        version: 1,
    };
    configuration
        .credentials
        .put(source)
        .await
        .map_err(|error| format!("persist the OAuth fixture credential: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::RedactedString;
    use awaken_config_resolver::{InferenceProfile, ModelTarget, ProfileCandidate};
    use awaken_credential_vault::CredentialBinding;
    use awaken_model_catalog::repo::CatalogRepo;
    use awaken_model_catalog::{
        ApiDialect, Offering, ProtocolEndpoint, ProtocolEndpointId, Provider, ProviderId,
    };

    const WORKSPACE: &str = "scenario-workspace";

    /// A configuration whose catalog offers `models` on one Anthropic endpoint
    /// and whose vault holds one key for it.
    async fn configured(models: &[&str]) -> (ScenarioModelConfiguration, String) {
        let clock = awaken_time::system_clock();
        let catalog = Arc::new(awaken_model_catalog::repo::InMemoryCatalogRepo::new());
        catalog
            .put_provider(Provider {
                id: ProviderId::new("anthropic"),
                slug: "anthropic".into(),
                display_name: "Anthropic".into(),
                version: 1,
            })
            .await
            .unwrap();
        catalog
            .put_endpoint(ProtocolEndpoint {
                workspace_id: None,
                id: ProtocolEndpointId::new("ep1"),
                provider_id: ProviderId::new("anthropic"),
                dialect: ApiDialect::AnthropicMessages,
                base_url: Some("http://127.0.0.1:9/v1/".into()),
                timeout_secs: 30,
                display_name: "fixture".into(),
                version: 1,
            })
            .await
            .unwrap();
        for model in models {
            catalog
                .put_offering(Offering {
                    model_id: (*model).into(),
                    provider_id: ProviderId::new("anthropic"),
                    protocol_endpoint_id: ProtocolEndpointId::new("ep1"),
                    dialect: ApiDialect::AnthropicMessages,
                    upstream_model: None,
                    source: Default::default(),
                    status: Default::default(),
                    last_seen_at_unix_ms: None,
                })
                .await
                .unwrap();
        }
        let credentials = Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new());
        let secrets = Arc::new(awaken_credential_vault::InMemorySecretStore::new());
        let credential = awaken_credential_vault::repo::enter_credential(
            awaken_credential_vault::CredentialCreateParams {
                workspace_id: WORKSPACE.into(),
                kind: awaken_credential_vault::CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new("fixture-key")), // awaken-allow(secret):
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            clock.as_ref(),
        )
        .await
        .unwrap();
        let configuration = ScenarioModelConfiguration::over(
            catalog,
            credentials,
            secrets,
            Arc::new(awaken_config_resolver::InMemoryProfileStore::new()),
            WORKSPACE.into(),
            clock,
        );
        (configuration, credential.id.0)
    }

    async fn bind(
        configuration: &ScenarioModelConfiguration,
        profile: &str,
        model: &str,
        credential: &str,
    ) {
        awaken_config_resolver::put_workspace_profile(
            configuration.profiles.as_ref(),
            WORKSPACE,
            profile,
            InferenceProfile {
                workspace_id: WORKSPACE.into(),
                primary: ProfileCandidate {
                    target: ModelTarget {
                        provider_id: Some("anthropic".into()),
                        protocol_endpoint_id: Some("ep1".into()),
                        ..ModelTarget::unqualified(model)
                    },
                    credential_binding: CredentialBinding::Exact {
                        credential_source_id: awaken_credential_contract::CredentialSourceId(
                            credential.into(),
                        ),
                    },
                },
                fallbacks: Vec::new(),
                disabled_endpoint_ids: Vec::new(),
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn binding_decision_table() {
        // B3: nothing bound -> the deterministic model.
        let (configuration, credential) = configured(&["upstream-a", "upstream-b"]).await;
        assert_eq!(configuration.bound_profile("label").await.unwrap(), None);
        assert!(configuration.executor_for("label").await.unwrap().is_none());

        // B2: only `default` bound -> every reference resolves through it, and
        // the vault realizes its credential.
        bind(
            &configuration,
            DEFAULT_SCENARIO_PROFILE,
            "upstream-a",
            &credential,
        )
        .await;
        assert_eq!(
            configuration
                .bound_profile("label")
                .await
                .unwrap()
                .as_deref(),
            Some(DEFAULT_SCENARIO_PROFILE)
        );
        let bound = configuration.executor_for("label").await.unwrap().unwrap();
        assert_eq!(
            bound.binding.identity().1,
            "upstream-a",
            "B2 runs the bound model"
        );

        // B1: a profile named by the reference wins over `default`.
        bind(&configuration, "label", "upstream-b", &credential).await;
        assert_eq!(
            configuration
                .bound_profile("label")
                .await
                .unwrap()
                .as_deref(),
            Some("label")
        );
        assert_eq!(
            configuration
                .bound_profile("other")
                .await
                .unwrap()
                .as_deref(),
            Some(DEFAULT_SCENARIO_PROFILE)
        );
        let bound = configuration.executor_for("label").await.unwrap().unwrap();
        assert_eq!(
            bound.binding.identity().1,
            "upstream-b",
            "B1 runs its own model"
        );

        // B4: a bound profile that cannot resolve fails; it never falls back.
        bind(&configuration, "broken", "not-offered", &credential).await;
        assert!(configuration.executor_for("broken").await.is_err());
    }

    #[tokio::test]
    async fn an_unconfigured_process_runs_the_deterministic_model() {
        // B3 at the executor boundary: with no configuration the deterministic
        // executor answers, byte for byte.
        let model = ConfiguredScenarioModel::with_configuration(Arc::new(UnboundModel), None);
        let request = ChatRequest {
            model_binding: awaken_runtime_contract::resolved::ModelBinding::parse(
                "scenario", "real", "genai",
            )
            .unwrap(),
            inference: Default::default(),
            messages: Vec::new(),
            tools: Vec::new(),
        };
        let error = model.infer(request).await.unwrap_err();
        assert!(
            error.to_string().contains("inference-profiles/default"),
            "{error}"
        );
    }
}
