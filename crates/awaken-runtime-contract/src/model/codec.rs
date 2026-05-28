use serde::{Serialize, de::DeserializeOwned};

use crate::error::StateError;

pub use serde_json::Value as JsonValue;

pub fn encode_json<T: Serialize>(key: &str, value: &T) -> Result<JsonValue, StateError> {
    serde_json::to_value(value).map_err(|err| StateError::KeyEncode {
        key: key.to_string(),
        message: err.to_string(),
    })
}

pub fn decode_json<T: DeserializeOwned>(key: &str, value: JsonValue) -> Result<T, StateError> {
    serde_json::from_value(value).map_err(|err| StateError::KeyDecode {
        key: key.to_string(),
        message: err.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
    struct Payload {
        value: String,
    }

    #[test]
    fn json_round_trip_works() {
        let payload = Payload { value: "ok".into() };

        let json = encode_json("payload", &payload).expect("encode should succeed");
        let decoded: Payload = decode_json("payload", json).expect("decode should succeed");

        assert_eq!(decoded, payload);
    }
}
