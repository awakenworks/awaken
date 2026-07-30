//! Managed Agents model-id ACL.
//!
//! The public field stays a string exactly as the Managed Agents API expects.
//! This codec is the only place where Awaken's provider / endpoint / ACP
//! qualifiers become config-domain model-selection intent.

use awaken_config_store::{ModelSelection, ModelTarget};
use awaken_runtime_contract::resolved::{AcpSessionConfiguration, Backend, ModelBinding};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedModelIdError {
    #[error("model id must not be empty")]
    Empty,
    #[error("invalid Managed model id `{0}`")]
    Invalid(String),
    #[error("model selection cannot be represented by a Managed model id")]
    UnsupportedSelection,
}

/// Parse a Managed-compatible model string into authoring intent.
pub fn parse_managed_model_id(value: &str) -> Result<ModelSelection, ManagedModelIdError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ManagedModelIdError::Empty);
    }
    if value.chars().any(char::is_whitespace) {
        return Err(ManagedModelIdError::Invalid(value.into()));
    }

    if let Some(endpoint) = value.strip_prefix("a2a:") {
        if endpoint.is_empty() {
            return Err(ManagedModelIdError::Invalid(value.into()));
        }
        return Ok(ModelSelection::Pinned(ModelBinding::new("", "", value)));
    }

    if let Some(acp) = value.strip_prefix("acp:") {
        return parse_acp(acp, value);
    }

    let (route, model_id) = match value.split_once('/') {
        Some((route, model_id)) => (Some(route), model_id),
        None => (None, value),
    };
    if model_id.is_empty() {
        return Err(ManagedModelIdError::Invalid(value.into()));
    }
    let (provider_id, endpoint_name) = route.map(parse_provider_route).transpose()?.unzip();
    Ok(ModelSelection::Target {
        target: ModelTarget {
            model_id: model_id.into(),
            provider_id,
            protocol_endpoint_id: None,
            endpoint_name: endpoint_name.flatten(),
        },
        backend_ref: "genai".into(),
        configuration: AcpSessionConfiguration::default(),
    })
}

fn parse_acp(acp: &str, original: &str) -> Result<ModelSelection, ManagedModelIdError> {
    let (executor_route, model_id) = match acp.split_once('/') {
        Some((route, model)) if !model.is_empty() => (route, Some(model)),
        Some(_) => return Err(ManagedModelIdError::Invalid(original.into())),
        None => (acp, None),
    };
    let mut coordinates = executor_route.split('@');
    let cli = coordinates.next().unwrap_or_default();
    if cli.is_empty() {
        return Err(ManagedModelIdError::Invalid(original.into()));
    }
    let provider_id = coordinates.next();
    let endpoint_name = coordinates.next();
    if coordinates.next().is_some()
        || provider_id.is_some_and(str::is_empty)
        || endpoint_name.is_some_and(str::is_empty)
    {
        return Err(ManagedModelIdError::Invalid(original.into()));
    }
    let backend_ref = format!("acp:{cli}");
    match (provider_id, model_id) {
        (None, None) => Ok(ModelSelection::BackendDefault {
            backend_ref,
            configuration: AcpSessionConfiguration::default(),
        }),
        (None, Some(model_ref)) => Ok(ModelSelection::BackendExact {
            backend_ref,
            model_ref: model_ref.into(),
            configuration: AcpSessionConfiguration::default(),
        }),
        (Some(provider_id), Some(model_id)) => Ok(ModelSelection::Target {
            target: ModelTarget {
                model_id: model_id.into(),
                provider_id: Some(provider_id.into()),
                protocol_endpoint_id: None,
                endpoint_name: endpoint_name.map(str::to_string),
            },
            backend_ref,
            configuration: AcpSessionConfiguration::default(),
        }),
        (Some(_), None) => Err(ManagedModelIdError::Invalid(original.into())),
    }
}

fn parse_provider_route(route: &str) -> Result<(String, Option<String>), ManagedModelIdError> {
    let mut coordinates = route.split('@');
    let provider = coordinates.next().unwrap_or_default();
    let endpoint = coordinates.next();
    if provider.is_empty() || endpoint.is_some_and(str::is_empty) || coordinates.next().is_some() {
        return Err(ManagedModelIdError::Invalid(route.into()));
    }
    Ok((provider.into(), endpoint.map(str::to_string)))
}

/// Render authored or resolved selection back to the one Managed string field.
pub fn render_managed_model_id(selection: &ModelSelection) -> Result<String, ManagedModelIdError> {
    match selection {
        ModelSelection::Target {
            target,
            backend_ref,
            ..
        } => render_target(target, backend_ref),
        ModelSelection::BackendDefault { backend_ref, .. } => render_acp_backend(backend_ref, None),
        ModelSelection::BackendExact {
            backend_ref,
            model_ref,
            ..
        } => render_acp_backend(backend_ref, Some(model_ref)),
        ModelSelection::Pinned(binding) => render_binding(binding),
        ModelSelection::Auto | ModelSelection::Profile { .. } => {
            Err(ManagedModelIdError::UnsupportedSelection)
        }
    }
}

fn render_target(target: &ModelTarget, backend_ref: &str) -> Result<String, ManagedModelIdError> {
    if target.model_id.is_empty() {
        return Err(ManagedModelIdError::UnsupportedSelection);
    }
    let endpoint = target
        .endpoint_name
        .as_deref()
        .or(target.protocol_endpoint_id.as_deref());
    let provider_route = match (&target.provider_id, endpoint) {
        (None, None) => None,
        (Some(provider), None) => Some(provider.clone()),
        (Some(provider), Some(endpoint)) => Some(format!("{provider}@{endpoint}")),
        (None, Some(_)) => return Err(ManagedModelIdError::UnsupportedSelection),
    };
    match Backend::from_ref(backend_ref) {
        Backend::Native => Ok(provider_route.map_or_else(
            || target.model_id.clone(),
            |route| format!("{route}/{}", target.model_id),
        )),
        Backend::Acp { cli } if !cli.is_empty() => match provider_route {
            Some(route) => Ok(format!("acp:{cli}@{route}/{}", target.model_id)),
            None => Ok(format!("acp:{cli}/{}", target.model_id)),
        },
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

fn render_acp_backend(
    backend_ref: &str,
    model_ref: Option<&String>,
) -> Result<String, ManagedModelIdError> {
    let Backend::Acp { cli } = Backend::from_ref(backend_ref) else {
        return Err(ManagedModelIdError::UnsupportedSelection);
    };
    if cli.is_empty() {
        return Err(ManagedModelIdError::UnsupportedSelection);
    }
    Ok(model_ref.map_or_else(
        || format!("acp:{cli}"),
        |model| format!("acp:{cli}/{model}"),
    ))
}

fn render_binding(binding: &ModelBinding) -> Result<String, ManagedModelIdError> {
    match Backend::from_ref(&binding.backend_ref) {
        Backend::Native if binding.provider_identity_ref.is_empty() => {
            Ok(binding.model_ref.clone())
        }
        Backend::Native => Ok(format!(
            "{}/{}",
            binding.provider_identity_ref, binding.model_ref
        )),
        Backend::Acp { cli } if !cli.is_empty() && binding.provider_identity_ref.is_empty() => {
            Ok(format!("acp:{cli}/{}", binding.model_ref))
        }
        Backend::Acp { cli } if !cli.is_empty() => Ok(format!(
            "acp:{cli}@{}/{}",
            binding.provider_identity_ref, binding.model_ref
        )),
        Backend::Remote { endpoint }
            if !endpoint.is_empty()
                && binding.provider_identity_ref.is_empty()
                && binding.model_ref.is_empty() =>
        {
            Ok(format!("a2a:{endpoint}"))
        }
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_complete_managed_model_id_decision_table() {
        // Causes: C1 native/ACP executor; C2 provider absent/present; C3 endpoint
        // absent/present; C4 model absent/present; C5 remote endpoint
        // absent/present. Effects: E1 catalog Target; E2 backend-owned default;
        // E3 backend-owned exact; E4 remote pinned selection; E5 reject.
        // Decision rules: native+model -> E1; ACP+C2=N+C4=N -> E2;
        // ACP+C2=N+C4=Y -> E3; ACP+C2=Y+C4=Y -> E1; endpoint without
        // provider or provider without model -> E5; A2A+C5=Y -> E4;
        // A2A+C5=N -> E5.
        let cases = [
            ("gpt-5", "gpt-5"),
            ("openai/gpt-5", "openai/gpt-5"),
            ("openai@edge/gpt-5", "openai@edge/gpt-5"),
            ("acp:codex", "acp:codex"),
            ("acp:codex/gpt-5", "acp:codex/gpt-5"),
            ("acp:codex@openai/gpt-5", "acp:codex@openai/gpt-5"),
            ("acp:codex@openai@edge/gpt-5", "acp:codex@openai@edge/gpt-5"),
            (
                "a2a:https://agent.example/a2a",
                "a2a:https://agent.example/a2a",
            ),
            ("anyrouter/qwen/qwen3-235b", "anyrouter/qwen/qwen3-235b"),
        ];
        for (wire, canonical) in cases {
            let selection = parse_managed_model_id(wire).expect(wire);
            assert_eq!(render_managed_model_id(&selection).unwrap(), canonical);
            if wire.starts_with("a2a:") {
                assert!(selection.resolved().is_some(), "{wire} is an exact remote");
            } else {
                assert!(selection.resolved().is_none(), "{wire} must remain intent");
            }
        }
        for invalid in ["", "acp:", "a2a:", "acp:codex@openai", "openai@/gpt-5"] {
            assert!(parse_managed_model_id(invalid).is_err(), "{invalid}");
        }
    }
}
