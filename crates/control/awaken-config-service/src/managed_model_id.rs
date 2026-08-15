//! Managed Agents model-id ACL.
//!
//! The public field stays an opaque string exactly as the Managed Agents API
//! expects. A model id is the readable head; optional named qualifiers select
//! provider, API dialect, endpoint, and executor without positional coupling:
//!
//! `qwen/qwen3-235b;provider=anyrouter;api=open_ai_responses;endpoint=primary;executor=acp:codex`
//!
//! This codec is the only place where those public selectors become config-
//! domain model-selection intent. Runtime consumes only the resolved binding.

use std::collections::BTreeMap;

use awaken_agent_config::{ModelSelection, ModelTarget};
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

    if let Some(profile) = value.strip_prefix("profile=") {
        let profile_id = decode_component(profile, value)?;
        if profile_id.is_empty() || profile_id.contains(';') {
            return Err(ManagedModelIdError::Invalid(value.into()));
        }
        return Ok(ModelSelection::Profile { profile_id });
    }

    if let Some(executor) = value.strip_prefix("executor=") {
        if executor.contains(';') {
            return Err(ManagedModelIdError::Invalid(value.into()));
        }
        return executor_only(&decode_component(executor, value)?, value);
    }

    let mut parts = value.split(';');
    let model_id = decode_component(parts.next().unwrap_or_default(), value)?;
    if model_id.is_empty() {
        return Err(ManagedModelIdError::Invalid(value.into()));
    }
    let mut qualifiers = BTreeMap::new();
    for part in parts {
        let Some((key, encoded)) = part.split_once('=') else {
            return Err(ManagedModelIdError::Invalid(value.into()));
        };
        if !matches!(key, "provider" | "api" | "endpoint" | "executor")
            || encoded.is_empty()
            || qualifiers
                .insert(key, decode_component(encoded, value)?)
                .is_some()
        {
            return Err(ManagedModelIdError::Invalid(value.into()));
        }
    }

    let provider_id = qualifiers.remove("provider");
    let api_dialect = qualifiers.remove("api");
    let endpoint_name = qualifiers.remove("endpoint");
    let executor = qualifiers
        .remove("executor")
        .unwrap_or_else(|| "native".into());
    if !qualifiers.is_empty()
        || api_dialect.is_some() && provider_id.is_none()
        || endpoint_name.is_some() && (provider_id.is_none() || api_dialect.is_none())
    {
        return Err(ManagedModelIdError::Invalid(value.into()));
    }

    let backend_ref = parse_executor(&executor, value)?;
    match Backend::from_ref(&backend_ref) {
        Backend::Native | Backend::Acp { .. } => Ok(ModelSelection::Target {
            target: ModelTarget {
                model_id,
                provider_id,
                api_dialect,
                protocol_endpoint_id: None,
                endpoint_name,
            },
            backend_ref,
            configuration: AcpSessionConfiguration::default(),
        }),
        Backend::Remote { .. } => Err(ManagedModelIdError::Invalid(value.into())),
    }
}

fn executor_only(executor: &str, original: &str) -> Result<ModelSelection, ManagedModelIdError> {
    let backend_ref = parse_executor(executor, original)?;
    match Backend::from_ref(&backend_ref) {
        Backend::Acp { cli } if !cli.is_empty() => Ok(ModelSelection::BackendDefault {
            backend_ref,
            configuration: AcpSessionConfiguration::default(),
        }),
        Backend::Remote { endpoint } if !endpoint.is_empty() => Ok(ModelSelection::Pinned(
            ModelBinding::new("", "", backend_ref),
        )),
        Backend::Native | Backend::Acp { .. } | Backend::Remote { .. } => {
            Err(ManagedModelIdError::Invalid(original.into()))
        }
    }
}

fn parse_executor(value: &str, original: &str) -> Result<String, ManagedModelIdError> {
    if value == "native" {
        return Ok("genai".into());
    }
    if value
        .strip_prefix("acp:")
        .is_some_and(|adapter| !adapter.is_empty())
        || value
            .strip_prefix("a2a:")
            .is_some_and(|endpoint| !endpoint.is_empty())
    {
        return Ok(value.into());
    }
    Err(ManagedModelIdError::Invalid(original.into()))
}

/// Render authored or resolved selection back to the one canonical Managed
/// string field. Qualifiers always use model, provider, API, endpoint, executor
/// order; the default native executor is omitted.
pub fn render_managed_model_id(selection: &ModelSelection) -> Result<String, ManagedModelIdError> {
    match selection {
        ModelSelection::Target {
            target,
            backend_ref,
            ..
        } => render_target(target, backend_ref),
        ModelSelection::BackendDefault { backend_ref, .. } => render_executor_only(backend_ref),
        ModelSelection::BackendExact {
            backend_ref,
            model_ref,
            ..
        } => render_model_and_executor(model_ref, backend_ref),
        ModelSelection::Pinned(binding) => render_binding(binding),
        ModelSelection::Profile { profile_id } if !profile_id.is_empty() => {
            Ok(format!("profile={}", encode_component(profile_id)))
        }
        ModelSelection::Auto | ModelSelection::Profile { .. } => {
            Err(ManagedModelIdError::UnsupportedSelection)
        }
    }
}

fn render_target(target: &ModelTarget, backend_ref: &str) -> Result<String, ManagedModelIdError> {
    if target.model_id.is_empty()
        || target.api_dialect.is_some() && target.provider_id.is_none()
        || target.endpoint_name.is_some()
            && (target.provider_id.is_none() || target.api_dialect.is_none())
        || target.protocol_endpoint_id.is_some()
    {
        return Err(ManagedModelIdError::UnsupportedSelection);
    }
    let mut rendered = encode_component(&target.model_id);
    if let Some(provider) = &target.provider_id {
        push_qualifier(&mut rendered, "provider", provider);
    }
    if let Some(api) = &target.api_dialect {
        push_qualifier(&mut rendered, "api", api);
    }
    if let Some(endpoint) = &target.endpoint_name {
        push_qualifier(&mut rendered, "endpoint", endpoint);
    }
    match Backend::from_ref(backend_ref) {
        Backend::Native => Ok(rendered),
        Backend::Acp { cli } if !cli.is_empty() => {
            push_qualifier(&mut rendered, "executor", &format!("acp:{cli}"));
            Ok(rendered)
        }
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

fn render_executor_only(backend_ref: &str) -> Result<String, ManagedModelIdError> {
    match Backend::from_ref(backend_ref) {
        Backend::Acp { cli } if !cli.is_empty() => Ok(format!("executor=acp:{cli}")),
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

fn render_model_and_executor(
    model_ref: &str,
    backend_ref: &str,
) -> Result<String, ManagedModelIdError> {
    if model_ref.is_empty() {
        return Err(ManagedModelIdError::UnsupportedSelection);
    }
    let mut rendered = encode_component(model_ref);
    match Backend::from_ref(backend_ref) {
        Backend::Acp { cli } if !cli.is_empty() => {
            push_qualifier(&mut rendered, "executor", &format!("acp:{cli}"));
            Ok(rendered)
        }
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

fn render_binding(binding: &ModelBinding) -> Result<String, ManagedModelIdError> {
    match Backend::from_ref(&binding.backend_ref) {
        Backend::Native if binding.model_ref.is_empty() => {
            Err(ManagedModelIdError::UnsupportedSelection)
        }
        Backend::Native => {
            let mut rendered = encode_component(&binding.model_ref);
            if !binding.provider_identity_ref.is_empty() {
                push_qualifier(&mut rendered, "provider", &binding.provider_identity_ref);
            }
            Ok(rendered)
        }
        Backend::Acp { cli } if !cli.is_empty() && binding.model_ref.is_empty() => {
            render_executor_only(&binding.backend_ref)
        }
        Backend::Acp { cli } if !cli.is_empty() => {
            let mut rendered = encode_component(&binding.model_ref);
            if !binding.provider_identity_ref.is_empty() {
                push_qualifier(&mut rendered, "provider", &binding.provider_identity_ref);
            }
            push_qualifier(&mut rendered, "executor", &format!("acp:{cli}"));
            Ok(rendered)
        }
        Backend::Remote { endpoint }
            if !endpoint.is_empty()
                && binding.provider_identity_ref.is_empty()
                && binding.model_ref.is_empty() =>
        {
            Ok(format!(
                "executor={}",
                encode_component(&format!("a2a:{endpoint}"))
            ))
        }
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

fn push_qualifier(rendered: &mut String, key: &str, value: &str) {
    rendered.push(';');
    rendered.push_str(key);
    rendered.push('=');
    rendered.push_str(&encode_component(value));
}

fn encode_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => encoded.push_str("%25"),
            ';' => encoded.push_str("%3B"),
            '=' => encoded.push_str("%3D"),
            other => encoded.push(other),
        }
    }
    encoded
}

fn decode_component(value: &str, original: &str) -> Result<String, ManagedModelIdError> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 2 >= bytes.len() {
            return Err(ManagedModelIdError::Invalid(original.into()));
        }
        let high = hex(bytes[index + 1]);
        let low = hex(bytes[index + 2]);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(ManagedModelIdError::Invalid(original.into()));
        };
        decoded.push(high << 4 | low);
        index += 3;
    }
    let decoded =
        String::from_utf8(decoded).map_err(|_| ManagedModelIdError::Invalid(original.into()))?;
    if decoded.chars().any(char::is_whitespace) {
        return Err(ManagedModelIdError::Invalid(original.into()));
    }
    Ok(decoded)
}

fn hex(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_complete_composable_model_reference_decision_table() {
        // Cause/effect graph:
        // C1=model present; C2=provider present; C3=API present; C4=endpoint
        // present; C5=executor native/ACP/A2A; C6=profile; C7=reserved bytes.
        // Effects: E1=catalog Target; E2=ACP backend default; E3=remote pin;
        // E4=profile selection; E5=reject; E6=canonical round trip.
        //
        // | Rule | model | provider/api/endpoint | executor | effect |
        // | R1   | yes   | none                  | native   | E1,E6  |
        // | R2   | yes   | provider[/api[/ep]]   | native   | E1,E6  |
        // | R3   | yes   | any valid chain       | ACP      | E1,E6  |
        // | R4   | no    | none                  | ACP      | E2,E6  |
        // | R5   | no    | none                  | A2A      | E3,E6  |
        // | R6   | no    | profile only          | n/a      | E4,E6  |
        // | R7   | any invalid dependency/dup    | any      | E5     |
        // | R8   | reserved component bytes      | any      | E6     |
        let cases = [
            ("gpt-5", "gpt-5"),
            ("gpt-5;provider=openai", "gpt-5;provider=openai"),
            (
                "gpt-5;provider=openai;api=open_ai_responses",
                "gpt-5;provider=openai;api=open_ai_responses",
            ),
            (
                "gpt-5;provider=openai;api=open_ai_responses;endpoint=primary",
                "gpt-5;provider=openai;api=open_ai_responses;endpoint=primary",
            ),
            (
                "gpt-5;executor=acp:codex;provider=openai;api=open_ai_responses",
                "gpt-5;provider=openai;api=open_ai_responses;executor=acp:codex",
            ),
            ("gpt-5;executor=acp:codex", "gpt-5;executor=acp:codex"),
            ("executor=acp:codex", "executor=acp:codex"),
            (
                "executor=a2a:https://agent.example/a2a",
                "executor=a2a:https://agent.example/a2a",
            ),
            ("profile=research-primary", "profile=research-primary"),
            (
                "model%3Bspecial;provider=vendor%3Dedge",
                "model%3Bspecial;provider=vendor%3Dedge",
            ),
            (
                "qwen/qwen3-235b;provider=anyrouter;api=open_ai_chat;executor=acp:codex",
                "qwen/qwen3-235b;provider=anyrouter;api=open_ai_chat;executor=acp:codex",
            ),
        ];
        for (wire, canonical) in cases {
            let selection = parse_managed_model_id(wire).expect(wire);
            assert_eq!(
                render_managed_model_id(&selection).unwrap(),
                canonical,
                "{wire}"
            );
        }
        for invalid in [
            "",
            "executor=native",
            "executor=acp:",
            "executor=a2a:",
            "gpt-5;api=open_ai_chat",
            "gpt-5;endpoint=primary",
            "gpt-5;provider=openai;endpoint=primary",
            "gpt-5;provider=openai;provider=other",
            "gpt-5;unknown=value",
            "gpt-5;executor=a2a:https://agent.example/a2a",
            "profile=one;executor=acp:codex",
            "gpt%20five",
            "gpt-5%ZZ",
        ] {
            assert!(parse_managed_model_id(invalid).is_err(), "{invalid}");
        }
    }
}
