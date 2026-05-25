use awaken_contract::{
    AgentSpec, ConfigRecord, ConfigRevisionRef, McpServerSpec, ModelBindingSpec, ProviderSpec,
    SkillSpec, ToolSpec,
};
use serde_json::Value;

use super::{
    ConfigRuntimeError, ConfigRuntimeManager, NS_AGENTS, NS_MCP_SERVERS, NS_MODELS, NS_PROVIDERS,
    NS_SKILLS, NS_TOOLS, deserialize_namespace, fingerprint_config,
};

pub(crate) struct ManagedConfigSnapshot {
    pub(crate) providers: Vec<ProviderSpec>,
    pub(crate) models: Vec<ModelBindingSpec>,
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
        let agent_values = self.load_namespace_entries(NS_AGENTS).await?;
        let mcp_values = self.load_namespace_entries(NS_MCP_SERVERS).await?;
        let tool_values = self.load_namespace_entries(NS_TOOLS).await?;
        let skill_values = self.load_namespace_entries(NS_SKILLS).await?;

        let fingerprint = fingerprint_config(&[
            (NS_PROVIDERS, &provider_values),
            (NS_MODELS, &model_values),
            (NS_AGENTS, &agent_values),
            (NS_MCP_SERVERS, &mcp_values),
            (NS_TOOLS, &tool_values),
            (NS_SKILLS, &skill_values),
        ])?;
        let mut source_config_revisions = Vec::new();
        source_config_revisions.extend(config_revision_refs(NS_PROVIDERS, &provider_values)?);
        source_config_revisions.extend(config_revision_refs(NS_MODELS, &model_values)?);
        source_config_revisions.extend(config_revision_refs(NS_AGENTS, &agent_values)?);
        source_config_revisions.extend(config_revision_refs(NS_TOOLS, &tool_values)?);
        source_config_revisions.extend(config_revision_refs(NS_SKILLS, &skill_values)?);

        Ok(ManagedConfigSnapshot {
            providers: deserialize_namespace(&provider_values)?,
            models: deserialize_namespace(&model_values)?,
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
}
