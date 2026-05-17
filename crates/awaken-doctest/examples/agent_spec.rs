//! Construct `AgentSpec` + `ProviderSpec` + `ModelBindingSpec` — verifies
//! the config-plane shapes docs cite in `reference/config` and
//! `reference/provider-model-config` still match the crate.

use awaken::registry_spec::{AgentSpec, ModelBindingSpec, ProviderSpec};

fn main() {
    let _provider = ProviderSpec {
        id: "openai".into(),
        adapter: "openai".into(),
        ..Default::default()
    };

    let _binding = ModelBindingSpec {
        id: "gpt-4o-mini".into(),
        provider_id: "openai".into(),
        upstream_model: "gpt-4o-mini".into(),
    };

    let _agent = AgentSpec {
        id: "assistant".into(),
        model_id: "gpt-4o-mini".into(),
        system_prompt: "You are a helpful assistant.".into(),
        ..Default::default()
    };
}
