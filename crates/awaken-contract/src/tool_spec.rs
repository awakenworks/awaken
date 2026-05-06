//! Tool registry record. The `description` field is the only field exposed
//! to the patch surface (ADR-0029); other fields are read-only snapshots
//! taken from `ToolDescriptor` at seed time.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema, Default)]
#[serde(deny_unknown_fields)]
pub struct ToolSpec {
    pub id: String,
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default)]
    pub parameters_schema: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trip_preserves_all_fields() {
        let spec = ToolSpec {
            id: "echo".into(),
            name: "Echo".into(),
            description: "Returns the input verbatim".into(),
            category: Some("debug".into()),
            parameters_schema: json!({ "type": "object", "properties": {} }),
        };
        let value = serde_json::to_value(&spec).unwrap();
        let back: ToolSpec = serde_json::from_value(value).unwrap();
        assert_eq!(spec, back);
    }

    #[test]
    fn unknown_field_is_rejected() {
        let bad = json!({
            "id": "x",
            "name": "x",
            "description": "x",
            "garbage": 1
        });
        assert!(serde_json::from_value::<ToolSpec>(bad).is_err());
    }
}
