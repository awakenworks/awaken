// @formal surface obligation=credential.http_effect_usage_binds_exact_fields_and_placements
// @formal reason: Projects a catalog row and the host-owned secret reference onto the one ACP process launch.
//! Projection of one ACP catalog row onto a concrete process launch.
//!
//! The catalog (`awaken-acp-catalog`) owns the rows; this module owns how a
//! resolved model, compaction window and per-agent environment become an
//! [`AcpLaunch`]. Precedence and the fail-closed rules live here once.

use crate::{AcpLaunch, AcpLaunchIdentity, OpenError};
use awaken_acp_catalog::{
    AcpCli, BackendModelInterface, ManagedModelInterface, ManagedProviderConfigCodec,
};
use awaken_provisioning_contract as pc;
use awaken_runtime_contract::resolved::BackendModelSelection;

mod managed_delivery;
use managed_delivery::project_acp_session;
pub use managed_delivery::{ProcessSecretRequirement, ResolvedModel};

impl From<awaken_acp_catalog::ProviderServerToolRefusal> for OpenError {
    fn from(refusal: awaken_acp_catalog::ProviderServerToolRefusal) -> Self {
        Self(refusal.0)
    }
}

impl AcpLaunch {
    /// Project the resolved model + optional compaction window + per-agent env
    /// overrides onto a concrete [`AcpLaunch`]. Precedence (low→high):
    /// static defaults < per-agent passthrough < typed model delivery < the secret
    /// API key (host-controlled, never overridable by passthrough). So a modeled
    /// fact (model/base_url) always wins over a stray passthrough key, and the key
    /// is always the host's.
    pub fn from_catalog(
        cli: &AcpCli,
        model: &ResolvedModel,
        context_window: Option<u64>,
        extra_env: &[(String, String)],
    ) -> Result<Self, OpenError> {
        Self::from_catalog_with_argv(cli, model, context_window, extra_env, None)
    }

    /// Project with an acquisition-resolved base argv. Product startup uses
    /// this for an absolute, already-installed wrapper path; callers without an
    /// override use the profile's preinstalled binary name.
    pub fn from_catalog_with_argv(
        cli: &AcpCli,
        model: &ResolvedModel,
        context_window: Option<u64>,
        extra_env: &[(String, String)],
        resolved_argv: Option<&[String]>,
    ) -> Result<Self, OpenError> {
        use std::collections::BTreeMap;
        let mut env: BTreeMap<String, pc::EnvVar> = BTreeMap::new();
        let inline = |name: &str, value: String| pc::EnvVar {
            name: name.to_string(),
            value: pc::EnvValue::Inline { value },
            visibility: pc::EnvVisibility::Process,
        };
        for (k, v) in cli.env {
            env.insert((*k).to_string(), inline(k, (*v).to_string()));
        }
        for (k, v) in extra_env {
            env.insert(k.clone(), inline(k, v.clone()));
        }
        let mut argv = resolved_argv
            .map(<[String]>::to_vec)
            .unwrap_or_else(|| cli.acquisition.local_argv());
        if argv.is_empty() || argv[0].trim().is_empty() {
            return Err(OpenError(
                "resolved ACP launch argv must not be empty".into(),
            ));
        }
        let mut session_config_options = BTreeMap::new();
        let mut session_mode = None;
        let mut session_working_directory = None;
        if let ResolvedModel::BackendOwned {
            model_selection,
            model,
            capability,
            session_configuration,
        } = model
        {
            session_configuration
                .validate_working_directory()
                .map_err(|reason| OpenError(format!("invalid ACP working directory: {reason}")))?;
            session_mode.clone_from(&session_configuration.mode);
            session_working_directory.clone_from(&session_configuration.working_directory);
            for (config_id, value) in &session_configuration.options {
                session_config_options.insert(config_id.clone(), value.clone());
            }
            // A retained managed projection cannot shadow the CLI-owned account,
            // endpoint, model, or home. Production supplies no extra env for this
            // mode; stripping catalog-known keys makes the boundary fail safe for
            // alternate resolvers and stale snapshots too.
            if let Some(delivery) = cli.model_delivery {
                for key in std::iter::once(delivery.base_url)
                    .chain(std::iter::once(delivery.model))
                    .chain(delivery.credential_env.iter().copied())
                    .chain(delivery.aliases.iter().copied())
                {
                    env.remove(key);
                }
            }
            if let Some(delivery) = cli.managed_provider_config {
                env.remove(delivery.config_env);
                if !delivery.provider_env.is_empty() {
                    env.remove(delivery.provider_env);
                }
            }
            for key in cli
                .config_home_env
                .into_iter()
                .chain(cli.config_home_aliases.iter().copied())
                .chain(cli.context_window_env)
            {
                env.remove(key);
            }
            match model_selection {
                BackendModelSelection::Default if !model.is_empty() => {
                    return Err(OpenError(
                        "backend_default_model_must_not_carry_an_exact_id".into(),
                    ));
                }
                BackendModelSelection::Default => {}
                BackendModelSelection::Exact if model.trim().is_empty() => {
                    return Err(OpenError("backend_exact_model_id_required".into()));
                }
                BackendModelSelection::Exact => match cli.backend_model_interface {
                    BackendModelInterface::ConfigOverride { flag, key } => {
                        argv.push(flag.to_string());
                        argv.push(format!("{key}={model:?}"));
                    }
                    BackendModelInterface::Flag { flag } => {
                        argv.push(flag.to_string());
                        argv.push(model.clone());
                    }
                    BackendModelInterface::SessionConfigOption { config_id } => {
                        match session_config_options.get(config_id) {
                            Some(value) if value != model => {
                                return Err(OpenError(format!(
                                    "backend model conflicts with ACP option `{config_id}`"
                                )));
                            }
                            Some(_) => {}
                            None => {
                                session_config_options.insert(config_id.to_string(), model.clone());
                            }
                        }
                    }
                    BackendModelInterface::Unsupported => {
                        return Err(OpenError(format!(
                            "backend_exact_model_unsupported: {}",
                            cli.id
                        )));
                    }
                },
            }
            return Ok(AcpLaunch {
                tool_narrowing_check: AcpLaunch::opaque_tool_narrowing,
                argv,
                env: env.into_values().collect(),
                identity: AcpLaunchIdentity::BackendOwned,
                session_model: None,
                session_mode,
                tool_aliases: Default::default(),
                session_config_options: session_config_options
                    .into_iter()
                    .map(
                        |(config_id, value)| awaken_protocol_acp::SessionConfigOptionSelection {
                            config_id,
                            value,
                        },
                    )
                    .collect(),
                session_working_directory,
                expected_capability: Some(capability.clone()),
                instructions: crate::AcpInstructions::default(),
            });
        }

        let ResolvedModel::Managed {
            base_url,
            model,
            process_secret,
            credential_artifact,
            acp,
            provider_server_tools,
        } = model
        else {
            unreachable!("backend-owned launch returned above")
        };
        let d = cli.model_delivery.as_ref();
        if let Some(unsupported) = provider_server_tools
            .iter()
            .find(|tool| !cli.realizes_provider_server_tool(tool))
        {
            return Err(OpenError(format!(
                "provider_server_tool_unsupported: {} cannot realize {}",
                cli.id,
                unsupported.provider_kind()
            )));
        }
        let native_web_search = provider_server_tools.iter().any(|tool| {
            matches!(
                tool,
                awaken_runtime_contract::resolved::ProviderServerTool::OpenAiWebSearch
                    | awaken_runtime_contract::resolved::ProviderServerTool::DeepSeekResponsesWebSearch
            )
        });
        let (session_mode, session_config_options, expected_capability) =
            project_acp_session(cli.id, acp.as_ref());
        if let Some(profile) = acp.as_ref() {
            profile
                .session_configuration
                .validate_working_directory()
                .map_err(|reason| OpenError(format!("invalid ACP working directory: {reason}")))?;
        }
        let session_working_directory = acp
            .as_ref()
            .and_then(|profile| profile.session_configuration.working_directory.clone());
        let session_model = match cli.managed_model_interface {
            ManagedModelInterface::Environment => None,
            ManagedModelInterface::SessionModel if model.trim().is_empty() => {
                return Err(OpenError("managed_session_model_id_required".into()));
            }
            ManagedModelInterface::SessionModel => Some(model.clone()),
        };
        if process_secret.is_some() && !cli.managed_credential_delivery.allows_process_secret() {
            return Err(OpenError(format!(
                "credential_delivery_mismatch: {} requires a managed artifact",
                cli.id
            )));
        }
        if credential_artifact.is_some() && cli.managed_credential_delivery.allows_process_secret()
        {
            return Err(OpenError(format!(
                "credential_delivery_mismatch: {} requires a process secret",
                cli.id
            )));
        }
        if d.is_none() && credential_artifact.is_none() {
            return Err(OpenError(format!("credential_driver_required: {}", cli.id)));
        }
        let Some(d) = d else {
            return Ok(AcpLaunch {
                tool_narrowing_check: AcpLaunch::opaque_tool_narrowing,
                argv,
                env: env.into_values().collect(),
                identity: AcpLaunchIdentity::Managed,
                session_model,
                session_mode,
                tool_aliases: Default::default(),
                session_config_options,
                session_working_directory,
                expected_capability,
                instructions: crate::AcpInstructions::default(),
            });
        };
        if !base_url.is_empty() {
            env.insert(d.base_url.to_string(), inline(d.base_url, base_url.clone()));
        }
        if !model.is_empty() {
            env.insert(d.model.to_string(), inline(d.model, model.clone()));
            for alias in d.aliases {
                env.insert((*alias).to_string(), inline(alias, model.clone()));
            }
        }
        if let Some(config) = cli.managed_provider_config {
            let document = match config.codec {
                ManagedProviderConfigCodec::Codex => {
                    let mut providers = serde_json::Map::new();
                    providers.insert(
                        config.provider_id.to_string(),
                        serde_json::json!({
                            "name": "Awaken managed provider",
                            "base_url": base_url,
                            "wire_api": config.wire_api,
                            "requires_openai_auth": config.requires_openai_auth,
                            "supports_standalone_web_search": native_web_search,
                        }),
                    );
                    let mut document = serde_json::json!({
                        "model": model,
                        "model_provider": config.provider_id,
                        "model_providers": providers,
                    });
                    if native_web_search {
                        document["web_search"] = serde_json::json!("live");
                    }
                    document
                }
                ManagedProviderConfigCodec::OpenCode {
                    provider_package,
                    credential_env,
                } => {
                    let mut models = serde_json::Map::new();
                    models.insert(model.clone(), serde_json::json!({"name": model}));
                    let mut providers = serde_json::Map::new();
                    providers.insert(
                        config.provider_id.to_string(),
                        serde_json::json!({
                            "npm": provider_package,
                            "name": "Awaken managed provider",
                            "options": {
                                "baseURL": base_url,
                                "apiKey": format!("{{env:{credential_env}}}"),
                            },
                            "models": models,
                        }),
                    );
                    serde_json::json!({
                        "model": format!("{}/{}", config.provider_id, model),
                        "provider": providers,
                    })
                }
            };
            let serialized = serde_json::to_string(&document)
                .map_err(|error| OpenError(format!("managed_provider_config_invalid: {error}")))?;
            env.insert(
                config.config_env.to_string(),
                inline(config.config_env, serialized),
            );
            if !config.provider_env.is_empty() {
                env.insert(
                    config.provider_env.to_string(),
                    inline(config.provider_env, config.provider_id.to_string()),
                );
            }
        }
        if let (Some(key), Some(window)) = (cli.context_window_env, context_window) {
            env.insert(key.to_string(), inline(key, window.to_string()));
        }
        // The typed secret goes last so no passthrough key can shadow it.
        if let Some(secret) = process_secret {
            let key = match secret.environment_variable() {
                Some(key) if d.supports_credential_env(key) => key,
                Some(key) => {
                    return Err(OpenError(format!(
                        "credential_environment_unsupported: {} does not accept {key}",
                        cli.id
                    )));
                }
                None => d.default_credential_env().ok_or_else(|| {
                    OpenError(format!(
                        "credential_environment_missing: {} has no process-secret environment",
                        cli.id
                    ))
                })?,
            };
            env.insert(
                key.to_string(),
                pc::EnvVar {
                    name: key.to_string(),
                    value: pc::EnvValue::Secret {
                        reference: secret.reference().to_string(),
                    },
                    visibility: pc::EnvVisibility::Process,
                },
            );
        }

        Ok(AcpLaunch {
            tool_narrowing_check: AcpLaunch::opaque_tool_narrowing,
            argv,
            env: env.into_values().collect(),
            identity: AcpLaunchIdentity::Managed,
            session_model,
            session_mode,
            tool_aliases: Default::default(),
            session_config_options,
            session_working_directory,
            expected_capability,
            instructions: crate::AcpInstructions::default(),
        })
    }
}

#[cfg(test)]
#[path = "acp_cli/tests.rs"]
mod tests;
