//! Field-level override for [`ToolSpec`] (ADR-0029).
//!
//! Stored as JSON inside `RecordMeta::user_overrides` for built-in tool
//! records. Only `description` is patchable today.

use serde::{Deserialize, Serialize};

use crate::tool_spec::ToolSpec;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ToolSpecPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl ToolSpecPatch {
    pub fn is_empty(&self) -> bool {
        self.description.is_none()
    }
}

/// Apply a [`ToolSpecPatch`] on top of a [`ToolSpec`]. `None` keeps the base
/// value, `Some(v)` replaces it. Read-only fields (id, name, category,
/// parameters_schema) are untouched — patch shape statically excludes them.
pub fn merge_tool_spec(base: ToolSpec, patch: ToolSpecPatch) -> ToolSpec {
    ToolSpec {
        description: patch.description.unwrap_or(base.description),
        ..base
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> ToolSpec {
        ToolSpec {
            id: "echo".into(),
            name: "Echo".into(),
            description: "stock".into(),
            ..Default::default()
        }
    }

    #[test]
    fn empty_patch_leaves_spec_unchanged() {
        let merged = merge_tool_spec(base(), ToolSpecPatch::default());
        assert_eq!(merged.description, "stock");
    }

    #[test]
    fn description_override_replaces_base() {
        let merged = merge_tool_spec(
            base(),
            ToolSpecPatch {
                description: Some("custom".into()),
            },
        );
        assert_eq!(merged.description, "custom");
    }

    #[test]
    fn unknown_field_is_rejected() {
        let bad = json!({"description": "x", "name": "renamed"});
        assert!(serde_json::from_value::<ToolSpecPatch>(bad).is_err());
    }

    #[test]
    fn null_description_decodes_as_clear() {
        // null on Option<String> deserializes as None; the service layer
        // treats null at the JSON-object level as "remove this field" but
        // the patch type itself accepts null harmlessly.
        let v = json!({"description": null});
        let patch: ToolSpecPatch = serde_json::from_value(v).unwrap();
        assert!(patch.description.is_none());
    }
}
