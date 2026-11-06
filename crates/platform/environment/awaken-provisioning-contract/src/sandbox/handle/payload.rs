//! Order-preserving opaque JSON for provider-owned handle payloads.
//!
//! Durable Session bindings compare the serialized handle byte-for-byte, so a
//! payload the contract cannot interpret must still re-encode with exactly the
//! member order it was decoded or constructed with. `serde_json::Value` cannot
//! promise that: its object order depends on a workspace-unified cargo feature.
//! This tree keeps member order explicitly while comparing objects by content.

use std::fmt;

use serde::de::{self, DeserializeOwned, MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::super::SandboxError;

/// One JSON value whose objects remember member order.
#[derive(Debug, Clone)]
pub(super) enum PayloadValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<PayloadValue>),
    Object(PayloadObject),
}

/// Ordered JSON object with unique member names.
#[derive(Debug, Clone, Default)]
pub(super) struct PayloadObject(Vec<(String, PayloadValue)>);

impl PayloadObject {
    /// Encode a provider-owned typed value in its own declaration order.
    pub(super) fn from_typed<T: Serialize>(value: &T) -> Result<Self, SandboxError> {
        let text = serde_json::to_string(value).map_err(|error| {
            SandboxError::invalid(format!("sandbox handle payload cannot be encoded: {error}"))
        })?;
        serde_json::from_str(&text).map_err(|error| {
            SandboxError::invalid(format!(
                "sandbox handle payload must encode as one JSON object: {error}"
            ))
        })
    }

    /// Decode the complete object into a provider-owned typed value.
    pub(super) fn to_typed<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.to_json())
    }

    pub(super) fn get(&self, name: &str) -> Option<&PayloadValue> {
        self.0
            .iter()
            .find_map(|(member, value)| (member == name).then_some(value))
    }

    pub(super) fn remove(&mut self, name: &str) -> Option<PayloadValue> {
        let index = self.0.iter().position(|(member, _)| member == name)?;
        Some(self.0.remove(index).1)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn members(&self) -> impl Iterator<Item = (&str, &PayloadValue)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }

    fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Object(
            self.0
                .iter()
                .map(|(name, value)| (name.clone(), value.to_json()))
                .collect(),
        )
    }
}

impl PayloadValue {
    pub(super) fn to_typed<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_value(self.to_json())
    }

    fn to_json(&self) -> serde_json::Value {
        match self {
            Self::Null => serde_json::Value::Null,
            Self::Bool(value) => serde_json::Value::Bool(*value),
            Self::Number(value) => serde_json::Value::Number(value.clone()),
            Self::String(value) => serde_json::Value::String(value.clone()),
            Self::Array(values) => {
                serde_json::Value::Array(values.iter().map(Self::to_json).collect())
            }
            Self::Object(object) => object.to_json(),
        }
    }
}

impl PartialEq for PayloadObject {
    /// Content equality: member order is encoding evidence, not identity.
    fn eq(&self, other: &Self) -> bool {
        self.0.len() == other.0.len()
            && self
                .0
                .iter()
                .all(|(name, value)| other.get(name) == Some(value))
    }
}

impl Eq for PayloadObject {}

impl PartialEq for PayloadValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Number(left), Self::Number(right)) => left == right,
            (Self::String(left), Self::String(right)) => left == right,
            (Self::Array(left), Self::Array(right)) => left == right,
            (Self::Object(left), Self::Object(right)) => left == right,
            _ => false,
        }
    }
}

impl Eq for PayloadValue {}

impl Serialize for PayloadObject {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

impl Serialize for PayloadValue {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut seq = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    seq.serialize_element(value)?;
                }
                seq.end()
            }
            Self::Object(object) => object.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for PayloadObject {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer
            .deserialize_map(PayloadVisitor)
            .and_then(|value| match value {
                PayloadValue::Object(object) => Ok(object),
                _ => Err(de::Error::custom(
                    "sandbox handle payload must be an object",
                )),
            })
    }
}

impl<'de> Deserialize<'de> for PayloadValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(PayloadVisitor)
    }
}

struct PayloadVisitor;

impl<'de> Visitor<'de> for PayloadVisitor {
    type Value = PayloadValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value")
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(PayloadValue::Null)
    }

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        Ok(PayloadValue::Null)
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        PayloadValue::deserialize(deserializer)
    }

    fn visit_bool<E: de::Error>(self, value: bool) -> Result<Self::Value, E> {
        Ok(PayloadValue::Bool(value))
    }

    fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
        Ok(PayloadValue::Number(value.into()))
    }

    fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
        Ok(PayloadValue::Number(value.into()))
    }

    fn visit_f64<E: de::Error>(self, value: f64) -> Result<Self::Value, E> {
        serde_json::Number::from_f64(value)
            .map(PayloadValue::Number)
            .ok_or_else(|| E::custom("sandbox handle payload number is not finite"))
    }

    fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
        Ok(PayloadValue::String(value.to_owned()))
    }

    fn visit_string<E: de::Error>(self, value: String) -> Result<Self::Value, E> {
        Ok(PayloadValue::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
        let mut values = Vec::with_capacity(seq.size_hint().unwrap_or(0).min(64));
        while let Some(value) = seq.next_element()? {
            values.push(value);
        }
        Ok(PayloadValue::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        let mut members: Vec<(String, PayloadValue)> = Vec::new();
        while let Some(name) = map.next_key::<String>()? {
            if members.iter().any(|(member, _)| member == &name) {
                return Err(de::Error::custom(format_args!(
                    "sandbox handle payload has a duplicate member `{name}`"
                )));
            }
            let value = map.next_value()?;
            members.push((name, value));
        }
        Ok(PayloadValue::Object(PayloadObject(members)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn member_order_is_preserved_while_equality_is_by_content() {
        // Cause/effect table. C1 decode text whose nested members are not in
        // lexical order; C2 the same content in another order; C3 different
        // content; C4 a duplicate member. E1 re-encoding reproduces C1 bytes;
        // E2 C1 == C2; E3 C1 != C3; E4 C4 is rejected.
        let text = r#"{"z":1,"a":{"y":[true,null,"s"],"b":-2},"m":1.5}"#;
        let decoded: PayloadObject = serde_json::from_str(text).unwrap();
        assert_eq!(serde_json::to_string(&decoded).unwrap(), text, "E1");
        let reordered: PayloadObject =
            serde_json::from_str(r#"{"a":{"b":-2,"y":[true,null,"s"]},"m":1.5,"z":1}"#).unwrap();
        assert_eq!(decoded, reordered, "E2");
        let different: PayloadObject =
            serde_json::from_str(r#"{"z":1,"a":{"y":[null,true,"s"],"b":-2},"m":1.5}"#).unwrap();
        assert_ne!(decoded, different, "E3 array order is content");
        assert!(
            serde_json::from_str::<PayloadObject>(r#"{"a":1,"a":2}"#).is_err(),
            "E4"
        );
        assert!(
            serde_json::from_str::<PayloadObject>("[1]").is_err(),
            "object only"
        );
    }

    #[test]
    fn typed_payloads_keep_declaration_order_and_round_trip() {
        #[derive(Debug, PartialEq, Serialize, Deserialize)]
        struct Typed {
            zeta: u64,
            alpha: Option<String>,
        }
        let typed = Typed {
            zeta: u64::MAX,
            alpha: Some("a".into()),
        };
        let object = PayloadObject::from_typed(&typed).unwrap();
        assert_eq!(
            serde_json::to_string(&object).unwrap(),
            serde_json::to_string(&typed).unwrap()
        );
        assert_eq!(object.to_typed::<Typed>().unwrap(), typed);
        assert!(
            PayloadObject::from_typed(&7_u8).is_err(),
            "non-object payload"
        );
    }
}
