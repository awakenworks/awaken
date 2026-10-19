// @formal surface requirement=runtime.plugin_activation.capability_bound
// @formal reason: One composition declaration projects metadata and pure extension-owned config
// @formal reason: admission; it owns neither resources, authorization nor Runtime execution.
//! The single platform plugin configuration declaration and discovery projection.

#![warn(unreachable_pub)]

use awaken_executable_agent_contract::capability::{
    PluginCapability, PluginConfigurationSource, PluginConfigurationValidator,
};
use awaken_ext_builtin_tools::{WebFetchPlugin, WebSearchPlugin, WebSearchProviderRegistry};
use awaken_runtime_contract::plugin::Plugin;
use serde_json::Value;

pub fn platform_plugin_capabilities() -> Vec<PluginCapability> {
    platform_plugin_capabilities_with_web_search(&WebSearchProviderRegistry::builtins())
}

pub fn platform_plugin_capabilities_with_web_search(
    providers: &WebSearchProviderRegistry,
) -> Vec<PluginCapability> {
    PluginConfigurationSource::capabilities(&platform_plugin_configurations_with_web_search(
        providers,
    ))
}

pub fn platform_plugin_configurations() -> Vec<PluginConfigurationSource> {
    platform_plugin_configurations_with_web_search(&WebSearchProviderRegistry::builtins())
}

/// Metadata and the pure decoder are paired once. Web provider/credential
/// references continue through their existing Workspace publication resolvers.
pub fn platform_plugin_configurations_with_web_search(
    providers: &WebSearchProviderRegistry,
) -> Vec<PluginConfigurationSource> {
    let search = WebSearchPlugin::new(providers.clone(), None).manifest();
    let fetch = WebFetchPlugin::new(providers.clone(), None).manifest();
    vec![
        configuration(
            PluginCapability {
                id: awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.into(),
                schema_keys: vec![awaken_ext_background_task::BACKGROUND_TASK_PLUGIN_ID.into()],
                config_schema: Some(awaken_ext_background_task::config_schema()),
                bound: awaken_ext_background_task::background_task_manifest().bound,
            },
            Some(validate_background),
        ),
        configuration(
            PluginCapability {
                id: awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.into(),
                schema_keys: vec![awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID.into()],
                config_schema: Some(awaken_ext_state_machine::config_schema()),
                bound: Default::default(),
            },
            Some(validate_state_machine),
        ),
        configuration(
            PluginCapability {
                id: awaken_ext_memory::MEMORY_PLUGIN_ID.into(),
                schema_keys: vec![awaken_ext_memory::MEMORY_PLUGIN_ID.into()],
                config_schema: Some(awaken_ext_memory::memory_config_schema()),
                bound: Default::default(),
            },
            Some(validate_memory),
        ),
        configuration(
            PluginCapability {
                id: awaken_ext_compact::COMPACT_PLUGIN_ID.into(),
                schema_keys: vec![awaken_ext_compact::COMPACT_PLUGIN_ID.into()],
                config_schema: Some(awaken_ext_compact::compact_config_schema()),
                bound: Default::default(),
            },
            Some(validate_compact),
        ),
        configuration(
            PluginCapability {
                id: search.id,
                schema_keys: search.config_sections,
                config_schema: Some(providers.config_schema()),
                bound: search.bound,
            },
            None,
        ),
        configuration(
            PluginCapability {
                id: fetch.id,
                schema_keys: fetch.config_sections,
                config_schema: Some(providers.fetch_config_schema()),
                bound: fetch.bound,
            },
            None,
        ),
    ]
}

fn configuration(
    capability: PluginCapability,
    validate: Option<PluginConfigurationValidator>,
) -> PluginConfigurationSource {
    PluginConfigurationSource {
        capability,
        validate,
    }
}

fn validate_memory(value: Option<&Value>) -> Result<(), String> {
    awaken_ext_memory::MemoryConfig::from_value(value)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn validate_compact(value: Option<&Value>) -> Result<(), String> {
    value
        .map(|value| serde_json::from_value::<awaken_ext_compact::CompactConfig>(value.clone()))
        .transpose()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn validate_state_machine(value: Option<&Value>) -> Result<(), String> {
    awaken_ext_state_machine::StateMachineConfig::from_value(
        value.cloned().unwrap_or_else(|| serde_json::json!({})),
    )
    .and_then(|config| config.into_machines())
    .map(|_| ())
    .map_err(|error| error.to_string())
}

fn validate_background(value: Option<&Value>) -> Result<(), String> {
    let config = value
        .map(|value| {
            serde_json::from_value::<awaken_ext_background_task::BackgroundTaskConfig>(
                value.clone(),
            )
        })
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or_default();
    config.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_projection_and_validation_share_every_platform_declaration() {
        // DT-CONFIG-SOURCE C1 each installed declaration -> exactly one public
        // metadata row; C2 four pure configurations -> one owned decoder each;
        // C3 Web providers -> original Workspace resolver, never a fake Runtime
        // plugin. Absent config is not materialized or rewritten by inspection.
        let sources = platform_plugin_configurations();
        let projected = PluginConfigurationSource::capabilities(&sources);
        assert_eq!(projected, platform_plugin_capabilities());
        let ids = sources
            .iter()
            .map(|source| source.capability.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(ids.len(), sources.len());
        for source in sources {
            if let Some(validate) = source.validate {
                assert!(validate(None).is_ok(), "{}", source.capability.id);
                assert!(
                    validate(Some(&Value::Null)).is_err(),
                    "{}",
                    source.capability.id
                );
            } else {
                assert!(["web_search", "web_fetch"].contains(&source.capability.id.as_str()));
            }
        }
    }

    #[test]
    fn advertised_shapes_and_owner_decoders_agree_on_closed_configuration_inputs() {
        // DT-CONFIG-WIRE: absent/partial/default/typed/nullable -> owned
        // admission; wrong root/type/unknown structural key/bounds -> refusal.
        // Every owner-admitted fixture must also satisfy its public Schema.
        // Semantic failures may satisfy Schema: the type/compiler, not JSON
        // Schema or a copied Control rule, owns those additional decisions.
        let sources = platform_plugin_configurations();
        let fixtures = [
            (
                "memory",
                serde_json::json!({"instructions":"keep facts", "max_entries":1}),
                true,
            ),
            ("memory", serde_json::json!({"max_entries":0}), false),
            (
                "memory",
                serde_json::json!({"extraction_promt":"typo"}),
                false,
            ),
            (
                "compact",
                serde_json::json!({"keep_last":0,"instructions":null}),
                true,
            ),
            ("compact", serde_json::json!({"prefetch_ratio":1.0}), false),
            ("compact", serde_json::json!({"agent_id":" "}), false),
            ("compact", serde_json::json!({"unknown":true}), false),
            ("state_machine", serde_json::json!({"machines":[]}), true),
            ("state_machine", serde_json::json!({"machine":[]}), false),
            (
                "state_machine",
                serde_json::json!({"continuation":{"unknown":1}}),
                false,
            ),
            (
                "state_machine",
                serde_json::json!({"machines":[{"name":"m","initial":"ready"},{"name":"m","initial":"ready"}]}),
                false,
            ),
            (
                "background_task",
                serde_json::json!({"tools":["Read"]}),
                true,
            ),
            ("background_task", serde_json::json!({"tools":[""]}), false),
            (
                "background_task",
                serde_json::json!({"unknown":true}),
                false,
            ),
            (
                "background_task",
                serde_json::json!({"tools":[awaken_ext_background_task::management_tool_ids()[0]]}),
                false,
            ),
        ];
        for (id, authored, accepted) in fixtures {
            let source = sources
                .iter()
                .find(|source| source.capability.id == id)
                .unwrap();
            let result = source.validate.unwrap()(Some(&authored));
            assert_eq!(result.is_ok(), accepted, "{id}: {authored}: {result:?}");
            if accepted {
                let schema = source.capability.config_schema.as_ref().unwrap();
                assert!(
                    jsonschema::validator_for(schema)
                        .unwrap()
                        .is_valid(&authored),
                    "{id}: {authored}"
                );
            }
        }
    }
    #[test]
    fn platform_capabilities_expose_the_schema() {
        // Capability cause/effect table. C1 the authoritative registry contains
        // direct and server-routed providers; C2 every provider id is unique;
        // C3 a direct provider requires credentials. E1 the projected schema
        // contains every direct provider and no duplicate branch; E2 the free
        // default remains selectable; E3 paid authoring exposes only the Vault
        // source pin and hides provider wire mechanics. The provider crate owns
        // the server-route list,
        // so this adapter test deliberately derives expectations from the same
        // registry instead of copying its current branch count.
        //
        // | Rule | registry | credential | Effect |
        // | C1 | builtins | none | E1 + E2 |
        // | C1 + C2 + C3 | builtins | required | E1 + E3 |
        let providers = WebSearchProviderRegistry::builtins();
        let caps = platform_plugin_capabilities_with_web_search(&providers);
        assert!(caps.iter().any(
            |c| c.id == awaken_ext_state_machine::STATE_MACHINE_PLUGIN_ID
                && c.config_schema.is_some()
        ));
        let web = caps
            .iter()
            .find(|capability| capability.id == awaken_ext_builtin_tools::WEB_SEARCH_PLUGIN_ID)
            .expect("WebSearch capability");
        assert_eq!(
            web.bound.tools,
            awaken_runtime_contract::plugin::IdBound::Exact(vec![
                awaken_ext_builtin_tools::WEB_SEARCH_TOOL_ID.into()
            ])
        );
        let branches = web.config_schema.as_ref().unwrap()["oneOf"]
            .as_array()
            .unwrap();
        let branch_ids = branches
            .iter()
            .map(|branch| {
                branch["properties"]["provider_id"]["const"]
                    .as_str()
                    .expect("provider branch id")
            })
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(branch_ids.len(), branches.len(), "C2/E1 unique branches");
        for descriptor in providers.descriptors() {
            assert!(branch_ids.contains(descriptor.id.as_str()), "C1/E1");
        }
        assert!(branch_ids.contains("duckduckgo"), "C1/E2 free default");
        let fetch = caps
            .iter()
            .find(|capability| capability.id == awaken_ext_builtin_tools::WEB_FETCH_PLUGIN_ID)
            .expect("WebFetch capability");
        assert_eq!(
            fetch.bound.tools,
            awaken_runtime_contract::plugin::IdBound::Exact(vec![
                awaken_ext_builtin_tools::WEB_FETCH_TOOL_ID.into()
            ])
        );
        let brave = branches
            .iter()
            .find(|branch| branch["properties"]["provider_id"]["const"] == "brave")
            .expect("Brave provider branch");
        let credential = &brave["properties"]["credential"];
        assert_eq!(
            credential["required"],
            serde_json::json!(["id", "revision"]),
            "C3/E3 authoring exposes only an exact Vault selection"
        );
        assert!(
            credential.get("x-awaken-credential-application").is_none()
                && !credential.to_string().contains("X-Subscription-Token"),
            "C3/E3 provider injection mechanics never enter the model-facing schema"
        );
    }
}
