use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use awaken_server_contract::ProviderSpec;
use serde_json::{Value, json};

use super::*;

fn provider_spec_with_kind_and_key(
    adapter: &str,
    kind: Option<&str>,
    api_key: Option<&str>,
) -> ProviderSpec {
    let mut options: BTreeMap<String, Value> = BTreeMap::new();
    if let Some(kind) = kind {
        options.insert("credentials_kind".into(), json!(kind));
    }
    ProviderSpec {
        id: format!("test-{adapter}"),
        adapter: adapter.into(),
        api_key: api_key.map(|key| key.to_string().into()),
        adapter_options: options,
        ..ProviderSpec::default()
    }
}

fn provider_spec_with_env_credentials(adapter: &str, kind: Option<&str>) -> ProviderSpec {
    let mut spec = provider_spec_with_kind_and_key(adapter, kind, None);
    spec.adapter_options
        .insert("allow_env_credentials".into(), json!(true));
    spec
}

fn test_broker() -> Arc<dyn awaken_runtime::credentials::CredentialBroker> {
    Arc::new(awaken_runtime::credentials::AwakenCredentialBroker::new())
}

#[test]
fn supported_adapters_includes_recent_additions() {
    let names: std::collections::HashSet<&str> = supported_adapters().into_iter().collect();
    for required in ["vertex", "github_copilot", "ollama_cloud"] {
        assert!(
            names.contains(required),
            "expected adapter {required} to be exposed via supported_adapters()"
        );
    }
}

#[test]
fn supported_adapters_filters_unknown_candidates() {
    let names: std::collections::HashSet<&str> = supported_adapters().into_iter().collect();
    for speculative in ["bedrock", "azure", "azure_openai", "mistral", "perplexity"] {
        if AdapterKind::from_lower_str(speculative).is_none() {
            assert!(
                !names.contains(speculative),
                "speculative candidate {speculative} leaked into supported_adapters() despite genai not supporting it"
            );
        }
    }
}

#[test]
fn unsupported_adapter_error_points_at_genai_docs() {
    let err = parse_adapter_kind("definitely-not-a-real-adapter").unwrap_err();
    let display = err.to_string();
    assert!(
        display.contains("definitely-not-a-real-adapter"),
        "error must echo the offending name, got: {display}"
    );
    assert!(
        display.contains("docs.rs/genai"),
        "error must point operators at genai's AdapterKind docs, got: {display}"
    );
}

#[test]
fn build_genai_executor_for_every_supported_adapter() {
    for name in supported_adapters() {
        let mut adapter_options = BTreeMap::new();
        adapter_options.insert("allow_env_credentials".into(), json!(true));
        let spec = ProviderSpec {
            id: format!("test-{name}"),
            adapter: name.to_string(),
            adapter_options,
            ..ProviderSpec::default()
        };
        build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
            panic!("supported adapter `{name}` failed to build executor: {err:?}")
        });
    }
}

#[test]
fn build_genai_executor_with_api_key_for_every_supported_adapter() {
    for name in supported_adapters() {
        let spec = ProviderSpec {
            id: format!("test-{name}"),
            adapter: name.to_string(),
            api_key: Some("test-secret-key".to_string().into()),
            ..ProviderSpec::default()
        };
        build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
            panic!("supported adapter `{name}` (with api_key) failed to build: {err:?}")
        });
    }
}

#[test]
fn build_genai_executor_with_base_url_override_for_every_supported_adapter() {
    for name in supported_adapters() {
        let spec = ProviderSpec {
            id: format!("test-{name}"),
            adapter: name.to_string(),
            api_key: Some("test-secret-key".to_string().into()),
            base_url: Some("https://example.invalid/v1".to_string()),
            ..ProviderSpec::default()
        };
        build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
            panic!("supported adapter `{name}` (with base_url) failed to build: {err:?}")
        });
    }
}

#[test]
fn build_genai_executor_with_full_options_for_every_supported_adapter() {
    for name in supported_adapters() {
        let mut adapter_options = BTreeMap::new();
        adapter_options.insert(
            "headers".into(),
            json!({ "X-Awaken-Trace": "regression-test" }),
        );
        adapter_options.insert("future_extension_key".into(), json!({ "ignored": true }));
        let spec = ProviderSpec {
            id: format!("test-{name}"),
            adapter: name.to_string(),
            api_key: Some("test-secret-key".to_string().into()),
            base_url: Some("https://example.invalid/v1".to_string()),
            timeout_secs: 60,
            adapter_options,
        };
        build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
            panic!("supported adapter `{name}` (full options) failed to build: {err:?}")
        });
    }
}

#[test]
fn build_genai_executor_clamps_zero_timeout_for_every_supported_adapter() {
    for name in supported_adapters() {
        let spec = ProviderSpec {
            id: format!("test-{name}"),
            adapter: name.to_string(),
            api_key: Some("test-secret-key".to_string().into()),
            timeout_secs: 0,
            ..ProviderSpec::default()
        };
        build_genai_provider_executor_with_broker(&spec, test_broker()).unwrap_or_else(|err| {
            panic!("supported adapter `{name}` (zero timeout) failed to build: {err:?}")
        });
    }
}

#[test]
fn parse_adapter_kind_is_case_insensitive_for_every_supported_adapter() {
    for name in supported_adapters() {
        let upper = name.to_ascii_uppercase();
        let mixed: String = name
            .chars()
            .enumerate()
            .map(|(index, character)| {
                if index % 2 == 0 {
                    character.to_ascii_uppercase()
                } else {
                    character
                }
            })
            .collect();
        for variant in [name.to_string(), upper, mixed, format!("  {name}  ")] {
            parse_adapter_kind(&variant).unwrap_or_else(|err| {
                panic!("`{variant}` (canonical: {name}) failed to parse: {err:?}")
            });
        }
    }
}

#[test]
fn supported_adapters_unique_no_duplicate_names() {
    let names: Vec<&'static str> = supported_adapters();
    let mut seen = std::collections::HashSet::with_capacity(names.len());
    for name in &names {
        assert!(
            seen.insert(*name),
            "duplicate entry `{name}` in supported_adapters()"
        );
    }
    assert!(
        names.len() >= 19,
        "supported_adapters() shrank below floor of 19 (got {}): {names:?}",
        names.len()
    );
}

#[test]
fn vertex_anthropic_namespaces_parse_when_routed_through_adapter_string() {
    let kind = parse_adapter_kind("vertex").expect("vertex must parse");
    assert_eq!(kind, AdapterKind::Vertex);
    let kind = parse_adapter_kind("github_copilot").expect("github_copilot must parse");
    assert_eq!(kind, AdapterKind::GithubCopilot);
    let cloud = parse_adapter_kind("ollama_cloud").expect("ollama_cloud must parse");
    let local = parse_adapter_kind("ollama").expect("ollama must parse");
    assert_ne!(
        cloud, local,
        "ollama_cloud and ollama must map to distinct kinds"
    );
    assert_eq!(cloud, AdapterKind::OllamaCloud);
    assert_eq!(local, AdapterKind::Ollama);
}

#[test]
fn parse_adapter_kind_accepts_legacy_aliases() {
    assert_eq!(
        parse_adapter_kind("openai-resp").unwrap(),
        AdapterKind::OpenAIResp
    );
    assert_eq!(
        parse_adapter_kind("responses").unwrap(),
        AdapterKind::OpenAIResp
    );
    assert_eq!(
        parse_adapter_kind("  Anthropic ").unwrap(),
        AdapterKind::Anthropic
    );
}

#[test]
fn build_genai_omitted_api_key_is_rejected_by_default() {
    let spec = provider_spec_with_kind_and_key("openai", None, None);
    let err = match build_genai_provider_executor_with_broker(&spec, test_broker()) {
        Ok(_) => panic!("missing bearer api_key must fail closed"),
        Err(error) => error,
    };
    assert!(
        matches!(err, ConfigRuntimeError::InvalidConfig(ref message) if message.contains("api_key")),
        "expected InvalidConfig naming api_key, got: {err:?}"
    );
}

#[test]
fn build_genai_env_credentials_requires_explicit_opt_in() {
    let spec = provider_spec_with_env_credentials("openai", None);
    build_genai_provider_executor_with_broker(&spec, test_broker())
        .expect("explicit env-credential bearer must build");
}

#[test]
fn build_genai_explicit_bearer_succeeds() {
    let spec = provider_spec_with_kind_and_key("openai", Some("bearer"), Some("sk-test-123"));
    build_genai_provider_executor_with_broker(&spec, test_broker())
        .expect("explicit bearer must build");
}

#[test]
fn build_genai_unknown_credentials_kind_rejected_with_clear_error() {
    let spec =
        provider_spec_with_kind_and_key("openai", Some("never-heard-of-it"), Some("sk-test-123"));
    let err = build_genai_provider_executor_with_broker(&spec, test_broker())
        .err()
        .expect("expected error");
    assert!(
        matches!(err, ConfigRuntimeError::InvalidConfig(ref message) if message.contains("never-heard-of-it")),
        "expected InvalidConfig naming the bad kind, got: {err:?}"
    );
}

#[test]
fn build_genai_service_account_kind_with_non_vertex_adapter_rejected() {
    let spec = provider_spec_with_kind_and_key(
        "openai",
        Some("service_account_json"),
        Some(r#"{"client_email":"x@y","private_key":"-----BEGIN PRIVATE KEY-----"}"#),
    );
    let err = build_genai_provider_executor_with_broker(&spec, test_broker())
        .err()
        .expect("expected error");
    assert!(
        matches!(err, ConfigRuntimeError::InvalidConfig(ref message)
            if message.contains("service_account_json")
                && message.contains("vertex")
                && message.contains("openai")),
        "expected InvalidConfig naming the kind/adapter mismatch, got: {err:?}"
    );
}

#[derive(Default)]
struct RecordingBroker {
    registered: parking_lot::Mutex<Vec<String>>,
}

#[async_trait]
impl awaken_runtime::credentials::CredentialBroker for RecordingBroker {
    fn register(
        &self,
        provider_id: String,
        _material: awaken_runtime::credentials::CredentialMaterial,
    ) {
        self.registered.lock().push(provider_id);
    }

    async fn token_for(
        &self,
        _provider_id: &str,
        _scope: &str,
    ) -> Result<
        awaken_runtime::credentials::IssuedToken,
        awaken_runtime::credentials::CredentialError,
    > {
        unreachable!("static-bearer build must not call token_for");
    }
}

#[test]
fn build_genai_static_bearer_does_not_register_with_broker() {
    let recording: Arc<RecordingBroker> = Arc::new(RecordingBroker::default());
    let broker: Arc<dyn awaken_runtime::credentials::CredentialBroker> =
        Arc::clone(&recording) as _;

    let spec = provider_spec_with_kind_and_key("openai", Some("bearer"), Some("sk-x"));
    build_genai_provider_executor_with_broker(&spec, broker).expect("static bearer must build");

    assert!(
        recording.registered.lock().is_empty(),
        "static bearer must not register with the broker"
    );
}

#[test]
fn build_genai_env_credentials_do_not_register_with_broker() {
    let recording: Arc<RecordingBroker> = Arc::new(RecordingBroker::default());
    let broker: Arc<dyn awaken_runtime::credentials::CredentialBroker> =
        Arc::clone(&recording) as _;

    let spec = provider_spec_with_env_credentials("openai", None);
    build_genai_provider_executor_with_broker(&spec, broker)
        .expect("explicit env fallback must build");

    assert!(
        recording.registered.lock().is_empty(),
        "explicit env fallback must not register with the broker"
    );
}
