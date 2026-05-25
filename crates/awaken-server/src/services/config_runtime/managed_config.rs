use awaken_contract::{
    AgentSpec, ConfigRecord, ConfigRevisionRef, McpServerSpec, ModelPoolSpec, ModelSpec,
    ProviderSpec, SkillSpec, ToolSpec, validate_unique_model_ids,
};
use serde_json::Value;

use super::{
    ConfigRuntimeError, ConfigRuntimeManager, NS_AGENTS, NS_MCP_SERVERS, NS_MODELS, NS_PROVIDERS,
    NS_SKILLS, NS_TOOLS, deserialize_namespace, fingerprint_config,
};

/// Config-store namespace for model pools. Defined here (its only consumer)
/// rather than alongside the other namespace constants to avoid growing the
/// oversized `config_runtime.rs`.
const NS_MODEL_POOLS: &str = "model-pools";

pub(crate) struct ManagedConfigSnapshot {
    pub(crate) providers: Vec<ProviderSpec>,
    pub(crate) models: Vec<ModelSpec>,
    pub(crate) pools: Vec<ModelPoolSpec>,
    pub(crate) agents: Vec<AgentSpec>,
    pub(crate) mcp_servers: Vec<McpServerSpec>,
    pub(crate) tools: Vec<ToolSpec>,
    pub(crate) skills: Vec<SkillSpec>,
    pub(crate) source_config_revisions: Vec<ConfigRevisionRef>,
    pub(crate) fingerprint: u64,
}

impl ConfigRuntimeManager {
    pub(crate) async fn load_managed_config(
        &self,
    ) -> Result<ManagedConfigSnapshot, ConfigRuntimeError> {
        let provider_values = self.load_namespace_entries(NS_PROVIDERS).await?;
        let model_values = self.load_namespace_entries(NS_MODELS).await?;
        let pool_values = self.load_namespace_entries(NS_MODEL_POOLS).await?;
        let agent_values = self.load_namespace_entries(NS_AGENTS).await?;
        let mcp_values = self.load_namespace_entries(NS_MCP_SERVERS).await?;
        let tool_values = self.load_namespace_entries(NS_TOOLS).await?;
        let skill_values = self.load_namespace_entries(NS_SKILLS).await?;

        let fingerprint = fingerprint_config(&[
            (NS_PROVIDERS, &provider_values),
            (NS_MODELS, &model_values),
            (NS_MODEL_POOLS, &pool_values),
            (NS_AGENTS, &agent_values),
            (NS_MCP_SERVERS, &mcp_values),
            (NS_TOOLS, &tool_values),
            (NS_SKILLS, &skill_values),
        ])?;
        let mut source_config_revisions = Vec::new();
        source_config_revisions.extend(config_revision_refs(NS_PROVIDERS, &provider_values)?);
        source_config_revisions.extend(config_revision_refs(NS_MODELS, &model_values)?);
        source_config_revisions.extend(config_revision_refs(NS_MODEL_POOLS, &pool_values)?);
        source_config_revisions.extend(config_revision_refs(NS_AGENTS, &agent_values)?);
        source_config_revisions.extend(config_revision_refs(NS_TOOLS, &tool_values)?);
        source_config_revisions.extend(config_revision_refs(NS_SKILLS, &skill_values)?);

        let models: Vec<ModelSpec> = deserialize_namespace(&model_values)?;
        validate_unique_model_ids(&models)
            .map_err(|error| ConfigRuntimeError::InvalidConfig(error.to_string()))?;

        Ok(ManagedConfigSnapshot {
            providers: deserialize_namespace(&provider_values)?,
            models,
            pools: deserialize_namespace(&pool_values)?,
            agents: deserialize_namespace(&agent_values)?,
            mcp_servers: deserialize_namespace(&mcp_values)?,
            tools: deserialize_namespace(&tool_values)?,
            skills: deserialize_namespace(&skill_values)?,
            source_config_revisions,
            fingerprint,
        })
    }
}

fn config_revision_refs(
    namespace: &str,
    entries: &[(String, Value)],
) -> Result<Vec<ConfigRevisionRef>, ConfigRuntimeError> {
    let mut refs = Vec::new();
    for (id, value) in entries {
        let record: ConfigRecord<Value> = ConfigRecord::from_value(value.clone())
            .map_err(|error| {
                awaken_contract::contract::storage::StorageError::Serialization(error.to_string())
            })
            .map_err(ConfigRuntimeError::Storage)?;
        if record.meta.hidden {
            continue;
        }
        refs.push(ConfigRevisionRef {
            namespace: namespace.to_string(),
            id: id.clone(),
            revision: record.meta.revision,
        });
    }
    Ok(refs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_contract::contract::storage::StorageError;
    use awaken_contract::{AgentSpec, ConfigRecord, RecordMeta};
    use serde_json::json;

    use super::super::{ConfigRuntimeError, deserialize_namespace};

    fn minimal_agent_spec(id: &str) -> AgentSpec {
        AgentSpec {
            id: id.into(),
            model_id: "test-model".into(),
            system_prompt: "test prompt".into(),
            max_rounds: 1,
            ..Default::default()
        }
    }

    #[test]
    fn deserialize_namespace_decodes_legacy_bare_spec() {
        let spec = minimal_agent_spec("agent-a");
        let value = serde_json::to_value(&spec).expect("serialization must succeed");
        let entries = vec![("agent-a".to_string(), value)];
        let result: Vec<AgentSpec> =
            deserialize_namespace(&entries).expect("legacy bare spec must decode");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "agent-a");
    }

    #[test]
    fn deserialize_namespace_decodes_envelope() {
        let spec = minimal_agent_spec("agent-b");
        let record = ConfigRecord {
            spec,
            meta: RecordMeta::new_user(),
        };
        let value = record
            .to_value()
            .expect("envelope serialization must succeed");
        let entries = vec![("agent-b".to_string(), value)];
        let result: Vec<AgentSpec> = deserialize_namespace(&entries).expect("envelope must decode");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "agent-b");
    }

    #[test]
    fn deserialize_namespace_skips_hidden_envelope() {
        let visible = minimal_agent_spec("visible");
        let hidden = minimal_agent_spec("hidden");

        let mut hidden_meta = RecordMeta::new_user();
        hidden_meta.hidden = true;

        let visible_record = ConfigRecord {
            spec: visible,
            meta: RecordMeta::new_user(),
        };
        let hidden_record = ConfigRecord {
            spec: hidden,
            meta: hidden_meta,
        };

        let entries = vec![
            (
                "visible".to_string(),
                visible_record.to_value().expect("serialize visible"),
            ),
            (
                "hidden".to_string(),
                hidden_record.to_value().expect("serialize hidden"),
            ),
        ];
        let result: Vec<AgentSpec> = deserialize_namespace(&entries).expect("decode must succeed");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "visible");
    }

    #[test]
    fn deserialize_namespace_skips_hidden_before_effective_validation() {
        let mut hidden_meta = RecordMeta::new_user();
        hidden_meta.hidden = true;
        hidden_meta.user_overrides = Some(json!({ "unknown_patch_field": true }));

        let hidden_record = ConfigRecord {
            spec: json!({ "not": "an agent spec" }),
            meta: hidden_meta,
        };
        let entries = vec![(
            "hidden".to_string(),
            hidden_record.to_value().expect("serialize hidden"),
        )];

        let result: Vec<AgentSpec> =
            deserialize_namespace(&entries).expect("hidden invalid record must be skipped");
        assert!(result.is_empty());
    }

    #[test]
    fn deserialize_namespace_mixes_legacy_and_envelope() {
        let bare_spec = minimal_agent_spec("bare");
        let envelope_spec = minimal_agent_spec("envelope");

        let bare_value = serde_json::to_value(&bare_spec).expect("serialize bare");
        let envelope_record = ConfigRecord {
            spec: envelope_spec,
            meta: RecordMeta::new_user(),
        };
        let envelope_value = envelope_record.to_value().expect("serialize envelope");

        let entries = vec![
            ("bare".to_string(), bare_value),
            ("envelope".to_string(), envelope_value),
        ];
        let result: Vec<AgentSpec> =
            deserialize_namespace(&entries).expect("mixed decode must succeed");
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, "bare");
        assert_eq!(result[1].id, "envelope");
    }

    #[test]
    fn deserialize_namespace_propagates_decode_error() {
        let bad_value = json!({"completely": "wrong"});
        let entries = vec![("bad".to_string(), bad_value)];
        let err = deserialize_namespace::<AgentSpec>(&entries)
            .expect_err("invalid spec must produce an error");
        assert!(
            matches!(
                err,
                ConfigRuntimeError::Storage(StorageError::Serialization(_))
            ),
            "expected Storage(Serialization(_)), got: {err:?}"
        );
    }

    /// Direct-write bypass: HTTP `PUT /v1/config/models/{id}` enforces that
    /// the namespace key equals the payload `id`, but `ConfigStore::put` is
    /// public, so seed code, alternative store backends, or future writers
    /// can land two `NS_MODELS` entries with distinct namespace keys whose
    /// payloads carry the same `id`. `load_managed_config` must catch that
    /// regardless of namespace-key dedup at the HTTP layer.
    #[tokio::test]
    async fn load_managed_config_rejects_duplicate_model_ids_in_store() {
        let (manager, store) = super::super::tests::make_manager_with_store().await;

        let make_entry = |store_key: &str, model_id: &str| {
            let spec = awaken_contract::ModelSpec::new(model_id, "boot", "boot-model");
            let record = ConfigRecord {
                spec,
                meta: RecordMeta::new_user(),
            };
            let value = record.to_value().expect("envelope serialization");
            (store_key.to_string(), value)
        };

        // Two distinct namespace keys, identical payload `id` — the
        // bypass scenario the HTTP layer cannot see.
        let (key_a, value_a) = make_entry("store-key-a", "dup-id");
        let (key_b, value_b) = make_entry("store-key-b", "dup-id");
        store
            .put(NS_MODELS, &key_a, &value_a)
            .await
            .expect("seed first entry");
        store
            .put(NS_MODELS, &key_b, &value_b)
            .await
            .expect("seed second entry");

        let err = match manager.load_managed_config().await {
            Err(error) => error,
            Ok(_) => panic!("expected duplicate model id rejection, got Ok"),
        };
        let msg = err.to_string();
        assert!(
            matches!(err, ConfigRuntimeError::InvalidConfig(_)),
            "expected InvalidConfig, got: {err:?}"
        );
        assert!(
            msg.contains("duplicate model id"),
            "expected 'duplicate model id' in message, got: {msg}"
        );
        assert!(
            msg.contains("'dup-id'"),
            "expected duplicated id in message, got: {msg}"
        );
    }
}
