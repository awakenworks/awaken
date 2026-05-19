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

    /* Use the constructor instead of a struct literal so future fields
     * (pricing, etc.) don't force every example to recompile. The
     * constructor sets all optional fields to their canonical defaults. */
    let _binding = ModelBindingSpec::new("gpt-4o-mini", "openai", "gpt-4o-mini");

    let _agent = AgentSpec {
        id: "assistant".into(),
        model_id: "gpt-4o-mini".into(),
        system_prompt: "You are a helpful assistant.".into(),
        ..Default::default()
    };
}
