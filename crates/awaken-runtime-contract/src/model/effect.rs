use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::error::StateError;

use super::{JsonValue, decode_json, encode_json};

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

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    struct TestPayload {
        value: String,
    }

    struct TestEffect;
    impl EffectSpec for TestEffect {
        const KEY: &'static str = "test.effect";
        type Payload = TestPayload;
    }

    #[test]
    fn typed_effect_round_trip_works() {
        let payload = TestPayload {
            value: "hello".into(),
        };
        let encoded =
            TypedEffect::from_spec::<TestEffect>(&payload).expect("encoding should succeed");
        let decoded = encoded
            .decode::<TestEffect>()
            .expect("decoding should succeed");
        assert_eq!(decoded, payload);
    }
}
