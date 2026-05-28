use std::collections::HashSet;
use std::time::Instant;

use awaken_runtime::registry::{
    ProviderRemovalPreview, SerializableRegistryDiagnostic, diagnose_registry_set_serializable,
};
use awaken_server_contract::{AgentSpec, ConfigRecord, ProviderSpec};
use serde_json::{Map, Value};

use super::normalization::into_object;
use super::{
    ConfigNamespace, ConfigService, ConfigServiceError, ProviderTestResult,
    effective_visible_record,
};

impl ConfigService {
    pub async fn preview_remove_provider(
        &self,
        id: &str,
    ) -> Result<ProviderRemovalPreview, ConfigServiceError> {
        if self
            .store
            .get(ConfigNamespace::Providers.as_str(), id)
            .await?
            .is_none()
        {
            return Err(ConfigServiceError::NotFound(format!("providers/{id}")));
        }

        let model_refs = self.find_dependents(ConfigNamespace::Providers, id).await?;
        let model_ids = model_refs
            .into_iter()
            .map(|reference| reference.id)
            .collect::<Vec<_>>();
        let agent_ids = self.agents_referencing_models(&model_ids).await?;

        Ok(ProviderRemovalPreview::new(id, model_ids, agent_ids))
    }

    /// Single-pass scan: returns agent ids whose effective spec references
    /// any of the supplied `model_ids` and does not declare a remote endpoint.
    /// Avoids the previous O(model_ids × agents) double scan in
    /// `preview_remove_provider` and the provider force-cascade path.
    pub(super) async fn agents_referencing_models(
        &self,
        model_ids: &[String],
    ) -> Result<Vec<String>, ConfigServiceError> {
        if model_ids.is_empty() {
            return Ok(Vec::new());
        }
        let model_id_set: HashSet<&str> = model_ids.iter().map(String::as_str).collect();
        let agents = self.store.list("agents", 0, usize::MAX).await?;
        let mut refs = Vec::new();
        for (agent_id, value) in agents {
            let Some(agent) = effective_visible_record::<AgentSpec>(value)? else {
                continue;
            };
            if agent.endpoint.is_none() && model_id_set.contains(agent.model_id.as_str()) {
                refs.push(agent_id);
            }
        }
        Ok(refs)
    }

    pub fn registry_diagnostics(
        &self,
    ) -> Result<Vec<SerializableRegistryDiagnostic>, ConfigServiceError> {
        let registries = self
            .state
            .run
            .runtime
            .registry_set()
            .ok_or(ConfigServiceError::Apply(
                "runtime does not expose a configurable registry snapshot".into(),
            ))?;
        Ok(diagnose_registry_set_serializable(&registries))
    }

    pub(super) async fn normalize_provider_payload(
        &self,
        path_id: Option<&str>,
        body: &mut Map<String, Value>,
    ) -> Result<(), ConfigServiceError> {
        let explicit_clear = matches!(body.get("api_key"), Some(Value::String(value)) if value.is_empty())
            || matches!(body.get("api_key"), Some(Value::Null));
        if explicit_clear {
            body.remove("api_key");
            return Ok(());
        }

        if body.contains_key("api_key") || path_id.is_none() {
            return Ok(());
        }

        let Some(path_id) = path_id else {
            return Ok(());
        };
        let Some(existing) = self
            .store
            .get(ConfigNamespace::Providers.as_str(), path_id)
            .await?
        else {
            return Ok(());
        };
        // The stored value may be either a bare spec or a ConfigRecord envelope.
        // Navigate into spec if needed before accessing fields.
        let spec_value = crate::services::config_envelope::unwrap_spec(existing);
        let Some(existing_object) = spec_value.as_object() else {
            return Ok(());
        };
        if let Some(existing_key) = existing_object.get("api_key") {
            body.insert("api_key".into(), existing_key.clone());
        }
        Ok(())
    }

    /// Test whether a stored provider config is usable.
    ///
    /// Strategy depends on `credentials_kind`:
    ///
    /// - **Static / bearer** (default): construction-only probe. Loads the
    ///   stored `ProviderSpec` and runs `build_genai_provider_executor` to
    ///   prove that the adapter name parses, the api_key (if any) is the
    ///   right shape, and adapter_options are valid. **No network call.**
    ///
    /// - **Dynamic** (`service_account_json`, future cloud creds):
    ///   construction-only probe **plus** a live token mint via the
    ///   credential broker. This catches revoked keys, deleted service
    ///   accounts, unreachable token endpoints, and missing scopes —
    ///   problems that a construction probe cannot see. The mint reuses
    ///   the same broker code that production inference does, so a
    ///   passing test is strong evidence that the next inference will
    ///   succeed at the auth layer.
    ///
    /// In both cases the LLM endpoint itself is not contacted; that
    /// would require a full runtime context (cancellation, streaming,
    /// observability) and would also bill the user for a token.
    pub async fn test_provider(&self, id: &str) -> Result<ProviderTestResult, ConfigServiceError> {
        let raw = self
            .store
            .get(ConfigNamespace::Providers.as_str(), id)
            .await?
            .ok_or_else(|| ConfigServiceError::NotFound(format!("providers/{id}")))?;

        let spec: ProviderSpec = ConfigRecord::<ProviderSpec>::from_value(raw)
            .map_err(|e| ConfigServiceError::InvalidPayload(e.to_string()))
            .map(|r| r.spec)?;

        // Construction probe: catches adapter parsing, material parsing,
        // header validation, and any other build-time check. Reuses the
        // production builder so any change to the build path is covered
        // here automatically.
        let start = Instant::now();
        let broker: std::sync::Arc<dyn awaken_runtime::credentials::CredentialBroker> =
            std::sync::Arc::new(awaken_runtime::credentials::AwakenCredentialBroker::new());
        let build_result =
            crate::services::config_runtime::build_genai_provider_executor_with_broker(
                &spec,
                std::sync::Arc::clone(&broker),
            );
        let mut latency_ms = start.elapsed().as_millis() as u64;

        if let Err(e) = build_result {
            return Ok(ProviderTestResult {
                ok: false,
                latency_ms,
                network_tested: false,
                error: Some(redact_provider_error(&e.to_string(), &spec)),
            });
        }

        // Pre-flight token mint for dynamic credentials. Skipped for bearer
        // (static / env-var fallback) where the broker would either no-op
        // or hand back the static value — neither tests anything new.
        let kind = match awaken_runtime::credentials::CredentialKind::from_options(
            &spec.adapter_options,
        ) {
            Ok(k) => k,
            Err(_) => {
                // Already caught by the build probe above; defensive-coded.
                return Ok(ProviderTestResult {
                    ok: true,
                    latency_ms,
                    network_tested: false,
                    error: None,
                });
            }
        };
        let mut network_tested = false;
        if matches!(
            kind,
            awaken_runtime::credentials::CredentialKind::GoogleServiceAccountJson
        ) {
            let scope = "https://www.googleapis.com/auth/cloud-platform";
            let mint_start = Instant::now();
            network_tested = true;
            let mint_result = broker.token_for(&spec.id, scope).await;
            latency_ms = latency_ms.saturating_add(mint_start.elapsed().as_millis() as u64);
            if let Err(err) = mint_result {
                return Ok(ProviderTestResult {
                    ok: false,
                    latency_ms,
                    network_tested,
                    error: Some(redact_provider_error(&err.to_string(), &spec)),
                });
            }
        }

        Ok(ProviderTestResult {
            ok: true,
            latency_ms,
            network_tested,
            error: None,
        })
    }

    pub(super) fn redact_response(
        &self,
        namespace: ConfigNamespace,
        value: Value,
    ) -> Result<Value, ConfigServiceError> {
        match namespace {
            ConfigNamespace::Providers => {
                let mut object = into_object(value)?;
                let has_api_key = object
                    .get("api_key")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty());
                object.remove("api_key");
                if has_api_key {
                    object.insert("has_api_key".into(), Value::Bool(true));
                }
                Ok(Value::Object(object))
            }
            ConfigNamespace::McpServers => {
                let mut object = into_object(value)?;
                let env_keys = object
                    .get("env")
                    .and_then(Value::as_object)
                    .map(|env| {
                        let mut keys = env.keys().cloned().collect::<Vec<_>>();
                        keys.sort();
                        keys
                    })
                    .unwrap_or_default();
                object.remove("env");
                if !env_keys.is_empty() {
                    object.insert("has_env".into(), Value::Bool(true));
                    object.insert(
                        "env_keys".into(),
                        Value::Array(env_keys.into_iter().map(Value::String).collect()),
                    );
                }
                Ok(Value::Object(object))
            }
            ConfigNamespace::Agents
            | ConfigNamespace::Models
            | ConfigNamespace::ModelPools
            | ConfigNamespace::Skills => Ok(value),
        }
    }
}

fn redact_provider_error(message: &str, spec: &ProviderSpec) -> String {
    let mut needles = Vec::new();
    if let Some(api_key) = spec.api_key.as_ref() {
        let secret = api_key.expose_secret();
        push_redaction_needle(&mut needles, secret);
        if let Ok(value) = serde_json::from_str::<Value>(secret) {
            collect_string_value_needles(&value, &mut needles);
        }
    }
    collect_adapter_option_error_needles(&spec.adapter_options, &mut needles);

    needles.sort_by_key(|needle| std::cmp::Reverse(needle.len()));
    needles.dedup();

    let mut redacted = message.to_owned();
    for needle in needles {
        redacted = redacted.replace(&needle, "***");
    }
    redacted
}

fn collect_adapter_option_error_needles(
    options: &std::collections::BTreeMap<String, Value>,
    needles: &mut Vec<String>,
) {
    for (key, value) in options {
        let lower = key.to_lowercase();
        if lower == "headers" {
            collect_string_key_and_value_needles(value, needles);
        } else if lower.contains("api_key")
            || lower.contains("bearer")
            || lower.contains("private_key")
            || lower.contains("token")
            || lower.contains("password")
            || lower.contains("secret")
        {
            collect_string_value_needles(value, needles);
        }
    }
}

fn collect_string_value_needles(value: &Value, needles: &mut Vec<String>) {
    match value {
        Value::String(value) => push_redaction_needle(needles, value),
        Value::Array(values) => {
            for value in values {
                collect_string_value_needles(value, needles);
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                collect_string_value_needles(value, needles);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn collect_string_key_and_value_needles(value: &Value, needles: &mut Vec<String>) {
    match value {
        Value::String(value) => push_redaction_needle(needles, value),
        Value::Array(values) => {
            for value in values {
                collect_string_key_and_value_needles(value, needles);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                push_redaction_needle(needles, key);
                collect_string_key_and_value_needles(value, needles);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn push_redaction_needle(needles: &mut Vec<String>, value: &str) {
    let value = value.trim();
    if value.len() >= 4 {
        needles.push(value.to_owned());
    }
}
