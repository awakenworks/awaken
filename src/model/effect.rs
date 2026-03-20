use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::contract::lifecycle::TerminationReason;
use crate::error::StateError;

use super::{JsonValue, decode_json, encode_json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeEffect {
    AddUserMessage { message: String },
    AddSystemReminder { message: String },
    Terminate { reason: TerminationReason },
    Suspend { reason: String },
    Block { reason: String },
    PublishJson { topic: String, payload: JsonValue },
}

impl EffectSpec for RuntimeEffect {
    const KEY: &'static str = "__runtime.runtime_effect";
    type Payload = Self;
}

pub trait EffectSpec: 'static + Send + Sync {
    const KEY: &'static str;

    type Payload: Serialize + DeserializeOwned + Send + Sync + 'static;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEffect {
    pub key: String,
    pub payload: JsonValue,
}

impl TypedEffect {
    pub fn from_spec<E: EffectSpec>(payload: &E::Payload) -> Result<Self, StateError> {
        Ok(Self {
            key: E::KEY.to_string(),
            payload: encode_json(E::KEY, payload)?,
        })
    }

    pub fn decode<E: EffectSpec>(&self) -> Result<E::Payload, StateError> {
        decode_json(E::KEY, self.payload.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectLogEntry {
    pub id: u64,
    pub key: String,
}

super::define_log_slot! {
    slot = EffectLog,
    update = EffectLogUpdate,
    entry = EffectLogEntry,
    key = "__runtime.effect_log",
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_effect_round_trip_works() {
        let effect = RuntimeEffect::Terminate {
            reason: crate::contract::lifecycle::TerminationReason::NaturalEnd,
        };

        let encoded = TypedEffect::from_spec::<RuntimeEffect>(&effect)
            .expect("runtime effect encoding should succeed");
        let decoded = encoded
            .decode::<RuntimeEffect>()
            .expect("runtime effect decoding should succeed");

        assert_eq!(decoded, effect);
    }

    #[test]
    fn terminate_effect_with_stopped_reason_roundtrip() {
        use crate::contract::lifecycle::TerminationReason;

        let effect = RuntimeEffect::Terminate {
            reason: TerminationReason::stopped_with_detail("max_turns", "reached 10"),
        };
        let encoded = TypedEffect::from_spec::<RuntimeEffect>(&effect).unwrap();
        let decoded = encoded.decode::<RuntimeEffect>().unwrap();
        assert_eq!(decoded, effect);
    }

    #[test]
    fn all_runtime_effects_roundtrip() {
        use crate::contract::lifecycle::TerminationReason;

        let effects = vec![
            RuntimeEffect::AddUserMessage {
                message: "hello".into(),
            },
            RuntimeEffect::AddSystemReminder {
                message: "remember".into(),
            },
            RuntimeEffect::Terminate {
                reason: TerminationReason::NaturalEnd,
            },
            RuntimeEffect::Terminate {
                reason: TerminationReason::Cancelled,
            },
            RuntimeEffect::Terminate {
                reason: TerminationReason::Error("oops".into()),
            },
            RuntimeEffect::Suspend {
                reason: "waiting".into(),
            },
            RuntimeEffect::Block {
                reason: "denied".into(),
            },
            RuntimeEffect::PublishJson {
                topic: "metrics".into(),
                payload: serde_json::json!({"key": "value"}),
            },
        ];

        for effect in effects {
            let encoded = TypedEffect::from_spec::<RuntimeEffect>(&effect)
                .unwrap_or_else(|e| panic!("encode {effect:?}: {e}"));
            let decoded = encoded
                .decode::<RuntimeEffect>()
                .unwrap_or_else(|e| panic!("decode {effect:?}: {e}"));
            assert_eq!(decoded, effect);
        }
    }
}
