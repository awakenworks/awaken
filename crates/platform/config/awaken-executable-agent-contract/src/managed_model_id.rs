// @formal surface requirement=control.agent_profiles.immutable_profile
// @formal reason: One pure Config contract codec maps public selectors to the existing
// @formal reason: authoring value and back. Publication discovery uses the same renderer;
// @formal reason: this module grants no model, credential, catalog or runtime authority.
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

use crate::ModelSelection;
use awaken_agent_contract::{AcpSessionConfiguration, ModelTarget};
use awaken_runtime_contract::resolved::{Backend, ModelBinding};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedModelIdError {
    #[error("model id must not be empty")]
    Empty,
    #[error("invalid Managed model id: {0}")]
    Invalid(ManagedModelIdRefusal),
    #[error("model selection cannot be represented by a Managed model id")]
    UnsupportedSelection,
}

/// Finite public-selector refusal classes. The codec carries no arbitrary
/// diagnostic string; callers can classify grammar, encoding and intent errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManagedModelIdRefusal {
    #[error("malformed selector syntax")]
    Syntax,
    #[error("unsupported executor selection")]
    Executor,
    #[error("malformed component encoding")]
    Encoding,
    #[error("incomplete provider target")]
    IncompleteTarget,
    #[error("conflicting source and provider selectors")]
    ConflictingSource,
    #[error("invalid backend-owned model selection")]
    BackendSelection,
}

/// Parse a Managed-compatible model string into authoring intent.
pub fn parse_managed_model_id(value: &str) -> Result<ModelSelection, ManagedModelIdError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(ManagedModelIdError::Empty);
    }
    if value.chars().any(char::is_whitespace) {
        return Err(ManagedModelIdError::Invalid(ManagedModelIdRefusal::Syntax));
    }

    if let Some(profile) = value.strip_prefix("profile=") {
        let profile_id = decode_component(profile)?;
        if profile_id.is_empty() || profile_id.contains(';') {
            return Err(ManagedModelIdError::Invalid(ManagedModelIdRefusal::Syntax));
        }
        return Ok(ModelSelection::Profile { profile_id });
    }

    if let Some(executor) = value.strip_prefix("executor=") {
        if executor.contains(';') {
            return Err(ManagedModelIdError::Invalid(ManagedModelIdRefusal::Syntax));
        }
        return executor_only(&decode_component(executor)?);
    }

    let mut parts = value.split(';');
    let model_id = decode_component(parts.next().unwrap_or_default())?;
    if model_id.is_empty() {
        return Err(ManagedModelIdError::Invalid(ManagedModelIdRefusal::Syntax));
    }
    let mut qualifiers = BTreeMap::new();
    for part in parts {
        let Some((key, encoded)) = part.split_once('=') else {
            return Err(ManagedModelIdError::Invalid(ManagedModelIdRefusal::Syntax));
        };
        if !matches!(key, "provider" | "api" | "endpoint" | "executor" | "source")
            || encoded.is_empty()
            || qualifiers.insert(key, decode_component(encoded)?).is_some()
        {
            return Err(ManagedModelIdError::Invalid(ManagedModelIdRefusal::Syntax));
        }
    }

    let provider_id = qualifiers.remove("provider");
    let api_dialect = qualifiers.remove("api");
    let endpoint_name = qualifiers.remove("endpoint");
    let source = qualifiers.remove("source");
    let executor = qualifiers
        .remove("executor")
        .unwrap_or_else(|| "native".into());
    if !qualifiers.is_empty()
        || api_dialect.is_some() && provider_id.is_none()
        || endpoint_name.is_some() && (provider_id.is_none() || api_dialect.is_none())
    {
        return Err(ManagedModelIdError::Invalid(
            ManagedModelIdRefusal::IncompleteTarget,
        ));
    }

    let backend_ref = parse_executor(&executor)?;
    // Partial catalog Target and backend-owned exact model are distinct
    // existing intents. Keep old selectors and make source explicit, so the
    // canonical renderer/parser cannot silently change their meaning.
    if let Some(source) = source {
        if source != "backend"
            || provider_id.is_some()
            || api_dialect.is_some()
            || endpoint_name.is_some()
            || !matches!(Backend::from_ref(&backend_ref), Backend::Acp(_))
        {
            return Err(ManagedModelIdError::Invalid(
                ManagedModelIdRefusal::ConflictingSource,
            ));
        }
        return ModelSelection::try_backend_exact(
            backend_ref,
            model_id,
            AcpSessionConfiguration::default(),
        )
        .map_err(|_| ManagedModelIdError::Invalid(ManagedModelIdRefusal::BackendSelection));
    }
    match Backend::from_ref(&backend_ref) {
        Backend::Native | Backend::Acp(_) => Ok(ModelSelection::Target {
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
        Backend::Remote(_) | Backend::Invalid(_) => {
            Err(ManagedModelIdError::Invalid(ManagedModelIdRefusal::Syntax))
        }
    }
}

fn executor_only(executor: &str) -> Result<ModelSelection, ManagedModelIdError> {
    let backend_ref = parse_executor(executor)?;
    match Backend::from_ref(&backend_ref) {
        Backend::Acp(_) => {
            ModelSelection::try_backend_default(backend_ref, AcpSessionConfiguration::default())
                .map_err(|_| ManagedModelIdError::Invalid(ManagedModelIdRefusal::Executor))
        }
        Backend::Remote(_) => ModelBinding::parse("", "", backend_ref)
            .map(ModelSelection::Pinned)
            .map_err(|_| ManagedModelIdError::Invalid(ManagedModelIdRefusal::Executor)),
        Backend::Native | Backend::Invalid(_) => Err(ManagedModelIdError::Invalid(
            ManagedModelIdRefusal::Executor,
        )),
    }
}

fn parse_executor(value: &str) -> Result<String, ManagedModelIdError> {
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
    Err(ManagedModelIdError::Invalid(
        ManagedModelIdRefusal::Executor,
    ))
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
        ModelSelection::BackendDefault { backend_ref, .. } => {
            render_executor_only(backend_ref.backend_ref())
        }
        ModelSelection::BackendExact {
            backend_ref,
            model_ref,
            ..
        } => render_model_and_executor(model_ref.as_str(), backend_ref.backend_ref()),
        ModelSelection::Pinned(binding) => render_binding(binding),
        ModelSelection::Profile { profile_id } if !profile_id.is_empty() => {
            Ok(format!("profile={}", encode_component(profile_id)))
        }
        ModelSelection::Auto | ModelSelection::Profile { .. } => {
            Err(ManagedModelIdError::UnsupportedSelection)
        }
    }
}

/// Pure source-owned projection of frozen execution metadata to a public model ID.
/// Implementations may not query a mutable catalog, resolve credentials or own
/// registrations. IDs always use the one Managed authoring codec below.
pub trait PublishedModelReferenceProjector: Send + Sync {
    fn project(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
    ) -> Result<Option<ModelSelection>, ManagedModelIdError>;
}

/// Standalone Config, backend-owned and installed-executor reference projection.
pub struct CanonicalPublishedModelReferenceProjector;
impl PublishedModelReferenceProjector for CanonicalPublishedModelReferenceProjector {
    fn project(
        &self,
        candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
    ) -> Result<Option<ModelSelection>, ManagedModelIdError> {
        published_model_selection(candidate)
    }
}

/// Project one already-published candidate to an ID the public authoring codec
/// accepts. Provider/account identity is not the vendor coordinate, and the
/// exact frozen route must not collapse into its bare model name.
pub(crate) fn published_model_selection(
    candidate: &awaken_runtime_contract::resolved::ResolvedModelCandidate,
) -> Result<Option<ModelSelection>, ManagedModelIdError> {
    use awaken_runtime_contract::resolved::ModelProvisioning;
    let binding = candidate.binding();
    if binding.identity().1.is_empty() {
        return Ok(None);
    }
    let selection = match candidate.provisioning() {
        ModelProvisioning::Provider {
            provider_ref,
            route_ref,
            endpoint,
            ..
        } => {
            if endpoint.api_dialect.is_empty() {
                return Err(ManagedModelIdError::UnsupportedSelection);
            }
            ModelSelection::Target {
                target: ModelTarget {
                    model_id: binding.identity().1.into(),
                    provider_id: Some(published_reference_id(provider_ref)?.into()),
                    api_dialect: Some(endpoint.api_dialect.clone()),
                    protocol_endpoint_id: None,
                    // The existing Offering selector admits an exact endpoint
                    // ID here as well as its human name. No mutable lookup or
                    // inferred account/source precedence is involved.
                    endpoint_name: Some(published_reference_id(route_ref)?.into()),
                },
                backend_ref: binding.identity().2.into(),
                configuration: AcpSessionConfiguration::default(),
            }
        }
        ModelProvisioning::BackendOwned { .. } => ModelSelection::try_backend_exact(
            binding.identity().2,
            binding.identity().1,
            AcpSessionConfiguration::default(),
        )
        .map_err(|_| ManagedModelIdError::UnsupportedSelection)?,
        ModelProvisioning::HostExecutor => ModelSelection::Pinned(binding.clone()),
        ModelProvisioning::Remote { .. } => return Ok(None),
    };
    Ok(Some(selection))
}

fn published_reference_id(reference: &str) -> Result<&str, ManagedModelIdError> {
    let (id, revision) = reference
        .rsplit_once('@')
        .ok_or(ManagedModelIdError::UnsupportedSelection)?;
    if id.is_empty()
        || id.trim() != id
        || revision
            .parse::<u64>()
            .ok()
            .is_none_or(|revision| revision == 0)
    {
        return Err(ManagedModelIdError::UnsupportedSelection);
    }
    Ok(id)
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
        Backend::Acp(backend) => {
            push_qualifier(&mut rendered, "executor", &format!("acp:{backend}"));
            Ok(rendered)
        }
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

fn render_executor_only(backend_ref: &str) -> Result<String, ManagedModelIdError> {
    match Backend::from_ref(backend_ref) {
        Backend::Acp(backend) => Ok(format!("executor=acp:{backend}")),
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
        Backend::Acp(backend) => {
            push_qualifier(&mut rendered, "executor", &format!("acp:{backend}"));
            push_qualifier(&mut rendered, "source", "backend");
            Ok(rendered)
        }
        _ => Err(ManagedModelIdError::UnsupportedSelection),
    }
}

fn render_binding(binding: &ModelBinding) -> Result<String, ManagedModelIdError> {
    match Backend::from_ref(binding.identity().2) {
        Backend::Native if binding.identity().1.is_empty() => {
            Err(ManagedModelIdError::UnsupportedSelection)
        }
        Backend::Native => {
            let mut rendered = encode_component(binding.identity().1);
            if !binding.identity().0.is_empty() {
                push_qualifier(&mut rendered, "provider", binding.identity().0);
            }
            Ok(rendered)
        }
        Backend::Acp(_) if binding.identity().1.is_empty() => {
            render_executor_only(binding.identity().2)
        }
        Backend::Acp(backend) => {
            let mut rendered = encode_component(binding.identity().1);
            if !binding.identity().0.is_empty() {
                push_qualifier(&mut rendered, "provider", binding.identity().0);
            }
            push_qualifier(&mut rendered, "executor", &format!("acp:{backend}"));
            Ok(rendered)
        }
        Backend::Remote(endpoint)
            if binding.identity().0.is_empty() && binding.identity().1.is_empty() =>
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

fn decode_component(value: &str) -> Result<String, ManagedModelIdError> {
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
            return Err(ManagedModelIdError::Invalid(
                ManagedModelIdRefusal::Encoding,
            ));
        }
        let high = hex(bytes[index + 1]);
        let low = hex(bytes[index + 2]);
        let (Some(high), Some(low)) = (high, low) else {
            return Err(ManagedModelIdError::Invalid(
                ManagedModelIdRefusal::Encoding,
            ));
        };
        decoded.push(high << 4 | low);
        index += 3;
    }
    let decoded = String::from_utf8(decoded)
        .map_err(|_| ManagedModelIdError::Invalid(ManagedModelIdRefusal::Encoding))?;
    if decoded.chars().any(char::is_whitespace) {
        return Err(ManagedModelIdError::Invalid(
            ManagedModelIdRefusal::Encoding,
        ));
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
    use proptest::prelude::*;

    #[test]
    fn invalid_selectors_preserve_a_typed_refusal_cause() {
        // C1 malformed grammar; C2 incomplete provider axes; C3 unsupported
        // executor; C4 bad percent/UTF-8 encoding; C5 conflicting source.
        // R1-R5 each returns its finite cause, allowing callers to classify
        // without matching a free-form diagnostic or exposing rejected input.
        for (input, cause) in [
            ("model;unknown=value", ManagedModelIdRefusal::Syntax),
            (
                "model;endpoint=one",
                ManagedModelIdRefusal::IncompleteTarget,
            ),
            (
                "model;executor=unsupported",
                ManagedModelIdRefusal::Executor,
            ),
            ("model%ZZ", ManagedModelIdRefusal::Encoding),
            (
                "model;source=backend",
                ManagedModelIdRefusal::ConflictingSource,
            ),
        ] {
            assert_eq!(
                parse_managed_model_id(input),
                Err(ManagedModelIdError::Invalid(cause))
            );
        }
    }

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

    #[test]
    fn backend_owned_exact_model_round_trips_without_a_provider_catalog_route() {
        // B1 explicit backend source + ACP + no provider -> BackendExact;
        // B2 ordinary complete/partial selectors keep their Target meaning;
        // B3 provider/native/unknown source conflicts -> reject. This avoids a
        // transient catalog miss, credential state or a default source priority.
        for cli in [
            "claude",
            "codex",
            "gemini",
            "opencode",
            "hermes",
            "third-party",
        ] {
            let exact = ModelSelection::try_backend_exact(
                format!("acp:{cli}"),
                "vendor/model;v=1",
                Default::default(),
            )
            .unwrap();
            let wire = render_managed_model_id(&exact).unwrap();
            assert_eq!(parse_managed_model_id(&wire).unwrap(), exact, "B1 {cli}");
            let targeted = format!("model;provider=vendor;api=open_ai_chat;executor=acp:{cli}");
            assert!(
                parse_managed_model_id(&targeted)
                    .unwrap()
                    .target()
                    .is_some(),
                "B2 {cli}"
            );
            let partial = format!("model;executor=acp:{cli}");
            let selection = parse_managed_model_id(&partial).unwrap();
            assert!(
                selection.target().is_some(),
                "B2 original partial Target {cli}"
            );
            assert_eq!(
                parse_managed_model_id(&render_managed_model_id(&selection).unwrap()).unwrap(),
                selection
            );
        }
        for invalid in [
            "model;source=backend",
            "model;executor=acp:codex;provider=vendor;source=backend",
            "model;executor=acp:codex;source=unknown",
        ] {
            assert!(parse_managed_model_id(invalid).is_err(), "B3 {invalid}");
        }
    }

    #[test]
    fn provider_and_runtime_selector_matrix_is_open_and_fail_closed() {
        // Finite model check of the public grammar, not a provider allowlist.
        // Every provider/API identifier is opaque; validity depends only on the
        // documented dependency chain provider -> API -> endpoint and on the
        // executor's ability to accept an explicit model. This covers built-in
        // names, third-party names, and all runtime families.
        let providers = [
            None,
            Some("anthropic"),
            Some("openai"),
            Some("deepseek"),
            Some("kimi"),
            Some("gemini"),
            Some("vertex"),
            Some("anyrouter"),
            Some("third-party/vendor=v2"),
        ];
        let apis = [
            None,
            Some("anthropic_messages"),
            Some("open_ai_chat"),
            Some("open_ai_responses"),
            Some("gemini"),
            Some("vertex_gemini"),
            Some("third-party/messages=v9"),
        ];
        let endpoints = [None, Some("primary"), Some("regional/edge=v3")];
        let executors = [
            "native",
            "acp:codex",
            "acp:claude",
            "acp:gemini",
            "acp:opencode",
            "acp:hermes",
            "a2a:https://agent.example/a2a",
        ];
        let mut checked = 0;

        for provider in providers {
            for api in apis {
                for endpoint in endpoints {
                    for executor in executors {
                        checked += 1;
                        let mut wire = "vendor/model-v1".to_string();
                        if let Some(provider) = provider {
                            push_qualifier(&mut wire, "provider", provider);
                        }
                        if let Some(api) = api {
                            push_qualifier(&mut wire, "api", api);
                        }
                        if let Some(endpoint) = endpoint {
                            push_qualifier(&mut wire, "endpoint", endpoint);
                        }
                        push_qualifier(&mut wire, "executor", executor);

                        let valid_dependencies = api.is_none() || provider.is_some();
                        let valid_endpoint =
                            endpoint.is_none() || provider.is_some() && api.is_some();
                        let executor_accepts_model = !executor.starts_with("a2a:");
                        let parsed = parse_managed_model_id(&wire);
                        assert_eq!(
                            parsed.is_ok(),
                            valid_dependencies && valid_endpoint && executor_accepts_model,
                            "matrix case {wire}"
                        );
                        if let Ok(selection) = parsed {
                            let canonical = render_managed_model_id(&selection).expect("render");
                            assert_eq!(
                                parse_managed_model_id(&canonical).expect("canonical parse"),
                                selection,
                                "round trip {wire}"
                            );
                        }
                    }
                }
            }
        }
        assert_eq!(checked, 1_323);

        for executor in [
            "acp:codex",
            "acp:claude",
            "acp:gemini",
            "acp:opencode",
            "acp:hermes",
        ] {
            let wire = format!("executor={executor}");
            let selection = parse_managed_model_id(&wire).expect(&wire);
            assert_eq!(render_managed_model_id(&selection).unwrap(), wire);
        }
        let a2a = "executor=a2a:https://third-party.example/agents/finance";
        let selection = parse_managed_model_id(a2a).expect("A2A runtime");
        assert_eq!(render_managed_model_id(&selection).unwrap(), a2a);
    }

    proptest! {
        #[test]
        fn arbitrary_third_party_components_round_trip_canonically(
            model in "[A-Za-z0-9_./:;=%-]{1,32}",
            provider in "[A-Za-z0-9_./:;=%-]{1,32}",
            api in "[A-Za-z0-9_./:;=%-]{1,32}",
            endpoint in "[A-Za-z0-9_./:;=%-]{1,32}",
            cli in "[A-Za-z0-9_.=-]{1,16}",
            use_acp in any::<bool>(),
        ) {
            let selection = ModelSelection::Target {
                target: ModelTarget {
                    model_id: model,
                    provider_id: Some(provider),
                    api_dialect: Some(api),
                    protocol_endpoint_id: None,
                    endpoint_name: Some(endpoint),
                },
                backend_ref: if use_acp { format!("acp:{cli}") } else { "genai".into() },
                configuration: AcpSessionConfiguration::default(),
            };
            let wire = render_managed_model_id(&selection).expect("representable selection");
            let parsed = parse_managed_model_id(&wire).expect("rendered id parses");
            prop_assert_eq!(&parsed, &selection);
            prop_assert_eq!(render_managed_model_id(&parsed).unwrap(), wire);
        }

        #[test]
        fn malformed_percent_escapes_always_fail_closed(
            model in "[A-Za-z0-9_./:-]{1,32}",
            suffix in prop::sample::select(vec!["%", "%0", "%GG", "%0G", "%G0", "%FF"]),
        ) {
            let wire = format!("{model}{suffix};provider=third-party");
            prop_assert!(parse_managed_model_id(&wire).is_err());
        }
    }

    #[test]
    fn unicode_and_large_opaque_components_are_lossless() {
        let model = format!("模型/{};版本=β%", "m".repeat(4_096));
        let selection = ModelSelection::Target {
            target: ModelTarget {
                model_id: model,
                provider_id: Some("供应商/网关=v2".into()),
                api_dialect: Some("消息;协议%9".into()),
                protocol_endpoint_id: None,
                endpoint_name: Some("亚洲/边缘=主".into()),
            },
            backend_ref: "acp:第三方-cli".into(),
            configuration: AcpSessionConfiguration::default(),
        };
        let wire = render_managed_model_id(&selection).expect("large Unicode selection");
        assert_eq!(parse_managed_model_id(&wire).unwrap(), selection);
    }
}
