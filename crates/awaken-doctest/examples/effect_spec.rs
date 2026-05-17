//! `EffectSpec` + `TypedEffect::from_spec` round-trip — pins the surface
//! `reference/effects.md` cites. Previous docs drift (iter #5) was wrong
//! signature on `from_spec`; this guards against a repeat.

use awaken::model::{EffectSpec, TypedEffect};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromptOverridePayload {
    system_prompt: String,
}

struct SetPromptOverride;
impl EffectSpec for SetPromptOverride {
    const KEY: &'static str = "doctest.set_prompt_override";
    type Payload = PromptOverridePayload;
}

fn main() {
    let payload = PromptOverridePayload {
        system_prompt: "You are concise.".into(),
    };
    let effect = TypedEffect::from_spec::<SetPromptOverride>(&payload).expect("encode");
    assert_eq!(effect.key, SetPromptOverride::KEY);
    let _decoded: PromptOverridePayload = effect.decode::<SetPromptOverride>().expect("decode");
}
