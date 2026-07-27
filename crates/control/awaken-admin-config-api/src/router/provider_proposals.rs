//! Secret-free provider authoring hints derived from the process environment.

/// A read-only, non-executable hint derived from process environment. It is not a
/// catalog row, credential source, profile or publication and carries no secret.
/// The UI may use it to prefill existing authoring forms; only their explicit writes
/// create execution truth.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct EnvironmentProviderProposal {
    pub provider_id: String,
    pub endpoint_id: String,
    pub dialect: awaken_model_catalog::ApiDialect,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
    pub credential_env: String,
    pub credential_present: bool,
}

struct ProposalKeys {
    provider_id: &'static str,
    endpoint_id: &'static str,
    dialect: awaken_model_catalog::ApiDialect,
    base_url: &'static str,
    model: &'static str,
    credential: &'static str,
}

pub(super) fn provider_proposals_from(
    read: impl Fn(&str) -> Option<String>,
) -> Vec<EnvironmentProviderProposal> {
    use awaken_model_catalog::ApiDialect;

    let known = [
        ProposalKeys {
            provider_id: "anthropic",
            endpoint_id: "anthropic-messages",
            dialect: ApiDialect::AnthropicMessages,
            base_url: "ANTHROPIC_BASE_URL",
            model: "ANTHROPIC_MODEL",
            credential: "ANTHROPIC_API_KEY",
        },
        ProposalKeys {
            provider_id: "openai",
            endpoint_id: "openai-chat",
            dialect: ApiDialect::OpenAiChat,
            base_url: "OPENAI_BASE_URL",
            model: "OPENAI_MODEL",
            credential: "OPENAI_API_KEY",
        },
        ProposalKeys {
            provider_id: "gemini",
            endpoint_id: "gemini",
            dialect: ApiDialect::Gemini,
            base_url: "GEMINI_BASE_URL",
            model: "GEMINI_MODEL",
            credential: "GEMINI_API_KEY",
        },
        ProposalKeys {
            provider_id: "kimi",
            endpoint_id: "kimi-anthropic",
            dialect: ApiDialect::AnthropicMessages,
            base_url: "KIMI_BASE_URL",
            model: "KIMI_MODEL",
            credential: "KIMI_API_KEY",
        },
        ProposalKeys {
            provider_id: "minimax",
            endpoint_id: "minimax-anthropic",
            dialect: ApiDialect::AnthropicMessages,
            base_url: "MINIMAX_BASE_URL",
            model: "MINIMAX_MODEL",
            credential: "MINIMAX_API_KEY",
        },
    ];

    known
        .into_iter()
        .filter_map(|keys| {
            let base_url = read(keys.base_url).filter(|value| !value.trim().is_empty());
            let model_id = read(keys.model).filter(|value| !value.trim().is_empty());
            let credential_present =
                read(keys.credential).is_some_and(|value| !value.trim().is_empty());
            (base_url.is_some() || model_id.is_some() || credential_present).then(|| {
                EnvironmentProviderProposal {
                    provider_id: keys.provider_id.to_string(),
                    endpoint_id: keys.endpoint_id.to_string(),
                    dialect: keys.dialect,
                    base_url,
                    model_id,
                    credential_env: keys.credential.to_string(),
                    credential_present,
                }
            })
        })
        .collect()
}
